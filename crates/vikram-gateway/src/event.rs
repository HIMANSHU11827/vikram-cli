//! Inbound message event types shared by every gateway platform adapter.
//!
//! A leaf module: adapters, helpers and the runner import it, so it must not
//! import from sibling gateway modules.
//!
//! Rust port of hermes-agent `gateway/platforms/event.py` and the
//! `SessionSource` model from `gateway/session.py`.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Desktop attachment reference tags prepended by Desktop's context builder
/// before the user's visible text (e.g. `@image:/tmp/foo.png\n\n/moa ask something`).
/// Strip these when detecting slash commands so a media-ref prefix does not hide
/// a slash token from [`MessageEvent::is_command`].
///
/// Matches to end-of-line (not just whitespace-bounded) to handle Windows paths
/// that may contain spaces.
const ATTACHMENT_REF_PREFIXES: [&str; 3] = ["@image:", "@file:", "@url:"];
const MAX_FILENAME_CHARS: usize = 255;

fn strip_attachment_refs(text: &str) -> &str {
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start();
        let mut matched = false;
        for prefix in ATTACHMENT_REF_PREFIXES {
            if trimmed.len() >= prefix.len() && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix)
            {
                match trimmed.find('\n') {
                    Some(idx) => {
                        rest = &trimmed[idx + 1..];
                    }
                    None => {
                        rest = "";
                    }
                }
                matched = true;
                break;
            }
        }
        if !matched {
            return trimmed;
        }
    }
}

fn is_format_char(ch: char) -> bool {
    // Unicode category Cf (format). Rust std has no general-category tables;
    // cover the Cf codepoints that actually act as invisible controls.
    matches!(ch as u32,
        0x00AD | 0x0600..=0x0605 | 0x06DD | 0x070F | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F
        | 0xFEFF | 0xFFF9..=0xFFFB
        | 0x110BD | 0x110CD | 0x13430..=0x13438 | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
        | 0xE0001 | 0xE0020..=0xE007F)
}

/// An untrusted attachment filename reduced to a display-safe basename; `""` when
/// unusable. Directory parts (either separator), control and format characters are
/// dropped and the length is capped with the extension kept. Never a storage
/// path: callers keep their own random cache names.
pub fn clean_attachment_filename(name: &str) -> String {
    let base = name
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|&ch| !ch.is_control() && !is_format_char(ch))
        .collect::<String>()
        .trim()
        .to_string();
    if base.is_empty() || base == "." || base == ".." {
        return String::new();
    }
    if base.chars().count() <= MAX_FILENAME_CHARS {
        return base;
    }
    match base.rfind('.') {
        Some(dot_idx) => {
            let ext = &base[dot_idx..];
            let ext_len = ext.chars().count();
            if dot_idx > 0 && ext_len <= 16 {
                let keep = ext.to_string();
                let stem_chars = MAX_FILENAME_CHARS - ext_len;
                return base.chars().take(stem_chars).collect::<String>() + &keep;
            }
            base.chars().take(MAX_FILENAME_CHARS).collect()
        }
        None => base.chars().take(MAX_FILENAME_CHARS).collect(),
    }
}

/// Types of incoming messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum MessageType {
    #[default]
    Text,
    Location,
    Photo,
    Video,
    Audio,
    Voice,
    Document,
    Sticker,
    Command,
}

/// Result classification for message-processing lifecycle hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingOutcome {
    Success,
    Failure,
    Cancelled,
}

/// A supported messaging platform.
///
/// Hermes keeps a closed enum plus dynamic members for plugin platforms; the
/// Rust port keeps the open set as a validated lowercase string so bundled and
/// user plugins coexist without enum pollution.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Platform(String);

