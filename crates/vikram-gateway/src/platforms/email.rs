//! Email adapter — SMTP submission and IMAP polling over injected line sockets.
//!
//! Outbound is a real SMTP dialogue over a [`LineSocket`]: greeting → `EHLO` → optional
//! `AUTH LOGIN` → `MAIL FROM` → `RCPT TO` → `DATA` → dot-stuffed body → `.`. Inbound is IMAP4rev1
//! polling: `LOGIN`, `SELECT`, `UID SEARCH UNSEEN`, one `UID FETCH (BODY.PEEK[])` per hit, and a
//! best-effort `UID STORE … +FLAGS (\Seen)` once the batch is parsed.
//!
//! The host owns both sockets and therefore TLS: an `AUTH LOGIN` password must only ever ride a
//! TLS-terminated stream, so `smtps://` / `imaps://` (or `STARTTLS`) is the host's terminator's
//! job. This file contains no TLS code, no DNS, no listener — it only ever sees lines.
//!
//! Fidelity notes and simplifications:
//!
//! - **FETCH literals.** The literal after a `* N FETCH (UID n BODY[] {len}` line is consumed as
//!   the *single* line the transport hands over next. The scripted test transport delivers a whole
//!   multi-line body as one line, which is what the parser assumes; a real IMAP transport must be
//!   byte-aware (read exactly `{len}` octets) and hand that over as one line.
//! - **Outbound headers.** A text message carries only `Subject: vikram` and
//!   `Content-Type: text/plain; charset=utf-8`, per the hermes wire shape. No `From:`/`To:`,
//!   `Date:`, `Message-ID:` or `MIME-Version:` headers are invented here; the SMTP envelope
//!   (`MAIL FROM` / `RCPT TO`) already addresses the message. A media message additionally needs
//!   the MIME envelope (`MIME-Version: 1.0`, `Content-Type: multipart/mixed; boundary=…`) and,
//!   when `reply_to` names a parent, the `In-Reply-To` / `References` pair the reference emits in
//!   `_new_reply` (adapter.py:870). `send`'s wire shape stays exactly as it was: only the media
//!   path threads.
//! - **Outbound media.** [`send_media`](PlatformAdapter::send_media) submits one `multipart/mixed`
//!   message per call: a `text/plain` part carrying the caption (or a line naming the attachment
//!   when the caption is blank) followed by one base64 attachment part — the shape `_new_reply`
//!   and `_attach_file` produce (adapter.py:862-877, 440-447). Attachments must arrive as
//!   [`MediaSource::Bytes`]; a URL is refused truthfully because this adapter owns no fetcher.
//! - **Message ids.** SMTP assigns no id to a plain `250`, so `send` returns a locally generated
//!   `vikram-{counter}` (`vikram-1`, `vikram-2`, …) and never claims a server-assigned one.
//!   `send_media` reports the queue id a `250` after `DATA` actually names (`queued as …`,
//!   `id=…`) because that id is real, and falls back to the local counter when it names none.
//! - **Inbound shape.** One event per unseen message: `text` is `Subject` + a blank line + body,
//!   trimmed; the conversation is always a `dm` with the sender's address. Messages without a
//!   `From:` are skipped (a reply could not be addressed) but still marked `\Seen`.
//! - **IMAP status words.** SMTP replies are numeric (`250`, `354`); IMAP answers with a tag and a
//!   status word (`a1 OK`). Both parsers live here because they are different grammars.
//!
//! Deliberately not ported: STARTTLS / implicit TLS, inbound attachment extraction, HTML bodies,
//! threading on the text path, base64 / quoted-printable *decoding*, SMTP extensions beyond
//! `AUTH LOGIN` (PIPELINING, SIZE, 8BITMIME), IMAP `IDLE` (polling is the ingress), and
//! UIDVALIDITY bookkeeping.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::event::{MessageEvent, Platform, SessionSource, platform_names};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::socket::SharedLineSocket;

/// SMTP "service ready" greeting.
const SMTP_READY: u16 = 220;
/// SMTP "requested action completed".
const SMTP_OK: u16 = 250;
/// SMTP "authentication challenge / credentials accepted so far".
const SMTP_AUTH_CHALLENGE: u16 = 334;
/// SMTP "authentication successful".
const SMTP_AUTH_OK: u16 = 235;
/// SMTP "start mail input; end with <CRLF>.<CRLF>".
const SMTP_START_INPUT: u16 = 354;
/// SMTP "user not local; will forward" — an accepted recipient.
const SMTP_RCPT_FORWARD: u16 = 251;

/// Characters of a server reply repeated in an error message.
const REPLY_PREVIEW_CHARS: usize = 200;

/// Longest line a base64 body part may carry, excluding the CRLF (RFC 2045 §2.7).
const BASE64_LINE_CHARS: usize = 76;

/// Longest filename kept from a caller value, so one absurd name cannot bloat a part header.
const MAX_FILENAME_CHARS: usize = 200;

/// Longest MIME type kept from a caller value.
const MAX_MIME_CHARS: usize = 100;

/// SMTP + IMAP email adapter.
///
/// Holds no conversation state: an e-mail chat id *is* the peer's address, and every reply leaves
/// from the one configured `from` address through the same SMTP session.
pub struct EmailAdapter {
    smtp: SharedLineSocket,
    imap: Option<SharedLineSocket>,
    from: String,
    mailbox: String,
    username: Option<String>,
    password: Option<String>,
    /// Source of local outbound ids, `vikram-{n}`.
    counter: AtomicU64,
    /// `LOGIN`/`SELECT` already ran; later polls reuse the same IMAP session.
    imap_ready: AtomicBool,
}

impl EmailAdapter {
    /// Build an adapter over an SMTP socket and an optional IMAP socket (`None` = send-only).
    ///
    /// `from` is the SMTP envelope sender, `mailbox` the IMAP mailbox to select (`"INBOX"`).
    pub fn new(
        smtp: SharedLineSocket,
        imap: Option<SharedLineSocket>,
        from: impl Into<String>,
        mailbox: impl Into<String>,
    ) -> Self {
        Self {
            smtp,
            imap,
            from: from.into(),
            mailbox: mailbox.into(),
            username: None,
            password: None,
            counter: AtomicU64::new(0),
            imap_ready: AtomicBool::new(false),
        }
    }

    /// Credentials for both protocols: base64 in `AUTH LOGIN`, quoted strings in IMAP `LOGIN`.
    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    /// The SMTP envelope sender.
    pub fn from_address(&self) -> &str {
        &self.from
    }

    /// The IMAP mailbox [`poll_once`](Self::poll_once) selects.
    pub fn mailbox(&self) -> &str {
        &self.mailbox
    }

    /// Poll the inbox once: at most one event per unseen message, in `SEARCH` order.
    ///
    /// The first poll logs in and selects the mailbox; later polls only search, so a long-lived
    /// adapter keeps one IMAP session. Returns `Ok([])` when no IMAP socket was configured.
    ///
    /// Every failure — a closed socket, a `NO`/`BAD` status, a `FETCH` that carried no `BODY[]`
    /// literal — is an `Err(String)`. A message the parser cannot address (no `From:`) is not an
    /// error: it is skipped, but still flagged `\Seen` so the next poll does not re-read it.
    pub async fn poll_once(&self) -> Result<Vec<MessageEvent>, String> {
        let Some(imap) = self.imap.as_ref() else {
            return Ok(Vec::new());
        };
        if !self.imap_ready.load(Ordering::SeqCst) {
            self.imap_login(imap).await?;
            self.imap_ready.store(true, Ordering::SeqCst);
        }

        let search = imap_expect_ok(imap, "a3", "UID SEARCH UNSEEN", "UID SEARCH UNSEEN").await?;
        let uids = parse_search_uids(&search.lines);
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let mut events = Vec::with_capacity(uids.len());
        for uid in &uids {
            let literal = imap_fetch(imap, "a4", *uid).await?;
            if let Some(event) = parse_imap_message(*uid, &literal) {
                events.push(event);
            }
        }

        for uid in &uids {
            // Best effort: a refused STORE must not discard the events already parsed.
            let command = format!("UID STORE {uid} +FLAGS (\\Seen)");
            let _ = imap_expect_ok(imap, "a5", "UID STORE", &command).await;
        }
        Ok(events)
    }

    /// The opening SMTP session: greeting, `EHLO`, and `AUTH LOGIN` when credentials are set.
    async fn smtp_handshake(&self) -> Result<(), String> {
        let greeting = read_reply(&self.smtp).await?;
        if greeting.code != SMTP_READY {
            return Err(format!(
                "smtp server greeted with {} instead of {SMTP_READY}",
                greeting.code
            ));
        }
        self.expect_reply("EHLO vikram", "EHLO vikram", &[SMTP_OK])
            .await?;
        let (Some(username), Some(password)) = (self.username.as_deref(), self.password.as_deref())
        else {
            return Ok(());
        };
        self.expect_reply("AUTH LOGIN", "AUTH LOGIN", &[SMTP_AUTH_CHALLENGE])
            .await?;
        self.expect_reply(
            "AUTH LOGIN username",
            &base64_encode(username.as_bytes()),
            &[SMTP_AUTH_CHALLENGE],
        )
        .await?;
        // A wrong password is reported in the reply, never in the step name we return.
        self.expect_reply(
            "AUTH LOGIN password",
            &base64_encode(password.as_bytes()),
            &[SMTP_AUTH_OK],
        )
        .await
        .map(|_| ())
    }