pub mod platform_names {
    pub const LOCAL: &str = "local";
    pub const TELEGRAM: &str = "telegram";
    pub const DISCORD: &str = "discord";
    pub const WHATSAPP: &str = "whatsapp";
    pub const WHATSAPP_CLOUD: &str = "whatsapp_cloud";
    pub const SLACK: &str = "slack";
    pub const SIGNAL: &str = "signal";
    pub const MATTERMOST: &str = "mattermost";
    pub const MATRIX: &str = "matrix";
    pub const EMAIL: &str = "email";
    pub const SMS: &str = "sms";
    pub const DINGTALK: &str = "dingtalk";
    pub const API_SERVER: &str = "api_server";
    pub const WEBHOOK: &str = "webhook";
    pub const MSGRAPH_WEBHOOK: &str = "msgraph_webhook";
    pub const FEISHU: &str = "feishu";
    pub const WECOM: &str = "wecom";
    pub const WECOM_CALLBACK: &str = "wecom_callback";
    pub const WEIXIN: &str = "weixin";
    pub const BLUEBUBBLES: &str = "bluebubbles";
    pub const QQBOT: &str = "qqbot";
    pub const YUANBAO: &str = "yuanbao";
    pub const RELAY: &str = "relay";
}

impl Platform {
    pub fn new(value: impl AsRef<str>) -> Option<Self> {
        let trimmed = value.as_ref().trim().to_lowercase();
        if trimmed.is_empty() {
            return None;
        }
        Some(Platform(trimmed))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn local() -> Self {
        Platform(platform_names::LOCAL.to_string())
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a message originated: routes responses, feeds the system-prompt
/// context block, and records origin for cron delivery.
///
/// Port of `SessionSource` (gateway/session.py). `scope_id`/`guild_id` are
/// mirrored both ways (scope_id wins on read) during the migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSource {
    pub platform: Platform,
    pub chat_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_name: Option<String>,
    #[serde(default = "default_chat_type")]
    pub chat_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id_alt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id_alt: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_bot: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    /// Deprecated alias kept for wire compatibility; both are written,
    /// `scope_id` wins on read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guild_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_chat_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub role_authorized: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Multiplex signal: an explicit profile route whose target is not served.
    /// Wire-invisible (never serialized).
    #[serde(skip)]
    pub profile_route_rejected: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_thread_created: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_thread_initial_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prospective_thread_id: Option<String>,
    /// Wire-invisible trust signal: came over the authenticated relay WebSocket.
    /// Never serialized so a peer cannot forge it.
    #[serde(skip)]
    pub delivered_via_upstream_relay: bool,
    /// Wire-invisible: key of a conversation its chat has moved past.
    #[serde(skip)]
    pub detached_route: Option<String>,
}

fn default_chat_type() -> String {
    "dm".to_string()
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl SessionSource {
    pub fn new(platform: Platform, chat_id: impl Into<String>) -> Self {
        Self {
            platform,
            chat_id: chat_id.into(),
            chat_name: None,
            chat_type: default_chat_type(),
            user_id: None,
            user_name: None,
            thread_id: None,
            chat_topic: None,
            user_id_alt: None,
            chat_id_alt: None,
            is_bot: false,
            scope_id: None,
            guild_id: None,
            parent_chat_id: None,
            message_id: None,
            role_authorized: false,
            profile: None,
            profile_route_rejected: false,
            auto_thread_created: false,
            auto_thread_initial_name: None,
            prospective_thread_id: None,
            delivered_via_upstream_relay: false,
            detached_route: None,
        }
    }

    /// Mirror `scope_id`/`guild_id` onto each other (scope_id wins) so readers
    /// of either field agree.
    pub fn normalize_scope(&mut self) {
        if self.scope_id.is_none() {
            self.scope_id = self.guild_id.clone();
        } else {
            self.guild_id = self.scope_id.clone();
        }
    }

    /// Human-readable description of the source ("DM with Alice",
    /// "channel: #general", "thread: …").
    pub fn description(&self) -> String {
        if self.platform == Platform::local() {
            return "CLI terminal".to_string();
        }
        let user = self
            .user_name
            .clone()
            .or_else(|| self.user_id.clone())
            .unwrap_or_else(|| "user".to_string());
        let chat = self
            .chat_name
            .clone()
            .unwrap_or_else(|| self.chat_id.clone());
        let base = match self.chat_type.as_str() {
            "dm" => format!("DM with {user}"),
            prefix @ ("group" | "channel") => format!("{prefix}: {chat}"),
            other => format!("{other}: {chat}"),
        };
        match &self.thread_id {
            Some(thread) => format!("{base}, thread: {thread}"),
            None => base,
        }
    }
}

/// Structured interactive-prompt reply (relay only): routed to the
/// approval/slash-confirm/clarify resolvers BEFORE dispatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptResponse {
    pub prompt_id: String,
    pub option_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_message_id: Option<String>,
}

/// Incoming message from a platform — the normalized shape all adapters produce.
///
/// Port of `MessageEvent` (event.py). Process-local admission receipts
/// (`_gateway_accepted` etc.) are deliberately not part of this struct; the
/// runner owns admission state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageEvent {
    pub text: String,
    #[serde(default)]
    pub message_type: MessageType,
    /// Author, mirrored from `source` for per-message prompt builders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    /// None only in isolated unit tests; production always sets it.
    pub source: SessionSource,
    /// Raw platform payload, kept for adapter-specific handling. Not serialized.
    #[serde(skip)]
    pub raw_message: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Delivery-ledger identity for the final send when it differs from `message_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger_message_id: Option<String>,
    /// Reply anchor for the final send when the answer is to a DIFFERENT message
    /// than the one that opened the turn. None = derive from the event as usual.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_anchor_override: Option<String>,
    /// Platform update id (Telegram `update_id`): lets `/restart` record it so the
    /// next gateway advances past it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_update_id: Option<i64>,
    /// Media attachments: local file paths (for vision tool access).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media_urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media_types: Vec<String>,
    /// Per-attachment text-inlining contract; None = legacy "text/* already inlined".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media_text_inlined: Vec<Option<bool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_author_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_author_name: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reply_to_is_own_message: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_response: Option<PromptResponse>,
    /// Auto-loaded skill(s) for topic/channel bindings; a single name or ordered list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_skill: Option<Vec<String>>,
    /// Per-channel ephemeral system prompt; applied at API call time, never persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_prompt: Option<String>,
    /// History-backfilled channel context; kept out of `text`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_context: Option<String>,
    /// Synthetic events (e.g. background-process notifications) that must bypass auth.
    #[serde(default, skip_serializing_if = "is_false")]
    pub internal: bool,
    /// Free-form per-event metadata; consumers must treat it as untyped.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
    /// May this event resolve gateway commands / control prompts? Proactive plugin
    /// events set False so untrusted payload text stays conversational.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub allow_gateway_control: bool,
    /// Was this message addressed to this bot? False lets a bare silence marker
    /// stand; None means unknown and keeps the visible fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_expected: Option<bool>,
    /// Synthetic turns that carry no channel inputs of their own reuse the session's
    /// pinned channel prompt instead of overwriting it with this event's None.
    #[serde(default, skip_serializing_if = "is_false")]
    pub inherit_channel_pin: bool,
    /// Identity of one edit of a message (Telegram: its `update_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_revision: Option<String>,
    /// Original filename per attachment, parallel to `media_urls` like `media_types`
    /// ("" = unknown). Display data only, never a path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media_filenames: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl MessageEvent {
    pub fn new(text: impl Into<String>, source: SessionSource) -> Self {
        Self {
            text: text.into(),
            message_type: MessageType::Text,
            user_id: None,
            user_name: None,
            source,
            raw_message: None,
            message_id: None,
            ledger_message_id: None,
            reply_anchor_override: None,
            platform_update_id: None,
            media_urls: Vec::new(),
            media_types: Vec::new(),
            media_text_inlined: Vec::new(),
            reply_to_message_id: None,
            reply_to_text: None,
            reply_to_author_id: None,
            reply_to_author_name: None,
            reply_to_is_own_message: false,
            prompt_response: None,
            auto_skill: None,
            channel_prompt: None,
            channel_context: None,
            internal: false,
            metadata: std::collections::BTreeMap::new(),
            allow_gateway_control: true,
            reply_expected: None,
            inherit_channel_pin: false,
            edit_revision: None,
            media_filenames: Vec::new(),
        }
    }

    /// One turn now answers `other` too: an addressed message wins, then an
    /// unknown one.
    pub fn absorb_reply_expected(&mut self, other: &MessageEvent) {
        if self.reply_expected != Some(true) && other.reply_expected != Some(false) {
            self.reply_expected = other.reply_expected;
        }
    }

    /// Append `other`'s attachments (urls, types and original names) to this
    /// event's. Names are padded with `""` on either side so each stays on its own
    /// attachment; events without names leave `media_filenames` untouched.
    pub fn extend_media(&mut self, other: &MessageEvent) {
        if !self.media_filenames.is_empty() || !other.media_filenames.is_empty() {
            fn pad(event: &MessageEvent) -> Vec<String> {
                let n = event.media_urls.len();
                let mut names: Vec<String> = event.media_filenames.clone();
                names.resize(n, String::new());
                names.truncate(n);
                names
            }
            self.media_filenames = pad(self).into_iter().chain(pad(other)).collect();
        }
        self.media_urls.extend(other.media_urls.iter().cloned());
        self.media_types.extend(other.media_types.iter().cloned());
    }

    fn command_text(&self) -> &str {
        strip_attachment_refs(&self.text)
    }

    /// Check if this is a command message (e.g., /new, /reset).
    pub fn is_command(&self) -> bool {
        self.allow_gateway_control && self.command_text().starts_with('/')
    }

    /// Extract command name if this is a command message.
    pub fn get_command(&self) -> Option<String> {
        if !self.is_command() {
            return None;
        }
        let raw = self
            .command_text()
            .split_whitespace()
            .next()?
            .strip_prefix('/')?
            .to_lowercase()
            .split('@')
            .next()?
            .to_string();
        if raw.contains('/') {
            return None; // valid command names never contain /
        }
        Some(raw)
    }

    /// Get the arguments after a command.
    pub fn get_command_args(&self) -> String {
        if !self.is_command() {
            return self.text.clone();
        }
        let parts: Vec<&str> = self.command_text().splitn(2, char::is_whitespace).collect();
        let args = parts.get(1).copied().unwrap_or("").trim_start();
        // iOS auto-corrects -- to — (em dash) and - to – (en dash)
        args.replace("\u{2014}\u{2014}", "--")
            .replace('\u{2014}', "--")
            .replace('\u{2013}', "-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(chat_type: &str, chat_id: &str) -> SessionSource {
        SessionSource {
            chat_type: chat_type.to_string(),
            ..SessionSource::new(Platform::new("telegram").unwrap(), chat_id)
        }
    }

    #[test]
    fn clean_attachment_filename_strips_paths_and_format_chars() {
        assert_eq!(clean_attachment_filename("/tmp/a/b.png"), "b.png");
        assert_eq!(clean_attachment_filename("C:\\tmp\\c.png"), "c.png");
        assert_eq!(clean_attachment_filename("no\u{202E}sus.txt"), "nosus.txt");
        assert_eq!(clean_attachment_filename(""), "");
        assert_eq!(clean_attachment_filename(".."), "");
        let long = "x".repeat(300) + ".jpeg";
        let cleaned = clean_attachment_filename(&long);
        assert_eq!(cleaned.chars().count(), MAX_FILENAME_CHARS);
        assert!(cleaned.ends_with(".jpeg"));
    }

    #[test]
    fn command_detection_strips_attachment_refs() {
        let src = source("dm", "1");
        let mut event = MessageEvent::new("@image:/tmp/foo.png\n/reset", src);
        assert!(event.is_command());
        assert_eq!(event.get_command(), Some("reset".to_string()));
        event.allow_gateway_control = false;
        assert!(!event.is_command());
        assert_eq!(event.get_command(), None);
    }

    #[test]
    fn command_args_normalize_dashes() {
        let src = source("dm", "1");
        let event = MessageEvent::new("/queue first \u{2014} second \u{2013} third", src);
        assert_eq!(event.get_command_args(), "first -- second - third");
    }

    #[test]
    fn platform_names_match_hermes() {
        assert_eq!(platform_names::TELEGRAM, "telegram");
        assert_eq!(Platform::new(" Telegram ").unwrap().as_str(), "telegram");
        assert_eq!(Platform::new("   "), None);
    }

    #[test]
    fn extend_media_pads_names() {
        let src = source("dm", "1");
        let mut a = MessageEvent::new("a", src.clone());
        a.media_urls = vec!["/cache/a1".into(), "/cache/a2".into()];
        a.media_types = vec!["image/png".into(), "image/jpeg".into()];
        a.media_filenames = vec!["one.png".into()];
        let mut b = MessageEvent::new("b", src);
        b.media_urls = vec!["/cache/b1".into()];
        b.media_types = vec!["image/gif".into()];
        a.extend_media(&b);
        assert_eq!(a.media_urls.len(), 3);
        assert_eq!(a.media_filenames, vec!["one.png", "", ""]);
    }
}