    /// Send one command line and require one of `expected`, with the server's text on failure.
    ///
    /// `label` names the step in error messages and must never carry credentials: the two
    /// `AUTH LOGIN` lines pass their base64 value as `command` but a plain step name as `label`.
    async fn expect_reply(
        &self,
        label: &str,
        command: &str,
        expected: &[u16],
    ) -> Result<Reply, String> {
        if let Err(err) = self.smtp.send_line(command).await {
            return Err(format!("smtp transport error while sending {label}: {err}"));
        }
        let reply = read_reply(&self.smtp).await?;
        if expected.contains(&reply.code) {
            return Ok(reply);
        }
        Err(refusal_message(label, &reply))
    }

    /// Send one command line and classify its reply for [`send`](PlatformAdapter::send).
    ///
    /// `None` = accepted; `Some(failure)` = a ready-to-return classified `SendResult`. The label
    /// is the command itself: no credential ever travels through this path.
    async fn smtp_expect(&self, command: &str, expected: &[u16]) -> Option<SendResult> {
        if let Err(err) = self.smtp.send_line(command).await {
            return Some(transient_failure(command, &err.to_string()));
        }
        match read_reply(&self.smtp).await {
            Ok(reply) if expected.contains(&reply.code) => None,
            Ok(reply) => Some(refusal(command, &reply)),
            Err(err) => Some(transient_failure(command, &err)),
        }
    }

    /// Submit one message body over the already-open SMTP session.
    ///
    /// Runs `MAIL FROM`, `RCPT TO`, `DATA`, writes `lines` (header block, blank line, dot-stuffed
    /// body, `.`) and returns the reply that accepted the message, or the classified
    /// [`SendResult`] that refused it. Nothing is written when the sender or the recipient is
    /// refused: the sequence stops at the first refusal exactly as RFC 5321 requires. The failure
    /// is boxed so the success path never pays for [`SendResult`]'s size (clippy
    /// `result_large_err`).
    async fn submit(&self, recipient: &str, lines: &[String]) -> Result<Reply, Box<SendResult>> {
        let mail_from = format!("MAIL FROM:<{}>", self.from);
        if let Some(failure) = self.smtp_expect(&mail_from, &[SMTP_OK]).await {
            return Err(Box::new(failure));
        }
        let rcpt_to = format!("RCPT TO:<{recipient}>");
        if let Some(failure) = self
            .smtp_expect(&rcpt_to, &[SMTP_OK, SMTP_RCPT_FORWARD])
            .await
        {
            return Err(Box::new(failure));
        }
        if let Some(failure) = self.smtp_expect("DATA", &[SMTP_START_INPUT]).await {
            return Err(Box::new(failure));
        }

        for line in lines {
            if let Err(err) = self.smtp.send_line(line).await {
                return Err(Box::new(transient_failure(
                    "message body",
                    &err.to_string(),
                )));
            }
        }
        match read_reply(&self.smtp).await {
            Ok(reply) if reply.code == SMTP_OK => Ok(reply),
            Ok(reply) => Err(Box::new(refusal("message body", &reply))),
            Err(err) => Err(Box::new(transient_failure("message body", &err))),
        }
    }

    /// Log in and select the mailbox, once per session.
    async fn imap_login(&self, imap: &SharedLineSocket) -> Result<(), String> {
        let (Some(username), Some(password)) = (self.username.as_deref(), self.password.as_deref())
        else {
            return Err(
                "imap polling needs credentials; build the adapter with with_credentials"
                    .to_string(),
            );
        };
        let command = format!(
            "LOGIN {} {}",
            quote_string(username),
            quote_string(password)
        );
        imap_expect_ok(imap, "a1", "LOGIN", &command).await?;
        let command = format!("SELECT {}", quote_string(&self.mailbox));
        imap_expect_ok(imap, "a2", "SELECT", &command).await?;
        Ok(())
    }

    /// Next `vikram-{n}` id. SMTP reports none, so local uniqueness is the best available claim.
    fn next_message_id(&self) -> String {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("vikram-{counter}")
    }
}

#[async_trait]
impl PlatformAdapter for EmailAdapter {
    /// The SMTP session opening; `false` on any unexpected status or transport failure.
    ///
    /// `is_reconnect` changes nothing: the adapter keeps no server-side queue, so a reconnect is
    /// the same handshake as a first connect.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        self.smtp_handshake().await.is_ok()
    }

    /// Best effort: close the SMTP session politely and drop the cached IMAP login.
    ///
    /// The sockets themselves belong to the host, which closes them.
    async fn disconnect(&mut self) {
        let _ = self.smtp.send_line("QUIT").await;
        self.imap_ready.store(false, Ordering::SeqCst);
    }

    /// Submit `content` as one plain-text message: `MAIL FROM`, `RCPT TO`, `DATA`, body, `.`.
    ///
    /// Blank content and a blank recipient are rejected as `bad_format` without a single byte on
    /// the wire. A `5xx` refusal is `bad_format` (permanent), a `4xx` refusal or a transport error
    /// is `transient` and retryable. `reply_to` and `metadata` are unused: SMTP submission has no
    /// threading parameter in this port, and the text wire shape is pinned as-is.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            return SendResult::failed("email send skipped: content is empty", Some("bad_format"));
        }
        let recipient = chat_id.trim();
        if recipient.is_empty() {
            return SendResult::failed(
                "email send skipped: no recipient address",
                Some("bad_format"),
            );
        }

        match self.submit(recipient, &message_lines(content)).await {
            Ok(_) => SendResult::ok(self.next_message_id()),
            Err(failure) => *failure,
        }
    }

    /// `{name: chat_id, chat_type: "dm"}` — an address is its own display name and every
    /// conversation is a DM. No traffic, so this cannot fail.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: "dm".to_string(),
        }
    }

    /// Attachments are implemented: one `multipart/mixed` message per call.
    fn supports_media(&self) -> bool {
        true
    }

    /// Submit one attachment as a `multipart/mixed` MIME message.
    ///
    /// The message mirrors the reference's shape (`_new_reply` + `_attach_file`, adapter.py:862-877
    /// and 440-447): `MIME-Version: 1.0`, `Content-Type: multipart/mixed; boundary=…`, a
    /// `text/plain; charset=utf-8` part whose body is the caption — or, when the caption is blank,
    /// a line naming the attachment, because hermes always attaches a text part to a media mail
    /// (`attach_empty_body`, adapter.py:875) and an empty body would render as a blank mail — then
    /// one part per call carrying `Content-Type` (caller value, else the per-kind default),
    /// `Content-Transfer-Encoding: base64` and `Content-Disposition: attachment; filename=…`, with
    /// the payload wrapped at [`BASE64_LINE_CHARS`] (RFC 2045 §2.7). `reply_to`, when it names a
    /// parent, adds the reference's `In-Reply-To` / `References` pair (adapter.py:870).
    ///
    /// Bytes only: [`MediaSource::Url`] is a typed refusal with zero bytes on the wire, because
    /// hermes downloads URL media before attaching it (`send_multiple_images` links URL images in
    /// the body instead, adapter.py:918-941) and this adapter has no fetcher. Acceptance follows
    /// SMTP: only a `250` after `DATA` is success, reported with the queue id that reply names when
    /// it names one; `5xx` is `bad_format`, `4xx` is `transient`, socket errors are `transient`.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let bytes = match &media.source {
            MediaSource::Bytes(bytes) => bytes,
            MediaSource::Url(url) => {
                return SendResult::failed(
                    format!(
                        "email media send needs bytes: refusing MediaSource::Url({url}) because \
                         this adapter has no fetcher — the reference downloads URL media before \
                         attaching it (adapter.py:918-941) — pass MediaSource::Bytes"
                    ),
                    Some("bad_format"),
                );
            }
        };
        if bytes.is_empty() {
            return SendResult::failed(
                "email media send skipped: attachment bytes are empty",
                Some("bad_format"),
            );
        }
        let recipient = chat_id.trim();
        if recipient.is_empty() {
            return SendResult::failed(
                "email media send skipped: no recipient address",
                Some("bad_format"),
            );
        }

        let (default_filename, default_mime) = default_media_attachment(media.kind);
        let filename = attachment_filename(media.filename.as_deref(), default_filename);
        let mime = attachment_mime(media.mime.as_deref(), default_mime);
        let body = media_body(media, &filename, &mime);
        let boundary = multipart_boundary(&body, bytes);
        let lines = multipart_message_lines(&boundary, reply_to, &body, &filename, &mime, bytes);

        match self.submit(recipient, &lines).await {
            Ok(reply) => {
                let queue_id = queue_id_from_reply(&reply.text);
                let message_id = match queue_id.clone() {
                    Some(queue_id) => queue_id,
                    None => self.next_message_id(),
                };
                let mut result = SendResult::ok(message_id);
                // The server's own wording stays visible: a caller can see which id is a queue id
                // and which is the local fallback.
                result.raw_response = Some(serde_json::json!({
                    "smtp_reply": reply.text,
                    "queue_id": queue_id,
                }));
                result
            }
            Err(failure) => *failure,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// SMTP wire helpers
// ---------------------------------------------------------------------------------------------

/// One parsed SMTP reply: the status code and the text of every line, folded with `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    code: u16,
    text: String,
}

/// Read one complete SMTP reply, folding `NNN-` continuations until the terminating `NNN ` line.
///
/// The code of the first line wins (RFC 5321 requires every line to repeat it). A stream that
/// ends before the terminator, or a line that is not a status line, is an `Err`.
async fn read_reply(socket: &SharedLineSocket) -> Result<Reply, String> {
    let mut code: Option<u16> = None;
    let mut texts: Vec<String> = Vec::new();
    loop {
        let Some(line) = socket
            .next_line()
            .await
            .map_err(|err| format!("smtp transport error while reading a reply: {err}"))?
        else {
            return Err("smtp connection closed while waiting for a reply".to_string());
        };
        let (line_code, text, more) = parse_reply_line(&line)?;
        code.get_or_insert(line_code);
        if !text.is_empty() {
            texts.push(text);
        }
        if !more {
            return Ok(Reply {
                code: code.unwrap_or(line_code),
                text: texts.join("\n"),
            });
        }
    }
}

/// Split one SMTP reply line into `(code, text, more_lines_follow)`.
fn parse_reply_line(line: &str) -> Result<(u16, String, bool), String> {
    let mut chars = line.chars();
    let mut digits = String::with_capacity(3);
    for _ in 0..3 {
        match chars.next() {
            Some(digit) if digit.is_ascii_digit() => digits.push(digit),
            _ => {
                return Err(format!(
                    "smtp reply line is not a status line: {}",
                    shortened(line, REPLY_PREVIEW_CHARS)
                ));
            }
        }
    }
    let code = digits
        .parse::<u16>()
        .map_err(|_| format!("smtp reply code is not a number: {digits}"))?;
    let (more, text) = match chars.next() {
        Some('-') => (true, chars.as_str()),
        Some(' ') => (false, chars.as_str()),
        // A bare code, or an unexpected separator: treat it as the last line.
        _ => (false, chars.as_str()),
    };
    Ok((code, text.to_string(), more))
}

/// Human-readable form of a refusal, used where a bare `Result` err is enough.
fn refusal_message(command: &str, reply: &Reply) -> String {
    let detail = shortened(&reply.text, REPLY_PREVIEW_CHARS);
    if detail.is_empty() {
        format!("smtp {command} was refused with status {}", reply.code)
    } else {
        format!(
            "smtp {command} was refused with status {}: {detail}",
            reply.code
        )
    }
}

/// Classify a refusal: `5xx` is permanent (`bad_format`), `4xx` is temporary and retryable.
fn refusal(command: &str, reply: &Reply) -> SendResult {
    let message = refusal_message(command, reply);
    match reply.code {
        500..=599 => SendResult::failed(message, Some("bad_format")),
        400..=499 => SendResult::failed(message, Some("transient")).retryable(None),
        _ => SendResult::failed(message, Some("unknown")),
    }
}

/// A transport-level failure: always retryable.
fn transient_failure(command: &str, detail: &str) -> SendResult {
    SendResult::failed(
        format!("smtp transport error during {command}: {detail}"),
        Some("transient"),
    )
    .retryable(None)
}

/// The DATA payload of a plain-text message: two headers, the blank separator, the dot-stuffed
/// content, `.`.
fn message_lines(content: &str) -> Vec<String> {
    let headers = [
        "Subject: vikram".to_string(),
        "Content-Type: text/plain; charset=utf-8".to_string(),
    ];
    let body: Vec<String> = content.lines().map(str::to_string).collect();
    envelope_lines(&headers, &body)
}

/// A DATA payload: header lines, the blank separator, the dot-stuffed body lines, `.`.
///
/// A body line that starts with `.` gets one more `.` (RFC 5321 §4.5.2) so it cannot be mistaken
/// for the end-of-data terminator. The rule is applied to every line of the body, which is what
/// makes it safe for MIME payloads too: a caption line is stuffed exactly like a plain-text `send`.
fn envelope_lines(headers: &[String], body: &[String]) -> Vec<String> {
    let mut lines = Vec::with_capacity(headers.len() + body.len() + 2);
    lines.extend(headers.iter().cloned());
    lines.push(String::new());
    for line in body {
        if line.starts_with('.') {
            lines.push(format!(".{line}"));
        } else {
            lines.push(line.clone());
        }
    }
    lines.push(".".to_string());
    lines
}

// ---------------------------------------------------------------------------------------------
// Outbound MIME helpers
// ---------------------------------------------------------------------------------------------

/// The `(filename, mime)` an attachment gets when the caller supplied neither.
///
/// Deliberately the same table the Discord uploader uses, so one `OutboundMedia` reads the same on
/// every adapter that can attach it.
fn default_media_attachment(kind: MediaKind) -> (&'static str, &'static str) {
    match kind {
        MediaKind::Image => ("image.png", "image/png"),
        MediaKind::Video => ("video.mp4", "video/mp4"),
        MediaKind::Audio => ("audio.mp3", "audio/mpeg"),
        MediaKind::Document => ("attachment.bin", "application/octet-stream"),
    }
}

/// The filename a `Content-Disposition` parameter may carry.
///
/// The last path component of `raw` (an absolute path is not a filename), with quotes,
/// backslashes and control characters dropped — a raw CR/LF would end the header line and let a
/// caller inject headers — cut to [`MAX_FILENAME_CHARS`]. An absent or unusable name becomes
/// `default`.
fn attachment_filename(raw: Option<&str>, default: &'static str) -> String {
    let basename = raw
        .and_then(|value| value.rsplit(['/', '\\']).next())
        .unwrap_or_default();
    let cleaned: String = basename
        .chars()
        .filter(|ch| !ch.is_control() && !matches!(ch, '"' | '\\'))
        .take(MAX_FILENAME_CHARS)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        default.to_string()
    } else {
        cleaned.to_string()
    }
}

/// The MIME type an attachment part may carry: `raw` when it is a plausible `type/subtype` of
/// token characters (no whitespace, no parameters — a `; charset=…` would break the header), else
/// `default`.
fn attachment_mime(raw: Option<&str>, default: &'static str) -> String {
    raw.map(str::trim)
        .filter(|value| {
            value.len() <= MAX_MIME_CHARS
                && value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '+' | '.'))
        })
        .filter(|value| {
            value
                .split_once('/')
                .is_some_and(|(kind, subtype)| !kind.is_empty() && !subtype.is_empty())
        })
        .unwrap_or(default)
        .to_string()
}

/// The `text/plain` body of a media message: the caption when it carries words, else a line
/// naming what was attached.
///
/// The fallback keeps the mandatory text part truthful instead of shipping a blank body: hermes
/// attaches a text part to every media mail (`attach_empty_body`, adapter.py:875), so the message
/// still says what arrived.
fn media_body(media: &OutboundMedia, filename: &str, mime: &str) -> String {
    match media
        .caption
        .as_deref()
        .map(str::trim)
        .filter(|caption| !caption.is_empty())
    {
        Some(caption) => caption.to_string(),
        None => format!("Attached file: {filename} ({mime})"),
    }
}

/// `reply_to` as a message-id header value, or `None` when it names nothing.
///
/// Whitespace — CR and LF above all, since either would end the header line and let a caller
/// inject headers — is dropped rather than kept: a message id is one token, so anything that would
/// split or terminate the header cannot be part of it. A bare id gains the angle brackets RFC 5322
/// requires; `<m1@example.com>` stays as it is.
fn thread_message_id(reply_to: &str) -> Option<String> {
    let cleaned: String = reply_to.chars().filter(|ch| !ch.is_whitespace()).collect();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.len() > 2 && cleaned.starts_with('<') && cleaned.ends_with('>') {
        Some(cleaned)
    } else {
        Some(format!("<{cleaned}>"))
    }
}

/// A fresh `multipart/mixed` boundary for one message.
///
/// The delimiter must not occur inside any part, so it mixes the clock with a process counter
/// rather than being a constant, and is regenerated while it still collides with the caption or
/// the attachment's base64 (base64 output can never contain `-`, so only the caption can collide;
/// nine successive 47-character collisions would need a caption holding all nine tokens). The
/// token is 47 characters, inside RFC 2046's 70-character limit.
fn multipart_boundary(text_body: &str, data: &[u8]) -> String {
    let encoded = base64_encode(data);
    for _ in 0..9 {
        let candidate = fresh_boundary();
        if !text_body.contains(&candidate) && !encoded.contains(&candidate) {
            return candidate;
        }
    }
    fresh_boundary()
}

/// One boundary candidate: `----VIKRAMMediaBoundary{nanos}{counter}`, always the same length.
///
/// The clock read cannot fail a send: an unavailable or pre-epoch system clock contributes a zero
/// nonce and the process-wide counter still makes the token unique.
fn fresh_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "----VIKRAMMediaBoundary{nanos:016x}{:08x}",
        seq & 0xffff_ffff
    )
}

/// `data` as base64 lines of at most [`BASE64_LINE_CHARS`] characters (RFC 2045 §2.7).
fn base64_lines(data: &[u8]) -> Vec<String> {
    let encoded = base64_encode(data);
    encoded
        .as_bytes()
        .chunks(BASE64_LINE_CHARS)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect()
}

/// The DATA payload lines of a one-attachment `multipart/mixed` message.
///
/// Shape: a `text/plain` part carrying `text_body`, then one base64 attachment part, closed by the
/// `--boundary--` delimiter. `MIME-Version` appears once, at the top, as RFC 2045 requires; the
/// part headers are the reference's (`_attach_file`, adapter.py:440-447) minus the redundant
/// per-part `MIME-Version`.
fn multipart_message_lines(
    boundary: &str,
    reply_to: Option<&str>,
    text_body: &str,
    filename: &str,
    mime: &str,
    data: &[u8],
) -> Vec<String> {
    let mut headers = vec![
        "Subject: vikram".to_string(),
        "MIME-Version: 1.0".to_string(),
        format!("Content-Type: multipart/mixed; boundary=\"{boundary}\""),
    ];
    if let Some(message_id) = reply_to.and_then(thread_message_id) {
        headers.push(format!("In-Reply-To: {message_id}"));
        headers.push(format!("References: {message_id}"));
    }

    let mut body: Vec<String> = Vec::with_capacity(text_body.lines().count() + data.len() / 57 + 9);
    // Part 1: the caption/body, as raw UTF-8 text (this port has no quoted-printable).
    body.push(format!("--{boundary}"));
    body.push("Content-Type: text/plain; charset=utf-8".to_string());
    body.push("Content-Transfer-Encoding: 8bit".to_string());
    body.push(String::new());
    body.extend(text_body.lines().map(str::to_string));
    // Part 2: the attachment itself.
    body.push(format!("--{boundary}"));
    body.push(format!("Content-Type: {mime}"));
    body.push("Content-Transfer-Encoding: base64".to_string());
    body.push(format!(
        "Content-Disposition: attachment; filename=\"{filename}\""
    ));
    body.push(String::new());
    body.extend(base64_lines(data));
    body.push(format!("--{boundary}--"));

    envelope_lines(&headers, &body)
}

/// The queue id a `250` reply names, else `None`.
///
/// Postfix (`250 2.0.0 Ok: queued as 4Wxyz123`) and Exim (`250 OK id=1abcd-000`) are the two
/// spellings MTAs use. The value is taken from the accepted reply only: an id is never invented,
/// so a reply that names none leaves the caller with the local `vikram-{n}` fallback.
fn queue_id_from_reply(text: &str) -> Option<String> {
    let lowered = text.to_ascii_lowercase();
    for marker in ["queued as ", "id="] {
        let Some(start) = lowered.find(marker) else {
            continue;
        };
        let Some(rest) = text.get(start + marker.len()..) else {
            continue;
        };
        let Some(token) = rest.split_whitespace().next() else {
            continue;
        };
        let token = token.trim_matches(|ch: char| matches!(ch, ',' | ';' | '.' | ')' | ']' | '"'));
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// IMAP wire helpers
// ---------------------------------------------------------------------------------------------

/// A tagged IMAP reply: every line read while waiting (untagged ones included), the status word
/// (`OK` / `NO` / `BAD`, uppercased) and the text after it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImapReply {
    lines: Vec<String>,
    status: String,
    text: String,
}

/// Send `{tag} {command}` and require an `OK` status.
///
/// `label` names the command in error messages and must never contain credentials.
async fn imap_expect_ok(
    imap: &SharedLineSocket,
    tag: &str,
    label: &str,
    command: &str,
) -> Result<ImapReply, String> {
    imap.send_line(&format!("{tag} {command}"))
        .await
        .map_err(|err| format!("imap transport error during {label}: {err}"))?;
    let reply = imap_await_tag(imap, tag, label).await?;
    if reply.status == "OK" {
        Ok(reply)
    } else {
        Err(format!(
            "imap {label} failed: {} {}",
            reply.status,
            shortened(&reply.text, REPLY_PREVIEW_CHARS)
        ))
    }
}

/// Read lines until the one tagged `{tag}`, keeping every untagged line seen on the way.
async fn imap_await_tag(
    imap: &SharedLineSocket,
    tag: &str,
    label: &str,
) -> Result<ImapReply, String> {
    let mut lines: Vec<String> = Vec::new();
    loop {
        let Some(line) = imap
            .next_line()
            .await
            .map_err(|err| format!("imap transport error during {label}: {err}"))?
        else {
            return Err(format!("imap connection closed while waiting for {label}"));
        };
        lines.push(line.clone());
        let Some(rest) = strip_tag(&line, tag) else {
            continue;
        };
        let mut parts = rest.splitn(2, char::is_whitespace);
        let status = parts.next().unwrap_or_default().to_uppercase();
        let text = parts.next().unwrap_or_default().trim().to_string();
        if status.is_empty() {
            return Err(format!(
                "imap reply to {label} carried no status: {}",
                shortened(&line, REPLY_PREVIEW_CHARS)
            ));
        }
        return Ok(ImapReply {
            lines,
            status,
            text,
        });
    }
}

/// `Some(rest)` when `line` is tagged `{tag}` (a tag is followed by a space or ends the line).
fn strip_tag<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(tag)?;
    if rest.is_empty() {
        return Some(rest);
    }
    rest.starts_with(' ').then(|| rest.trim_start())
}

/// Fetch one message body with `UID FETCH … (BODY.PEEK[])` and return its literal.
///
/// The literal is the line the transport hands over right after the `… BODY[] {len}` header (see
/// the module note: a byte-aware transport must deliver the literal as one line).
async fn imap_fetch(imap: &SharedLineSocket, tag: &str, uid: u32) -> Result<String, String> {
    imap.send_line(&format!("{tag} UID FETCH {uid} (BODY.PEEK[])"))
        .await
        .map_err(|err| format!("imap transport error during UID FETCH {uid}: {err}"))?;
    let mut literal: Option<String> = None;
    loop {
        let Some(line) = imap
            .next_line()
            .await
            .map_err(|err| format!("imap transport error during UID FETCH {uid}: {err}"))?
        else {
            return Err(format!(
                "imap connection closed while waiting for UID FETCH {uid}"
            ));
        };
        if let Some(rest) = strip_tag(&line, tag) {
            let status = rest
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_uppercase();
            if status != "OK" {
                return Err(format!(
                    "imap UID FETCH {uid} failed: {} {}",
                    status,
                    shortened(&line, REPLY_PREVIEW_CHARS)
                ));
            }
            return literal
                .ok_or_else(|| format!("imap UID FETCH {uid} answered without a BODY[] literal"));
        }
        if is_fetch_literal_header(&line) {
            let next = imap
                .next_line()
                .await
                .map_err(|err| format!("imap transport error during UID FETCH {uid}: {err}"))?;
            literal = Some(next.ok_or_else(|| {
                format!("imap connection closed inside the literal of UID FETCH {uid}")
            })?);
        }
    }
}

/// Is this the untagged `* n FETCH (… BODY[] {len}` line whose next line is the literal?
fn is_fetch_literal_header(line: &str) -> bool {
    line.starts_with("* ")
        && line.contains("BODY[")
        && line
            .split_whitespace()
            .any(|word| word.eq_ignore_ascii_case("FETCH"))
}

/// Uids from `* SEARCH 1 2 3` (the untagged line of a `UID SEARCH UNSEEN` reply), de-duplicated.
fn parse_search_uids(lines: &[String]) -> Vec<u32> {
    let mut uids: Vec<u32> = Vec::new();
    for line in lines {
        let Some(rest) = line.strip_prefix("* ") else {
            continue;
        };
        let rest = rest.trim_start();
        if !rest
            .get(..6)
            .is_some_and(|keyword| keyword.eq_ignore_ascii_case("SEARCH"))
        {
            continue;
        }
        for token in rest.get(6..).unwrap_or_default().split_whitespace() {
            if let Ok(uid) = token.parse::<u32>()
                && !uids.contains(&uid)
            {
                uids.push(uid);
            }
        }
    }
    uids
}

/// Build the gateway event for one fetched message, or `None` when it has no usable sender.
fn parse_imap_message(uid: u32, literal: &str) -> Option<MessageEvent> {
    let (header_block, body) = split_headers(literal);
    let headers = parse_headers(header_block);

    let sender = sender_address(header(&headers, "From")?)?;
    let subject = header(&headers, "Subject").unwrap_or_default();
    let date = header(&headers, "Date").unwrap_or_default();
    let message_id = header(&headers, "Message-ID")
        .map(str::to_string)
        .unwrap_or_else(|| uid.to_string());

    let text = format!("{subject}\n\n{body}").trim().to_string();

    let mut source = SessionSource::new(email_platform(), sender.clone());
    source.chat_name = Some(sender.clone());
    source.chat_type = "dm".to_string();
    source.user_id = Some(sender.clone());
    source.user_name = Some(sender.clone());
    source.message_id = Some(message_id.clone());

    let mut event = MessageEvent::new(text, source);
    event.user_id = Some(sender.clone());
    event.user_name = Some(sender);
    event.message_id = Some(message_id);
    event.raw_message = Some(serde_json::json!({
        "uid": uid,
        "date": date,
        "literal": literal,
    }));
    Some(event)
}

/// Split an RFC 5322 message at the first blank line into `(headers, body)`.
fn split_headers(literal: &str) -> (&str, &str) {
    if let Some((headers, body)) = literal.split_once("\n\n") {
        return (headers, body);
    }
    if let Some((headers, body)) = literal.split_once("\r\n\r\n") {
        return (headers, body);
    }
    (literal, "")
}

/// Parse header lines into `(name, value)` pairs, unfolding continuation lines.
///
/// Names keep their wire spelling; look them up with [`header`], which is case-insensitive.
fn parse_headers(block: &str) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in block.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = headers.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    headers
}

/// First header with `name`, case-insensitively.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The bare address of a `From:` value: `Alice <alice@example.com>` → `alice@example.com`.
fn sender_address(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let address = match (trimmed.rfind('<'), trimmed.rfind('>')) {
        (Some(start), Some(end)) if start < end => trimmed.get(start + 1..end).unwrap_or(trimmed),
        _ => trimmed,
    };
    let address = address.trim().trim_matches('"').trim();
    if address.is_empty() {
        None
    } else {
        Some(address.to_string())
    }
}

/// An IMAP quoted string (RFC 3501 `quoted`): `"` and `\` escaped, control characters dropped.
fn quote_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            // A bare CR/LF cannot appear inside a quoted string.
            '\r' | '\n' => {}
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// The gateway's platform handle for this adapter. The constant is a fixed non-empty name, so the
/// fallback arm is unreachable and exists only to keep this function panic-free.
fn email_platform() -> Platform {
    match Platform::new(platform_names::EMAIL) {
        Some(platform) => platform,
        None => Platform::local(),
    }
}

/// First `limit` characters plus an ellipsis, for error text.
fn shortened(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

/// Standard-alphabet Base64 with `=` padding (RFC 4648 §4), implemented here because the crate may
/// not add dependencies. Only `AUTH LOGIN` needs it.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = chunk.first().copied().unwrap_or(0) as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (first << 16) | (second << 8) | third;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::{LineSocket, SocketError};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, MutexGuard};

    /// Records every line written and replays the scripted incoming lines in order.
    struct MockSocket {
        sent: Mutex<Vec<String>>,
        incoming: Mutex<VecDeque<String>>,
    }

    impl MockSocket {
        fn new(incoming: Vec<&str>) -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                incoming: Mutex::new(incoming.into_iter().map(str::to_string).collect()),
            })
        }

        fn sent(&self) -> Vec<String> {
            lock(&self.sent).clone()
        }
    }

    #[async_trait]
    impl LineSocket for MockSocket {
        async fn send_line(&self, line: &str) -> Result<(), SocketError> {
            lock(&self.sent).push(line.to_string());
            Ok(())
        }

        async fn next_line(&self) -> Result<Option<String>, SocketError> {
            Ok(lock(&self.incoming).pop_front())
        }
    }

    /// An SMTP socket that accepts `ok_lines` writes and then fails every further write.
    struct WriteFailingSocket {
        ok_lines: usize,
        written: Mutex<usize>,
        incoming: Mutex<VecDeque<String>>,
    }

    impl WriteFailingSocket {
        fn new(ok_lines: usize, incoming: Vec<&str>) -> Arc<Self> {
            Arc::new(Self {
                ok_lines,
                written: Mutex::new(0),
                incoming: Mutex::new(incoming.into_iter().map(str::to_string).collect()),
            })
        }
    }

    #[async_trait]
    impl LineSocket for WriteFailingSocket {
        async fn send_line(&self, _line: &str) -> Result<(), SocketError> {
            let mut written = lock(&self.written);
            if *written >= self.ok_lines {
                return Err(SocketError::Io("connection reset by peer".to_string()));
            }
            *written += 1;
            Ok(())
        }

        async fn next_line(&self) -> Result<Option<String>, SocketError> {
            Ok(lock(&self.incoming).pop_front())
        }
    }

    /// Poisoned-lock-free guard: a panic in one test must not cascade into an unreadable failure.
    fn lock<T>(guard: &Mutex<T>) -> MutexGuard<'_, T> {
        guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn shared(socket: &Arc<MockSocket>) -> SharedLineSocket {
        Arc::clone(socket) as SharedLineSocket
    }

    const FROM: &str = "bot@example.com";
    const RECIPIENT: &str = "bob@example.com";

    fn adapter(smtp: &Arc<MockSocket>, imap: Option<&Arc<MockSocket>>) -> EmailAdapter {
        EmailAdapter::new(shared(smtp), imap.map(shared), FROM, "INBOX")
    }

    /// Greeting, EHLO, AUTH LOGIN and one accepted message.
    fn smtp_session() -> Vec<&'static str> {
        vec![
            "220 mock.test ESMTP",
            "250-mock.test greets you",
            "250-PIPELINING",
            "250 AUTH LOGIN PLAIN",
            "334 VXNlcm5hbWU6",
            "334 UGFzc3dvcmQ6",
            "235 2.7.0 accepted",
            "250 2.1.0 sender ok",
            "250 2.1.5 recipient ok",
            "354 end with . on a line by itself",
            "250 2.0.0 queued as abc123",
        ]
    }

    /// One SMTP dialogue for a message body: sender ok, recipient ok, `DATA`, then the verdict.
    fn media_session(verdict: &'static str) -> Vec<&'static str> {
        vec!["250 sender ok", "250 recipient ok", "354 go", verdict]
    }

    /// The boundary a submitted message declared, read back out of its `Content-Type` header.
    fn boundary_of(sent: &[String]) -> String {
        sent.iter()
            .find_map(|line| line.strip_prefix("Content-Type: multipart/mixed; boundary=\""))
            .and_then(|rest| rest.strip_suffix('"'))
            .map(str::to_string)
            .expect("a multipart/mixed Content-Type header")
    }

    const MESSAGE_ONE: &str = "From: Alice <alice@example.com>\nSubject: Hello there\nMessage-ID: <m1@example.com>\nDate: Mon, 1 Jan 2024 00:00:00 +0000\n\nBody line one\nBody line two";
    const MESSAGE_TWO: &str = "from: bob@example.com\nsubject: Re: hello\nmessage-id: <m2@example.com>\ndate: Tue, 2 Jan 2024 00:00:00 +0000\n\n  reply body  ";

    /// LOGIN, SELECT, a two-hit SEARCH, both FETCHes and both STOREs.
    fn imap_session() -> Vec<&'static str> {
        vec![
            "a1 OK LOGIN completed",
            "a2 OK [READ-WRITE] SELECT completed",
            "* SEARCH 7 11",
            "a3 OK SEARCH completed",
            "* 1 FETCH (UID 7 BODY[] 120)",
            MESSAGE_ONE,
            ")",
            "a4 OK FETCH completed",
            "* 2 FETCH (UID 11 BODY[] 90)",
            MESSAGE_TWO,
            ")",
            "a4 OK FETCH completed",
            "a5 OK STORE completed",
            "a5 OK STORE completed",
        ]
    }

    #[test]
    fn base64_matches_the_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // The two credential lines AUTH LOGIN sends.
        assert_eq!(base64_encode(b"alice"), "YWxpY2U=");
        assert_eq!(base64_encode(b"s3cr3t"), "czNjcjN0");
    }

    #[tokio::test]
    async fn reply_parser_folds_multiline_continuations() {
        let socket = MockSocket::new(vec![
            "250-mock.test greets you",
            "250-PIPELINING",
            "250 AUTH LOGIN PLAIN",
        ]);
        let reply = read_reply(&shared(&socket)).await.expect("a reply");
        assert_eq!(reply.code, 250);
        assert_eq!(
            reply.text,
            "mock.test greets you\nPIPELINING\nAUTH LOGIN PLAIN"
        );

        let single = MockSocket::new(vec!["220 ready"]);
        let reply = read_reply(&shared(&single)).await.expect("a reply");
        assert_eq!((reply.code, reply.text.as_str()), (220, "ready"));

        let bare = MockSocket::new(vec!["354"]);
        let reply = read_reply(&shared(&bare)).await.expect("a reply");
        assert_eq!((reply.code, reply.text.as_str()), (354, ""));

        // A line that is not a status line, and a stream that simply ends, are both errors.
        let junk = MockSocket::new(vec!["not a status line"]);
        assert!(read_reply(&shared(&junk)).await.is_err());
        let closed = MockSocket::new(vec![]);
        assert!(read_reply(&shared(&closed)).await.is_err());
    }

    #[tokio::test]
    async fn connect_and_send_run_the_smtp_dialogue_with_dot_stuffing() {
        let smtp = MockSocket::new(smtp_session());
        let mut adapter = adapter(&smtp, None).with_credentials("alice", "s3cr3t");
        assert!(adapter.connect(false).await);

        let result = adapter
            .send(RECIPIENT, "hello\n.leading dot\nbye", None, None)
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("vikram-1"));
        assert_eq!(result.error_kind, None);
        assert!(!result.retryable);

        assert_eq!(
            smtp.sent(),
            vec![
                "EHLO vikram",
                "AUTH LOGIN",
                "YWxpY2U=",
                "czNjcjN0",
                "MAIL FROM:<bot@example.com>",
                "RCPT TO:<bob@example.com>",
                "DATA",
                "Subject: vikram",
                "Content-Type: text/plain; charset=utf-8",
                "",
                "hello",
                "..leading dot",
                "bye",
                ".",
            ]
        );
    }

    #[tokio::test]
    async fn connect_without_credentials_skips_auth() {
        let smtp = MockSocket::new(vec![
            "220 mock.test ESMTP",
            "250-mock.test greets you",
            "250 SIZE 10240000",
        ]);
        let mut adapter = adapter(&smtp, None);
        assert!(adapter.connect(false).await);
        assert_eq!(smtp.sent(), vec!["EHLO vikram"]);
        assert_eq!(adapter.from_address(), FROM);
        assert_eq!(adapter.mailbox(), "INBOX");
    }

    #[tokio::test]
    async fn connect_fails_on_a_bad_greeting_ehlo_or_password() {
        // 554: the server refuses to serve us at all.
        let greeting = MockSocket::new(vec!["554 5.3.2 no service here"]);
        let mut bad_greeting = adapter(&greeting, None);
        assert!(!bad_greeting.connect(false).await);
        assert!(greeting.sent().is_empty(), "nothing to say before EHLO");

        // EHLO refused.
        let ehlo = MockSocket::new(vec!["220 ready", "502 5.5.1 no EHLO here"]);
        let mut bad_ehlo = adapter(&ehlo, None);
        assert!(!bad_ehlo.connect(false).await);
        assert_eq!(ehlo.sent(), vec!["EHLO vikram"]);

        // Wrong password: 535 instead of 235.
        let auth = MockSocket::new(vec![
            "220 ready",
            "250 ok",
            "334 VXNlcm5hbWU6",
            "334 UGFzc3dvcmQ6",
            "535 5.7.8 credentials invalid",
        ]);
        let mut bad_auth = adapter(&auth, None).with_credentials("alice", "wrong");
        assert!(!bad_auth.connect(false).await);
        assert_eq!(
            auth.sent(),
            vec!["EHLO vikram", "AUTH LOGIN", "YWxpY2U=", "d3Jvbmc=",]
        );

        // A socket that dies before the greeting.
        let closed = MockSocket::new(vec![]);
        let mut no_socket = adapter(&closed, None);
        assert!(!no_socket.connect(false).await);
    }

    #[tokio::test]
    async fn smtp_refusals_are_classified() {
        // 550 on MAIL FROM is permanent: bad_format, not retryable, and no RCPT/DATA after it.
        let permanent = MockSocket::new(vec!["550 5.1.1 sender rejected"]);
        let result = adapter(&permanent, None)
            .send(RECIPIENT, "hi", None, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("550"), "{error}");
        assert!(error.contains("MAIL FROM"), "{error}");
        assert_eq!(permanent.sent(), vec!["MAIL FROM:<bot@example.com>"]);

        // 451 on RCPT TO is temporary: transient and retryable.
        let transient = MockSocket::new(vec!["250 sender ok", "451 4.3.0 try again later"]);
        let result = adapter(&transient, None)
            .send(RECIPIENT, "hi", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result.error.unwrap_or_default().contains("451"),
            "the server code must survive into the error text"
        );

        // A 554 at the end of DATA is a permanent body refusal.
        let body = MockSocket::new(vec![
            "250 ok",
            "250 ok",
            "354 go",
            "554 5.6.0 content rejected",
        ]);
        let result = adapter(&body, None).send(RECIPIENT, "hi", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));

        // A socket that dies mid-dialogue is a transport failure: transient and retryable.
        let closed = MockSocket::new(vec![]);
        let result = adapter(&closed, None)
            .send(RECIPIENT, "hi", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn blank_content_and_blank_recipient_never_touch_the_wire() {
        for content in ["", "   ", "\n\t "] {
            let smtp = MockSocket::new(vec![]);
            let result = adapter(&smtp, None)
                .send(RECIPIENT, content, None, None)
                .await;
            assert!(!result.success, "{content:?}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
            assert!(
                smtp.sent().is_empty(),
                "blank content must not reach the wire: {content:?}"
            );
        }
        let smtp = MockSocket::new(vec![]);
        let result = adapter(&smtp, None).send("  ", "hello", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(smtp.sent().is_empty());
    }

    #[tokio::test]
    async fn outbound_message_ids_are_unique_per_send() {
        let smtp = MockSocket::new(vec![
            "250 ok",
            "250 ok",
            "354 go",
            "250 queued",
            "250 ok",
            "250 ok",
            "354 go",
            "250 queued",
        ]);
        let adapter = adapter(&smtp, None);
        let first = adapter.send(RECIPIENT, "one", None, None).await;
        let second = adapter.send(RECIPIENT, "two", None, None).await;
        assert_eq!(first.message_id.as_deref(), Some("vikram-1"));
        assert_eq!(second.message_id.as_deref(), Some("vikram-2"));
    }

    #[tokio::test]
    async fn chat_info_is_the_address_as_a_dm() {
        let smtp = MockSocket::new(vec![]);
        let info = adapter(&smtp, None).get_chat_info(RECIPIENT).await;
        assert_eq!(info.name, RECIPIENT);
        assert_eq!(info.chat_type, "dm");
        assert!(
            smtp.sent().is_empty(),
            "there is no chat metadata to ask for"
        );
    }

    #[tokio::test]
    async fn imap_poll_logs_in_searches_and_builds_events() {
        let smtp = MockSocket::new(vec![]);
        let imap = MockSocket::new(imap_session());
        let adapter = adapter(&smtp, Some(&imap)).with_credentials("alice@example.com", "hunter2");

        let events = adapter.poll_once().await.expect("two events");
        assert_eq!(events.len(), 2);

        let first = &events[0];
        assert_eq!(first.text, "Hello there\n\nBody line one\nBody line two");
        assert_eq!(first.source.platform.as_str(), "email");
        assert_eq!(first.source.chat_id, "alice@example.com");
        assert_eq!(first.source.chat_name.as_deref(), Some("alice@example.com"));
        assert_eq!(first.source.chat_type, "dm");
        assert_eq!(first.source.user_id.as_deref(), Some("alice@example.com"));
        assert_eq!(first.user_id.as_deref(), Some("alice@example.com"));
        assert_eq!(first.user_name.as_deref(), Some("alice@example.com"));
        assert_eq!(first.message_id.as_deref(), Some("<m1@example.com>"));
        assert_eq!(first.source.message_id, first.message_id);
        match first.raw_message.as_ref() {
            Some(raw) => {
                assert_eq!(raw["uid"], serde_json::json!(7));
                assert_eq!(
                    raw["date"],
                    serde_json::json!("Mon, 1 Jan 2024 00:00:00 +0000")
                );
            }
            None => panic!("the raw fetch payload must be kept"),
        }

        // Headers are matched case-insensitively and the bare address is the chat id.
        let second = &events[1];
        assert_eq!(second.text, "Re: hello\n\n  reply body");
        assert_eq!(second.source.chat_id, "bob@example.com");
        assert_eq!(second.message_id.as_deref(), Some("<m2@example.com>"));

        assert_eq!(
            imap.sent(),
            vec![
                "a1 LOGIN \"alice@example.com\" \"hunter2\"",
                "a2 SELECT \"INBOX\"",
                "a3 UID SEARCH UNSEEN",
                "a4 UID FETCH 7 (BODY.PEEK[])",
                "a4 UID FETCH 11 (BODY.PEEK[])",
                "a5 UID STORE 7 +FLAGS (\\Seen)",
                "a5 UID STORE 11 +FLAGS (\\Seen)",
            ]
        );
        assert!(
            smtp.sent().is_empty(),
            "polling must not use the SMTP socket"
        );
    }

    #[tokio::test]
    async fn an_empty_search_returns_no_events() {
        let smtp = MockSocket::new(vec![]);
        let imap = MockSocket::new(vec![
            "a1 OK LOGIN completed",
            "a2 OK SELECT completed",
            "* SEARCH",
            "a3 OK SEARCH completed",
        ]);
        let adapter = adapter(&smtp, Some(&imap)).with_credentials("u", "p");
        assert!(adapter.poll_once().await.expect("no events").is_empty());
        assert!(
            !imap.sent().iter().any(|line| line.starts_with("a4")),
            "an empty SEARCH must not FETCH anything"
        );
    }

    #[tokio::test]
    async fn imap_failures_are_errors() {
        let smtp = MockSocket::new(vec![]);

        // A FETCH that answered without a BODY[] literal is malformed.
        let imap = MockSocket::new(vec![
            "a1 OK LOGIN completed",
            "a2 OK SELECT completed",
            "* SEARCH 3",
            "a3 OK SEARCH completed",
            "* 1 FETCH (UID 3 FLAGS (\\Seen))",
            "a4 OK FETCH completed",
        ]);
        let malformed = adapter(&smtp, Some(&imap)).with_credentials("u", "hunter2");
        let error = malformed
            .poll_once()
            .await
            .expect_err("a literal-less FETCH is malformed");
        assert!(error.contains("BODY[]"), "{error}");

        // A refused LOGIN is an error, and the password never appears in it.
        let imap = MockSocket::new(vec!["a1 NO [AUTHENTICATIONFAILED] invalid credentials"]);
        let refused = adapter(&smtp, Some(&imap)).with_credentials("u", "hunter2");
        let error = refused.poll_once().await.expect_err("NO is an error");
        assert!(error.contains("LOGIN"), "{error}");
        assert!(!error.contains("hunter2"), "the password leaked: {error}");

        // Polling without credentials cannot open a session.
        let imap = MockSocket::new(vec![]);
        let credential_less = adapter(&smtp, Some(&imap));
        assert!(credential_less.poll_once().await.is_err());
    }

    #[tokio::test]
    async fn imap_credentials_are_quote_escaped() {
        let smtp = MockSocket::new(vec![]);
        let imap = MockSocket::new(vec![
            "a1 OK LOGIN completed",
            "a2 OK SELECT completed",
            "* SEARCH",
            "a3 OK SEARCH completed",
        ]);
        let adapter = adapter(&smtp, Some(&imap)).with_credentials("al\"ice", "pa\\ss\"word");
        adapter.poll_once().await.expect("no events");
        let sent = imap.sent();
        assert_eq!(sent[0], "a1 LOGIN \"al\\\"ice\" \"pa\\\\ss\\\"word\"");
    }

    #[tokio::test]
    async fn a_message_without_a_sender_is_skipped_but_still_marked_seen() {
        let smtp = MockSocket::new(vec![]);
        let imap = MockSocket::new(vec![
            "a1 OK LOGIN completed",
            "a2 OK SELECT completed",
            "* SEARCH 9",
            "a3 OK SEARCH completed",
            "* 1 FETCH (UID 9 BODY[] 40)",
            "Subject: anonymous\n\nno sender here",
            ")",
            "a4 OK FETCH completed",
            "a5 OK STORE completed",
        ]);
        let adapter = adapter(&smtp, Some(&imap)).with_credentials("u", "p");
        let events = adapter.poll_once().await.expect("no event, no error");
        assert!(
            events.is_empty(),
            "a message with no From cannot be answered"
        );
        assert!(
            imap.sent()
                .contains(&"a5 UID STORE 9 +FLAGS (\\Seen)".to_string()),
            "an unanswerable message is still consumed"
        );
    }

    #[tokio::test]
    async fn polling_without_an_imap_socket_is_a_no_op() {
        let smtp = MockSocket::new(vec![]);
        let adapter = adapter(&smtp, None);
        assert!(adapter.poll_once().await.expect("no events").is_empty());
        assert!(smtp.sent().is_empty());
    }

    // -----------------------------------------------------------------------------------------
    // Outbound media
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn supports_media_is_true() {
        let smtp = MockSocket::new(vec![]);
        assert!(adapter(&smtp, None).supports_media());
        assert!(smtp.sent().is_empty(), "the capability flag has no wire");
    }

    #[tokio::test]
    async fn media_mime_transcript_is_a_multipart_mixed_message() {
        // 60 bytes encode to exactly 80 base64 characters: one full 76-char line plus a 4-char tail.
        let payload: Vec<u8> = (0..60u8).map(|i| b'a' + (i % 26)).collect();
        let smtp = MockSocket::new(media_session("250 2.0.0 Ok: queued as 4Wxyz123"));
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(payload.clone()))
            .with_caption("quarterly numbers");

        let result = adapter(&smtp, None)
            .send_media(RECIPIENT, &media, None)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("4Wxyz123"));
        assert!(result.error_kind.is_none());
        assert!(!result.retryable);
        match result.raw_response.as_ref() {
            Some(raw) => {
                assert_eq!(raw["queue_id"], serde_json::json!("4Wxyz123"));
                assert_eq!(
                    raw["smtp_reply"],
                    serde_json::json!("2.0.0 Ok: queued as 4Wxyz123")
                );
            }
            None => panic!("an accepted media message must keep the server reply"),
        }

        let sent = smtp.sent();
        let boundary = boundary_of(&sent);
        assert_eq!(boundary.len(), 47, "RFC 2046 caps a boundary at 70 chars");
        assert!(
            boundary.starts_with("----VIKRAMMediaBoundary"),
            "{boundary}"
        );

        let encoded = base64_encode(&payload);
        assert_eq!(encoded.len(), 80);
        let (first, second) = encoded.split_at(BASE64_LINE_CHARS);

        let expected = vec![
            "MAIL FROM:<bot@example.com>".to_string(),
            "RCPT TO:<bob@example.com>".to_string(),
            "DATA".to_string(),
            "Subject: vikram".to_string(),
            "MIME-Version: 1.0".to_string(),
            format!("Content-Type: multipart/mixed; boundary=\"{boundary}\""),
            String::new(),
            format!("--{boundary}"),
            "Content-Type: text/plain; charset=utf-8".to_string(),
            "Content-Transfer-Encoding: 8bit".to_string(),
            String::new(),
            "quarterly numbers".to_string(),
            format!("--{boundary}"),
            "Content-Type: application/octet-stream".to_string(),
            "Content-Transfer-Encoding: base64".to_string(),
            "Content-Disposition: attachment; filename=\"attachment.bin\"".to_string(),
            String::new(),
            first.to_string(),
            second.to_string(),
            format!("--{boundary}--"),
            ".".to_string(),
        ];
        assert_eq!(sent, expected);
    }

    #[tokio::test]
    async fn media_attachment_defaults_and_the_caption_policy() {
        // No caption, filename or mime: per-kind defaults plus a body that names the attachment.
        let smtp = MockSocket::new(media_session("250 ok"));
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"png".to_vec()));
        assert!(
            adapter(&smtp, None)
                .send_media(RECIPIENT, &media, None)
                .await
                .success
        );
        let sent = smtp.sent();
        assert!(
            sent.contains(&"Content-Type: image/png".to_string()),
            "{sent:?}"
        );
        assert!(
            sent.contains(&"Content-Disposition: attachment; filename=\"image.png\"".to_string()),
            "{sent:?}"
        );
        assert!(
            sent.contains(&"Attached file: image.png (image/png)".to_string()),
            "{sent:?}"
        );

        // A blank caption falls back to the same default body rather than an empty text part.
        let smtp = MockSocket::new(media_session("250 ok"));
        let media = OutboundMedia::new(MediaKind::Video, MediaSource::Bytes(b"mp4".to_vec()))
            .with_caption("   \n\t ");
        assert!(
            adapter(&smtp, None)
                .send_media(RECIPIENT, &media, None)
                .await
                .success
        );
        let sent = smtp.sent();
        assert!(
            sent.contains(&"Content-Type: video/mp4".to_string()),
            "{sent:?}"
        );
        assert!(
            sent.contains(&"Attached file: video.mp4 (video/mp4)".to_string()),
            "{sent:?}"
        );

        // Caller values win; a path is cut to its basename.
        let smtp = MockSocket::new(media_session("250 ok"));
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"a,b\n".to_vec()))
            .with_filename("/tmp/reports/Q3.csv")
            .with_mime("text/csv")
            .with_caption("numbers");
        assert!(
            adapter(&smtp, None)
                .send_media(RECIPIENT, &media, None)
                .await
                .success
        );
        let sent = smtp.sent();
        assert!(
            sent.contains(&"Content-Disposition: attachment; filename=\"Q3.csv\"".to_string()),
            "{sent:?}"
        );
        assert!(
            sent.contains(&"Content-Type: text/csv".to_string()),
            "{sent:?}"
        );
        assert!(sent.contains(&"numbers".to_string()), "{sent:?}");
        assert!(
            !sent
                .iter()
                .any(|line| line.contains('\r') || line.contains('\n')),
            "a header line must never carry a line break: {sent:?}"
        );

        // A MIME value with parameters is not a MIME type, and a filename with a CRLF is not a
        // filename: both fall back / get cleaned instead of ending up in a header verbatim.
        let smtp = MockSocket::new(media_session("250 ok"));
        let media = OutboundMedia::new(MediaKind::Audio, MediaSource::Bytes(b"ID3".to_vec()))
            .with_filename("voice\r\n.wav")
            .with_mime("audio/wav; charset=binary");
        assert!(
            adapter(&smtp, None)
                .send_media(RECIPIENT, &media, None)
                .await
                .success
        );
        let sent = smtp.sent();
        assert!(
            sent.contains(&"Content-Type: audio/mpeg".to_string()),
            "{sent:?}"
        );
        assert!(
            sent.contains(&"Content-Disposition: attachment; filename=\"voice.wav\"".to_string()),
            "{sent:?}"
        );
    }

    #[tokio::test]
    async fn media_body_is_dot_stuffed_like_a_text_send() {
        let smtp = MockSocket::new(media_session("250 ok"));
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"x".to_vec()))
            .with_caption(".hidden\n.\nkept");
        assert!(
            adapter(&smtp, None)
                .send_media(RECIPIENT, &media, None)
                .await
                .success
        );

        let sent = smtp.sent();
        let text_part = sent
            .iter()
            .position(|line| line == "Content-Type: text/plain; charset=utf-8")
            .expect("a text part");
        assert_eq!(
            sent.get(text_part + 1).map(String::as_str),
            Some("Content-Transfer-Encoding: 8bit")
        );
        assert_eq!(sent.get(text_part + 2).map(String::as_str), Some(""));
        assert_eq!(
            sent.get(text_part + 3).map(String::as_str),
            Some("..hidden")
        );
        assert_eq!(sent.get(text_part + 4).map(String::as_str), Some(".."));
        assert_eq!(sent.get(text_part + 5).map(String::as_str), Some("kept"));
        assert!(!sent.contains(&".hidden".to_string()), "{sent:?}");
        // The end-of-data terminator stays the only bare dot in the payload's tail.
        assert_eq!(sent.last().map(String::as_str), Some("."));
        assert_eq!(
            sent.iter().filter(|line| *line == ".").count(),
            1,
            "{sent:?}"
        );
    }

    #[tokio::test]
    async fn media_refusals_are_classified() {
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"data".to_vec()));

        // 550 on MAIL FROM is permanent: bad_format, not retryable, nothing after it.
        let smtp = MockSocket::new(vec!["550 5.1.1 sender rejected"]);
        let result = adapter(&smtp, None)
            .send_media(RECIPIENT, &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        assert!(result.message_id.is_none());
        let error = result.error.as_deref().unwrap_or_default().to_string();
        assert!(
            error.contains("550") && error.contains("MAIL FROM"),
            "{error}"
        );
        assert_eq!(smtp.sent(), vec!["MAIL FROM:<bot@example.com>"]);

        // 451 on RCPT TO is temporary: transient, retryable, and DATA is never reached.
        let smtp = MockSocket::new(vec!["250 sender ok", "451 4.3.0 try again later"]);
        let result = adapter(&smtp, None)
            .send_media(RECIPIENT, &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(result.message_id.is_none());
        assert!(
            result.error.as_deref().unwrap_or_default().contains("451"),
            "the server code must survive into the error text"
        );
        assert!(!smtp.sent().contains(&"DATA".to_string()));

        // 550 after DATA: the message was written but refused, so success is never reported.
        let smtp = MockSocket::new(media_session("550 5.6.0 content rejected"));
        let result = adapter(&smtp, None)
            .send_media(RECIPIENT, &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(result.message_id.is_none());
        assert!(result.raw_response.is_none());
        assert!(
            result.error.as_deref().unwrap_or_default().contains("550"),
            "{:?}",
            result.error
        );
        assert_eq!(smtp.sent().last().map(String::as_str), Some("."));
    }

    #[tokio::test]
    async fn media_queue_id_is_reported_only_when_the_server_names_one() {
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"data".to_vec()));

        for (verdict, expected) in [
            ("250 2.0.0 Ok: queued as 4Wxyz123", "4Wxyz123"),
            ("250 OK id=1a2b3c-000", "1a2b3c-000"),
        ] {
            let smtp = MockSocket::new(media_session(verdict));
            let result = adapter(&smtp, None)
                .send_media(RECIPIENT, &media, None)
                .await;
            assert!(result.success, "{verdict}");
            assert_eq!(result.message_id.as_deref(), Some(expected), "{verdict}");
        }

        // A folded 250 keeps its queue id in the terminating line.
        let smtp = MockSocket::new(vec![
            "250 sender ok",
            "250 recipient ok",
            "354 go",
            "250-mock.test says",
            "250 2.0.0 Ok: queued as FOLDED1",
        ]);
        let result = adapter(&smtp, None)
            .send_media(RECIPIENT, &media, None)
            .await;
        assert_eq!(result.message_id.as_deref(), Some("FOLDED1"));

        // A 250 that names no queue id gets the truthful local id, never a fabricated server one.
        let smtp = MockSocket::new(media_session("250 OK"));
        let result = adapter(&smtp, None)
            .send_media(RECIPIENT, &media, None)
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("vikram-1"));
        match result.raw_response.as_ref() {
            Some(raw) => assert_eq!(raw["queue_id"], serde_json::Value::Null),
            None => panic!("an accepted media message must keep the server reply"),
        }

        // `send` is untouched by that rule: the same accepted reply still yields the local id.
        let smtp = MockSocket::new(media_session("250 2.0.0 Ok: queued as 4Wxyz123"));
        let result = adapter(&smtp, None)
            .send(RECIPIENT, "hello", None, None)
            .await;
        assert_eq!(result.message_id.as_deref(), Some("vikram-1"));
    }

    #[tokio::test]
    async fn media_url_empty_bytes_and_blank_recipient_never_touch_the_wire() {
        // A URL cannot be fetched here: refused with the reason, zero SMTP writes.
        let smtp = MockSocket::new(vec![]);
        let media = OutboundMedia::image_url("https://example.test/a.png");
        let result = adapter(&smtp, None)
            .send_media(RECIPIENT, &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(result.message_id.is_none());
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("https://example.test/a.png"), "{error}");
        assert!(error.contains("Bytes"), "the fix must be named: {error}");
        assert!(smtp.sent().is_empty());

        // Empty bytes are not a zero-byte attachment.
        let smtp = MockSocket::new(vec![]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(Vec::new()));
        let result = adapter(&smtp, None)
            .send_media(RECIPIENT, &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(smtp.sent().is_empty());

        // A blank recipient is refused before MAIL FROM.
        let smtp = MockSocket::new(vec![]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"x".to_vec()));
        let result = adapter(&smtp, None).send_media("  ", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(smtp.sent().is_empty());
    }

    #[tokio::test]
    async fn media_threads_with_the_reply_parent_id() {
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"data".to_vec()));

        let smtp = MockSocket::new(media_session("250 ok"));
        assert!(
            adapter(&smtp, None)
                .send_media(RECIPIENT, &media, Some("<m1@example.com>"))
                .await
                .success
        );
        let sent = smtp.sent();
        assert!(
            sent.contains(&"In-Reply-To: <m1@example.com>".to_string()),
            "{sent:?}"
        );
        assert!(
            sent.contains(&"References: <m1@example.com>".to_string()),
            "{sent:?}"
        );

        // A bare id gains the angle brackets; whitespace cannot smuggle in another header.
        let smtp = MockSocket::new(media_session("250 ok"));
        assert!(
            adapter(&smtp, None)
                .send_media(
                    RECIPIENT,
                    &media,
                    Some("m2@example.com\r\nBcc: eve@example.com"),
                )
                .await
                .success
        );
        let sent = smtp.sent();
        assert!(
            sent.contains(&"In-Reply-To: <m2@example.comBcc:eve@example.com>".to_string()),
            "{sent:?}"
        );
        assert!(
            !sent.iter().any(|line| line.starts_with("Bcc:")),
            "header injection: {sent:?}"
        );

        // No parent, no threading headers.
        let smtp = MockSocket::new(media_session("250 ok"));
        assert!(
            adapter(&smtp, None)
                .send_media(RECIPIENT, &media, Some("   "))
                .await
                .success
        );
        let sent = smtp.sent();
        assert!(
            !sent
                .iter()
                .any(|line| line.starts_with("In-Reply-To:") || line.starts_with("References:")),
            "{sent:?}"
        );
    }

    #[tokio::test]
    async fn media_transport_failures_are_transient() {
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"data".to_vec()));

        // A socket that dies while the DATA verdict is awaited.
        let smtp = MockSocket::new(vec!["250 ok", "250 ok", "354 go"]);
        let result = adapter(&smtp, None)
            .send_media(RECIPIENT, &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(result.message_id.is_none());

        // A write that fails part-way through the multipart body.
        let socket = WriteFailingSocket::new(3, vec!["250 ok", "250 ok", "354 go"]);
        let adapter =
            EmailAdapter::new(Arc::clone(&socket) as SharedLineSocket, None, FROM, "INBOX");
        let result = adapter.send_media(RECIPIENT, &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(result.message_id.is_none());
    }

    #[test]
    fn base64_lines_wrap_at_76_characters() {
        // 60 bytes encode to 80 characters: one full line and a 4-character tail.
        let wrapped = base64_lines(&[0u8; 60]);
        assert_eq!(wrapped.len(), 2, "{wrapped:?}");
        assert_eq!(wrapped[0].len(), BASE64_LINE_CHARS);
        assert_eq!(wrapped[1].len(), 4);
        assert!(
            wrapped
                .iter()
                .all(|line| line.len() <= BASE64_LINE_CHARS && !line.starts_with('.')),
            "{wrapped:?}"
        );
        // 57 bytes are exactly one full line.
        assert_eq!(base64_lines(&[0u8; 57]), vec!["A".repeat(76)]);
    }
}
