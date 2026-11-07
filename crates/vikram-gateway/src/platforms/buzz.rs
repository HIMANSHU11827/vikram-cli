//! Buzz adapter — Block's Nostr-relay community, spoken as signed NIP-01 events over one
//! NIP-42-authenticated WebSocket.
//!
//! Rust port of `research/hermes-platforms/buzz/` (`adapter.py`, `nostr_auth.py`,
//! `buzz_cursors.py`, `send_targets.py`, `plugin.yaml`). The reference reaches the relay in two
//! ways: outbound through the `buzz` CLI ("JSON in, JSON out", never a shell) and inbound through
//! a NIP-42-authenticated WebSocket subscription with a CLI poll fallback. This port keeps only
//! the WebSocket and speaks the protocol directly — the socket is injected as a
//! [`crate::socket::LineSocket`] (a [`crate::socket::WsSocket`] in production), so every frame is
//! a text JSON line and no subprocess is involved.
//!
//! # Ported behaviour (with the reference it comes from)
//!
//! - **Key handling** (`nostr_auth.py`): [`BuzzKeypair::from_private_key`] accepts `BUZZ_PRIVATE_KEY`
//!   as 64 hex characters or an `nsec1…` bech32 string (`decode_private_key`, lines 70–84), rejects
//!   mixed case, a wrong hrp, a bad checksum, a non-bech32 character, non-zero padding and a length
//!   other than 32 bytes (`_decode_nsec`, lines 39–67), and derives the x-only public key
//!   (`public_key_hex`, lines 119–123) whose `npub1…` form is the display identity
//!   (`hex_to_npub`, adapter.py:247–260). The secret is never logged: [`BuzzKeypair`]'s `Debug`
//!   prints only the npub, and no error string ever carries key material.
//! - **Events** (NIP-01 as the reference signs them, `build_auth_event` lines 163–177): the id is
//!   SHA-256 over the compact serialization `[0,pubkey,created_at,kind,tags,content]`
//!   ([`NostrEvent::id_material`]), and the signature is BIP-340 (`schnorr_sign`, lines 126–148)
//!   through `k256`'s [`k256::schnorr::SigningKey`] instead of the reference's hand-rolled curve
//!   math. Inbound events are checked before dispatch — id recomputed and signature verified with
//!   [`k256::schnorr::VerifyingKey`] — and an event with a wrong-length id/sig/pubkey, a mismatched
//!   id or an invalid signature is dropped ([`NostrEvent::verify`]).
//! - **NIP-42 AUTH** ([`authenticate`]): await `["AUTH", challenge]`, answer with a signed kind-22242
//!   event tagged `["relay", url]` + `["challenge", challenge]` (+ the optional NIP-OA `BUZZ_AUTH_TAG`
//!   four-string `["auth", …]` tag, `parse_auth_tag` lines 151–160), then wait for
//!   `["OK", id, true, …]`; `false`, `NOTICE`/`CLOSED` and a silent relay are all failures
//!   (`_authenticate_websocket`, adapter.py:1003–1036, with `_WS_AUTH_TIMEOUT = 20 s`, line 143).
//! - **Subscription** ([`BuzzAdapter::connect`]): one `["REQ", "vikram-buzz-<n>", filter]` per watched
//!   channel with the reference's filter — `kinds` = the dispatch kinds, `#h` = the channel, and
//!   `since` = the cursor's high-water mark − 1 (same-second overlap de-duped by id) or `limit` =
//!   [`BUZZ_FETCH_LIMIT`] when the channel has no mark yet (`_send_channel_subscription`,
//!   adapter.py:1039–1055). `EOSE` is recorded per subscription, and a `CLOSED` whose reason is
//!   permanent (`restricted`, `not a channel member`, `auth-required`) drops that channel while any
//!   other `CLOSED` fails the stream so the host reconnects (`_handle_ws_message`, adapter.py:1185–1210).
//! - **Dispatch** ([`BuzzAdapter::plan_event`]): kinds 9 (chat), 45001 (forum post) and 45003 (forum
//!   comment) are dispatched and every other kind only advances the cursor (`_CHAT_KIND` line 74,
//!   `_DISPATCH_KINDS` line 81, `_handle_event` lines 1444–1470). Order is the reference's: id/dup
//!   check → cursor advance → kind filter → self-echo skip → DM latch → mention/p-tag/reply-to-us
//!   gate → allow-list → leading-mention strip → message. DM detection is
//!   `_is_direct_message_event` (lines 1527–1536) unchanged: kind 9, from someone else, `p`-tagged to
//!   us, text *not* mentioning us, in a conversation whose metadata still allows it
//!   (`_may_reclassify_as_dm`, lines 1516–1521: name `DM`, empty description, missing metadata fails
//!   closed) — and the latch sticks (`_maybe_latch_dm`, lines 1537–1543). Mention gating is
//!   `_is_mentioned` / `_is_addressed` (lines 1545–1560) with the reference's hex/npub/`@name`
//!   boundaries; the leading `@mention` is stripped so `@Vikram /whoami` is still a command
//!   (`_strip_mention`, lines 1562–1574). De-duplication and cursors are [`ChannelCursor`] /
//!   [`BuzzCursorStore`] (`buzz_cursors.py`: `last_ts`, `seen` capped at [`BUZZ_SEEN_CAP`], the file
//!   keyed by `identity` + `relay` so another identity's cursors are ignored, lines 21–55).
//! - **Text extraction and media markers**: `content` verbatim, leading mention stripped, NIP-94
//!   `imeta` tags parsed with the reference's structural limits ([`parse_imeta_attachments`],
//!   adapter.py:1344–1386 — https-only URL, 64-hex `x` digest, `0 < size ≤ 20 MiB`, at most 4
//!   attachments, malformed/over-limit ones counted and reported with the reference's
//!   `_attachment_rejection_note`, line 1388) and each accepted attachment surfaced as a marker line.
//!   The message type follows the attachment mime (`image/*` → photo, `audio/*` → audio,
//!   `video/*` → video, mixed/other → document).
//! - **Reactions**: `send_reaction` (`["EVENT", …]` kind 7) is the confirming variant; the reference's
//!   👀 "seen" acknowledgement after dispatch and its 👀 for a `reaction_only_users` sender that
//!   p-tags us with a real mention (`_handle_event` line 1476, `_dispatch_message` tail) are published
//!   best-effort, exactly as the reference logs their failures instead of raising.
//! - **Sending** ([`BuzzAdapter::send`]): publish kind 9 with `["h", channel]`, the NIP-10 reply tags
//!   the reference *reads* (`_event_reply_parent_id` lines 483–496: `e` with the relay hint at index 2
//!   and the `root`/`reply` marker at index 3; a thread reply joins the trigger's root instead of
//!   nesting, `_resolve_reply_anchor` lines 1634–1636) and a `p` tag for the parent author when it is
//!   known. Content is chunked at [`BUZZ_MAX_MESSAGE_LENGTH`] and each chunk's `OK` is awaited; a
//!   `false` is a failed [`SendResult`], never a delivered message.
//! - **Cursors across a restart** ([`BuzzAdapter::cursors`] / [`BuzzAdapter::restore_cursors`]): the
//!   persisted cursor is what closes the restart gap — reseeding from current history would mark
//!   everything that arrived while the gateway was down as already seen (`_load_cursors` /
//!   `_cursor_payload`, buzz_cursors.py:21–55). The host restores before [`BuzzAdapter::connect`] so
//!   the first `REQ` already uses the saved `since`.
//!
//! # Deliberate omissions (honest scope, not stubbed)
//!
//! Nothing below is faked; each item is absent rather than simulated, so no caller can mistake a
//! no-op for a delivery.
//!
//! - **The `buzz` CLI** (`_exec_buzz`, `_run_cli`, `_cli_error_message`, `_parse_send_receipt`,
//!   `_run_message_send`'s four recovery rungs, `edit_message`, `delete_message`, `send_typing`,
//!   `_standalone_send`, `interactive_setup`): every operation goes over the injected WebSocket
//!   instead. Consequences: `edit_message` / `delete_message` keep the trait defaults (unsupported),
//!   and the CLI-only recoveries — retrying without `--mention` when a mentioned pubkey left the
//!   channel, escaping an unresolvable `@token` with a zero-width separator, and the final
//!   `--mention <self>` downgrade — have no counterpart here.
//! - **Mention ``p``-tags on send**: the reference resolves `@Name` to member pubkeys through
//!   `channels members` + `users get` (`_mention_pubkeys_for`, `_channel_member_pubkeys`,
//!   `_profile_display_name`) so a genuine mention notifies. No member directory exists without the
//!   CLI, so an `@Name` stays plain prose and only the reply parent's author is `p`-tagged.
//! - **Inbound media download**: `_download_attachment` (NIP-94 size + SHA-256 verification, cached
//!   through `cache_media_bytes_async`) and `_localize_inbound_media` (same-relay `/media/<sha64>`
//!   URLs localized via `buzz media get`, `_find_relay_media_refs` / `_replace_media_refs`, lines
//!   179–222) are not ported. Nothing is fetched: `media_urls` stays empty, an accepted `imeta`
//!   attachment becomes a marker line naming its filename/mime/size, and a same-relay media URL stays
//!   in the text as a plain link.
//! - **The poll transport and DM discovery**: `_poll_loop`, `_poll_channel`, `_seed_channel`,
//!   `_discover_dms`, `_adopt_conversation`, the `dms list` fallback and the live kind-44100
//!   membership subscription (`_WS_MEMBERSHIP_KIND`, `_WS_MEMBERSHIP_SUB_ID`) are absent. The watch
//!   set is exactly the channels the host configures, and DM-shaped conversations are recognized
//!   from the same `channels list` metadata the reference's `_may_reclassify_as_dm` reads — supplied
//!   as [`BuzzChannel`] rather than fetched.
//! - **Process-level policy**: `check_requirements` / `validate_config` / `is_connected`, the
//!   YAML/env bridge, the profile secret scope, `_acquire_platform_lock`, the fatal-error map, the
//!   reconnect/backoff loop, the deferred cursor write lock, the serial-receive burst barrier and
//!   `GatewayRunner` admission all stay with the host; `crate::access` / `crate::pairing` own
//!   admission here. This adapter owns the wire protocol, its cursors and its own truthful failures.
//! - **`reply_to_mode` / `reply_in_thread`**: the reference's post-flat option is a CLI flag; here a
//!   `reply_to` anchor is always honored as an NIP-10 reply.
//!
//! # Divergence worth stating
//!
//! The reference signs every event with fresh auxiliary randomness (`secrets.token_bytes(32)`,
//! `schnorr_sign` line 135), so its signatures are not reproducible. This port signs with the
//! deterministic zero auxiliary randomness that `k256` uses (`sign_prehash` → `sign_raw` with
//! `aux_rand[0] == 0`), which is the BIP-340 test-vector mode: any signature this adapter produces is
//! byte-for-byte reproducible, and the golden vectors in the tests are generated from the reference
//! with `auxiliary_randomness = 32 × 0x00`.

use async_trait::async_trait;
use k256::schnorr::signature::hazmat::{PrehashSigner, PrehashVerifier};
use k256::schnorr::{Signature, SigningKey, VerifyingKey};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{ChatInfo, PlatformAdapter, SendResult};
use crate::socket::{LineSocket, SharedLineSocket, WsSocket};

// ---------------------------------------------------------------------------
// Protocol constants (adapter.py:74–160, nostr_auth.py, buzz_cursors.py)
// ---------------------------------------------------------------------------

/// The platform name this adapter reports (`Platform("buzz")`).
pub const BUZZ_PLATFORM: &str = "buzz";

/// Environment variable holding the identity's secret (hex or `nsec1…`); the only Buzz secret.
pub const BUZZ_PRIVATE_KEY_ENV: &str = "BUZZ_PRIVATE_KEY";

/// Environment variable holding the relay base URL (reference `BUZZ_RELAY_URL`).
pub const BUZZ_RELAY_URL_ENV: &str = "BUZZ_RELAY_URL";

/// Environment variable holding the optional NIP-OA owner-attestation tag JSON (`BUZZ_AUTH_TAG`).
pub const BUZZ_AUTH_TAG_ENV: &str = "BUZZ_AUTH_TAG";

/// Kind of a chat message (`_CHAT_KIND`, adapter.py:74).
pub const BUZZ_CHAT_KIND: u64 = 9;

/// Kind of a Buzz forum post, i.e. a thread root (`_DISPATCH_KINDS`, adapter.py:81).
pub const BUZZ_FORUM_POST_KIND: u64 = 45_001;

/// Kind of a comment on a forum post (`_DISPATCH_KINDS`, adapter.py:81).
pub const BUZZ_FORUM_COMMENT_KIND: u64 = 45_003;

/// Kinds that carry conversation content and are dispatched; everything else is housekeeping and
/// only advances the cursor (`_DISPATCH_KINDS`, adapter.py:81).
pub const BUZZ_DISPATCH_KINDS: [u64; 3] = [
    BUZZ_CHAT_KIND,
    BUZZ_FORUM_POST_KIND,
    BUZZ_FORUM_COMMENT_KIND,
];

/// Kind of a NIP-42 client-authentication event (`build_auth_event`, nostr_auth.py:163–177).
pub const BUZZ_AUTH_KIND: u64 = 22_242;

/// Kind of a NIP-25 reaction (`send_reaction`, adapter.py:859–868).
pub const BUZZ_REACTION_KIND: u64 = 7;

/// The emoji the reference publishes as its "seen" acknowledgement (adapter.py:1758).
pub const BUZZ_SEEN_EMOJI: &str = "\u{1f440}";

/// Events per subscription replay when a conversation has no high-water mark (`_FETCH_LIMIT`,
/// adapter.py:99).
pub const BUZZ_FETCH_LIMIT: usize = 50;

/// Per-channel de-duplication bound (`_SEEN_CAP`, buzz_cursors.py:10).
pub const BUZZ_SEEN_CAP: usize = 500;

/// Bound on remembered NIP-10 thread roots (`_THREAD_ROOT_CACHE`, adapter.py:1607).
pub const BUZZ_THREAD_ROOT_CACHE: usize = 512;

/// Inbound attachments considered per event (`_MAX_INBOUND_ATTACHMENTS`, adapter.py:108).
pub const BUZZ_MAX_ATTACHMENTS: usize = 4;

/// Total inbound attachment bytes the reference is willing to fetch (`_MAX_INBOUND_ATTACHMENT_BYTES`).
pub const BUZZ_MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// How long the relay may take to send its AUTH challenge or answer our AUTH event
/// (`_WS_AUTH_TIMEOUT`, adapter.py:143).
pub const BUZZ_AUTH_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the relay may take to answer a published event with `OK`. The reference delegates the
/// wait to the CLI (`_CLI_TIMEOUT`, adapter.py:103); this is the same order of magnitude.
pub const BUZZ_SEND_OK_TIMEOUT: Duration = Duration::from_secs(30);

/// Single-message character budget. Buzz itself documents no cap; the reference inherits
/// `BasePlatformAdapter.max_message_length` (4096 code points), so a longer reply is chunked here.
pub const BUZZ_MAX_MESSAGE_LENGTH: usize = 4096;

/// Length of a hex-encoded SHA-256 / x-only public key / event id.
const HEX32_LEN: usize = 64;

/// Length of a hex-encoded BIP-340 signature.
const SIG_HEX_LEN: usize = 128;

/// The bech32 charset (BIP-173), `nostr_auth.py:18`.
const BECH32_CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// The bech32 checksum generators, `nostr_auth.py:19`.
const BECH32_GENERATORS: [u32; 5] = [
    0x3B6A_57B2,
    0x2650_8E6D,
    0x1EA1_19FA,
    0x3D42_33DD,
    0x2A14_62B3,
];

/// Suffix substrings of a permanent `CLOSED` reason (`_handle_ws_message`, adapter.py:1195).
const PERMANENT_CLOSE_MARKERS: [&str; 3] = ["restricted", "not a channel member", "auth-required"];

/// Sender of a frame that failed every NIP-01 rule; used in dropped-event accounting only.
const CLOSED_REASON_FALLBACK: &str = "subscription closed";

// ---------------------------------------------------------------------------
// Small wire helpers: hex, bech32, user references
// ---------------------------------------------------------------------------

/// Lowercase hex, no allocation beyond the output (`bytes.hex()`).
fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Decode a hex string of exactly `len` bytes; `None` for any other length or a non-hex character.
fn hex_decode_exact(value: &str, len: usize) -> Option<Vec<u8>> {
    if value.len() != len * 2 {
        return None;
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(len);
    let nibble = |ch: u8| -> Option<u8> {
        match ch {
            b'0'..=b'9' => Some(ch - b'0'),
            b'a'..=b'f' => Some(ch - b'a' + 10),
            b'A'..=b'F' => Some(ch - b'A' + 10),
            _ => None,
        }
    };
    let mut nibbles = bytes.iter();
    while let (Some(high), Some(low)) = (nibbles.next(), nibbles.next()) {
        out.push((nibble(*high)? << 4) | nibble(*low)?);
    }
    Some(out)
}

/// True for exactly 64 lowercase hex characters — the reference's `_HEX64_RE` (adapter.py:86).
fn is_hex64(value: &str) -> bool {
    value.len() == HEX32_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(data: &[u8]) -> String {
    hex_encode(digest(&SHA256, data).as_ref())
}

/// BIP-173 polymod over 5-bit values (`_bech32_polymod`, nostr_auth.py:24–32).
fn bech32_polymod(values: &[u8]) -> u32 {
    let mut checksum: u32 = 1;
    for value in values {
        let top = checksum >> 25;
        checksum = ((checksum & 0x1ff_ffff) << 5) ^ u32::from(*value);
        for (index, generator) in BECH32_GENERATORS.iter().enumerate() {
            if (top >> index) & 1 == 1 {
                checksum ^= generator;
            }
        }
    }
    checksum
}

/// BIP-173 hrp expansion (`_bech32_hrp_expand`, nostr_auth.py:35–36).
fn bech32_hrp_expand(hrp: &str) -> Vec<u8> {
    let chars: Vec<u8> = hrp.bytes().collect();
    let mut out: Vec<u8> = chars.iter().map(|byte| byte >> 5).collect();
    out.push(0);
    out.extend(chars.iter().map(|byte| byte & 31));
    out
}

/// Re-group 8-bit bytes into 5-bit bech32 values (`_convertbits(data, 8, 5)`).
fn convert_bits_8_to_5(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().div_ceil(5) * 8);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for byte in data {
        accumulator = (accumulator << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(((accumulator >> bits) & 31) as u8);
        }
    }
    if bits > 0 {
        out.push(((accumulator << (5 - bits)) & 31) as u8);
    }
    out
}

/// Re-group 5-bit values into 8-bit bytes, rejecting non-zero padding (`_decode_nsec`, lines 55–64).
fn convert_bits_5_to_8(data: &[u8], label: &'static str) -> Result<Vec<u8>, BuzzError> {
    let mut out = Vec::with_capacity(data.len() * 5 / 8);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for value in data {
        accumulator = (accumulator << 5) | u32::from(*value);
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    if bits > 0 && accumulator & ((1 << bits) - 1) != 0 {
        return Err(BuzzError::bech32(label, "payload has non-zero padding"));
    }
    Ok(out)
}

/// Encode a bech32 string with a 32-byte payload (`hex_to_npub`, adapter.py:247–260 generalised).
fn bech32_encode_32(hrp: &str, payload: &[u8; 32]) -> String {
    let data = convert_bits_8_to_5(payload);
    let mut values = bech32_hrp_expand(hrp);
    values.extend_from_slice(&data);
    values.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let polymod = bech32_polymod(&values) ^ 1;
    let mut out = String::with_capacity(hrp.len() + 1 + data.len() + 6);
    out.push_str(hrp);
    out.push('1');
    for value in &data {
        out.push(BECH32_CHARSET[*value as usize] as char);
    }
    for index in 0..6 {
        out.push(BECH32_CHARSET[((polymod >> (5 * (5 - index))) & 31) as usize] as char);
    }
    out
}

/// Decode a bech32 string whose payload must be 32 bytes, enforcing the hrp and every bech32 rule
/// the reference enforces (`_decode_nsec`, nostr_auth.py:39–67).
fn bech32_decode_32(value: &str, expected_hrp: &'static str) -> Result<[u8; 32], BuzzError> {
    if value.is_empty() {
        return Err(BuzzError::bech32(expected_hrp, "value is empty"));
    }
    let has_lower = value.chars().any(|ch| ch.is_ascii_lowercase());
    let has_upper = value.chars().any(|ch| ch.is_ascii_uppercase());
    if has_lower && has_upper {
        return Err(BuzzError::bech32(
            expected_hrp,
            "cannot mix upper- and lowercase",
        ));
    }
    let normalized = value.to_ascii_lowercase();
    let Some(separator) = normalized.rfind('1') else {
        return Err(BuzzError::bech32(expected_hrp, "has no separator"));
    };
    if separator < 1 || separator + 7 > normalized.len() {
        return Err(BuzzError::bech32(expected_hrp, "is not a valid encoding"));
    }
    let (hrp, rest) = normalized.split_at(separator);
    if hrp != expected_hrp {
        return Err(BuzzError::bech32(expected_hrp, "uses the wrong prefix"));
    }
    let data: Vec<u8> = rest[1..]
        .bytes()
        .map(
            |byte| match BECH32_CHARSET.iter().position(|known| *known == byte) {
                Some(index) => Ok(index as u8),
                None => Err(BuzzError::bech32(expected_hrp, "has an invalid character")),
            },
        )
        .collect::<Result<_, _>>()?;
    let mut checksum_input = bech32_hrp_expand(hrp);
    checksum_input.extend_from_slice(&data);
    if bech32_polymod(&checksum_input) != 1 {
        return Err(BuzzError::bech32(expected_hrp, "has an invalid checksum"));
    }
    let payload = convert_bits_5_to_8(&data[..data.len().saturating_sub(6)], expected_hrp)?;
    let payload: [u8; 32] = payload.try_into().map_err(|_| {
        BuzzError::bech32(expected_hrp, "must encode a value that is exactly 32 bytes")
    })?;
    Ok(payload)
}

/// Encode a 64-hex public key as `npub1…` (`hex_to_npub`, adapter.py:247–260); `None` for anything
/// that is not 32 hex-encoded bytes.
pub fn pubkey_to_npub(pubkey_hex: &str) -> Option<String> {
    let bytes = hex_decode_exact(pubkey_hex, 32)?;
    let payload: [u8; 32] = bytes.try_into().ok()?;
    Some(bech32_encode_32("npub", &payload))
}

/// Decode an `npub1…` string to a lowercase hex public key (`npub_to_hex`, gateway/authz_mixin;
/// used by the allow-list normalizer, adapter.py:288–296).
pub fn npub_to_pubkey_hex(npub: &str) -> Option<String> {
    bech32_decode_32(npub.trim(), "npub")
        .ok()
        .map(|bytes| hex_encode(&bytes))
}

/// Normalize a user reference — a hex pubkey or an `npub` — to lowercase hex, rejecting everything
/// else (`_normalize_user_ref`, adapter.py:288–296).
pub fn normalize_user_ref(reference: &str) -> Option<String> {
    let lowered = reference.trim().to_lowercase();
    if lowered.is_empty() {
        return None;
    }
    if lowered.starts_with("npub1") {
        return npub_to_pubkey_hex(&lowered);
    }
    is_hex64(&lowered).then_some(lowered)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every way this adapter can fail. Failure text never contains key material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuzzError {
    /// A bech32 string violated the encoding rules; `label` names the expected prefix.
    #[error("{label}: {detail}")]
    Bech32 {
        /// The prefix that was expected (`nsec`, `npub`).
        label: &'static str,
        /// What was wrong with the encoding.
        detail: &'static str,
    },
    /// The private key is neither 64 hex characters nor a valid `nsec`.
    #[error("private key must be 64 hex characters or nsec")]
    KeyFormat,
    /// The private key is not exactly 32 bytes.
    #[error("private key must be 32 bytes")]
    KeyLength,
    /// The private key is zero or at least the secp256k1 curve order.
    #[error("private key is outside the secp256k1 range")]
    KeyRange,
    /// The relay URL is not `http(s)://` or `ws(s)://`.
    #[error("buzz relay url must be http(s) or ws(s): {0}")]
    RelayUrl(String),
    /// An inbound value is not a NIP-01 event of the shape this adapter can process.
    #[error("buzz event is not a valid nostr event: {0}")]
    Event(&'static str),
    /// The event's `id` is not the SHA-256 of its canonical serialization.
    #[error("buzz event id does not match its content")]
    EventIdMismatch,
    /// The BIP-340 signature does not verify against the event's pubkey.
    #[error("buzz event signature is invalid")]
    EventSignature,
    /// A tag list the caller supplied is not a NIP-OA owner-attestation tag.
    #[error("BUZZ_AUTH_TAG must be a four-string auth tag")]
    AuthTag,
    /// The socket itself failed.
    #[error("buzz socket: {0}")]
    Socket(String),
    /// The relay did not answer within the configured bound.
    #[error("buzz relay did not answer in time: {0}")]
    Timeout(&'static str),
    /// NIP-42 authentication failed; the message is the relay's reason.
    #[error("buzz relay rejected NIP-42 AUTH: {0}")]
    AuthRejected(String),
    /// The relay refused a published event; the message is its reason.
    #[error("buzz relay rejected the event: {0}")]
    PublishRejected(String),
    /// The relay closed a subscription for a reason that is not permanent for that channel.
    #[error("buzz relay closed subscription {subscription}: {reason}")]
    SubscriptionClosed {
        /// The subscription id the relay closed.
        subscription: String,
        /// The reason the relay gave.
        reason: String,
    },
    /// A frame is not a JSON array (only the AUTH path treats this as fatal; elsewhere it is skipped).
    #[error("buzz relay frame is not a nostr message")]
    MalformedFrame,
}

impl BuzzError {
    fn bech32(label: &'static str, detail: &'static str) -> Self {
        Self::Bech32 { label, detail }
    }
}

// ---------------------------------------------------------------------------
// Identity: nsec/hex → x-only pubkey → npub
// ---------------------------------------------------------------------------

/// The signing identity: a BIP-340 key pair plus its display forms.
///
/// The secret is never rendered: `Debug` prints the npub only, and no error string carries key
/// material (reference `_resolve_private_key`: "NEVER log it").
#[derive(Clone)]
pub struct BuzzKeypair {
    signing: SigningKey,
    pubkey_hex: String,
    npub: String,
}

impl std::fmt::Debug for BuzzKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuzzKeypair")
            .field("npub", &self.npub)
            .finish_non_exhaustive()
    }
}

impl BuzzKeypair {
    /// Derive the identity from `BUZZ_PRIVATE_KEY`-shaped input: 64 hex characters or `nsec1…`
    /// (`decode_private_key`, nostr_auth.py:70–84).
    pub fn from_private_key(raw: &str) -> Result<Self, BuzzError> {
        let secret = decode_private_key(raw)?;
        let signing = SigningKey::from_slice(&secret).map_err(|_| BuzzError::KeyRange)?;
        let pubkey_bytes = signing.verifying_key().to_bytes();
        let pubkey_hex = hex_encode(pubkey_bytes.as_slice());
        let payload: &[u8; 32] = (&pubkey_bytes).into();
        let npub = bech32_encode_32("npub", payload);
        Ok(Self {
            signing,
            pubkey_hex,
            npub,
        })
    }

    /// Derive the identity from the process environment's `BUZZ_PRIVATE_KEY`.
    pub fn from_env() -> Result<Self, BuzzError> {
        match std::env::var(BUZZ_PRIVATE_KEY_ENV) {
            Ok(value) if !value.trim().is_empty() => Self::from_private_key(&value),
            _ => Err(BuzzError::KeyFormat),
        }
    }

    /// The 64-character lowercase hex x-only public key: what `p` tags and allow-lists compare.
    pub fn public_key_hex(&self) -> &str {
        &self.pubkey_hex
    }

    /// The `npub1…` display form of [`Self::public_key_hex`].
    pub fn npub(&self) -> &str {
        &self.npub
    }

    /// The 32 raw public-key bytes.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        if let Some(bytes) = hex_decode_exact(&self.pubkey_hex, 32) {
            out.copy_from_slice(&bytes);
        }
        out
    }

    /// BIP-340-sign a 32-byte message hash, returning the 64 signature bytes (`schnorr_sign`,
    /// nostr_auth.py:126–148). Deterministic: zero auxiliary randomness.
    pub fn sign_hash(&self, hash: &[u8; 32]) -> Result<[u8; 64], BuzzError> {
        let signature: Signature = self
            .signing
            .sign_prehash(hash)
            .map_err(|_| BuzzError::EventSignature)?;
        Ok(signature.to_bytes())
    }
}

/// The secp256k1 group order `n = 2^256 − 2^32 − 977`, big-endian (`CURVE_ORDER`, nostr_auth.py:13) —
/// the exclusive upper bound a private key must stay under.
const CURVE_ORDER_BE: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
];

/// Decode `BUZZ_PRIVATE_KEY`-shaped input into 32 raw bytes, then apply the reference's
/// `1 ≤ key < CURVE_ORDER` range check (`decode_private_key`, nostr_auth.py:70–84).
fn decode_private_key(raw: &str) -> Result<[u8; 32], BuzzError> {
    let trimmed = raw.trim();
    let bytes: [u8; 32] = if trimmed.to_ascii_lowercase().starts_with("nsec1") {
        bech32_decode_32(trimmed, "nsec")?
    } else {
        hex_decode_exact(trimmed, 32)
            .ok_or(BuzzError::KeyFormat)?
            .try_into()
            .map_err(|_| BuzzError::KeyLength)?
    };
    let zero = bytes.iter().all(|byte| *byte == 0);
    if zero || bytes >= CURVE_ORDER_BE {
        return Err(BuzzError::KeyRange);
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Nostr events
// ---------------------------------------------------------------------------

/// One NIP-01 event. Tags keep the reference's shape: a list of lists whose values are strings
/// (NIP-01 requires strings; a non-string tag value makes an event unparseable rather than silently
/// coerced, because coercing would change its id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NostrEvent {
    /// 64-hex SHA-256 of [`Self::id_material`].
    pub id: String,
    /// 64-hex x-only public key of the signer.
    pub pubkey: String,
    /// Unix seconds the event was created.
    pub created_at: u64,
    /// Event kind: 9 chat, 45001/45003 forum, 22242 AUTH, 7 reaction.
    pub kind: u64,
    /// Tag list, e.g. `[["h", "<channel>"], ["p", "<pubkey>"]]`.
    pub tags: Vec<Vec<String>>,
    /// Event body; `""` for AUTH.
    pub content: String,
    /// 128-hex BIP-340 signature over the event id.
    pub sig: String,
}

impl NostrEvent {
    /// The canonical serialization the id is computed over: compact
    /// `[0,pubkey,created_at,kind,tags,content]`, matching
    /// `json.dumps(..., separators=(",", ":"), ensure_ascii=False)` (nostr_auth.py:172).
    pub fn id_material(
        pubkey: &str,
        created_at: u64,
        kind: u64,
        tags: &[Vec<String>],
        content: &str,
    ) -> String {
        let material = json!([0, pubkey, created_at, kind, tags, content]);
        serde_json::to_string(&material).unwrap_or_default()
    }

    /// Parse an event out of a relay `EVENT` frame payload.
    pub fn from_value(value: &Value) -> Result<Self, BuzzError> {
        let object = value.as_object().ok_or(BuzzError::Event("not an object"))?;
        let text = |key: &'static str| -> Result<String, BuzzError> {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or(BuzzError::Event(match key {
                    "id" => "id is not a string",
                    "pubkey" => "pubkey is not a string",
                    "sig" => "sig is not a string",
                    _ => "content is not a string",
                }))
        };
        let id = text("id")?;
        let pubkey = text("pubkey")?;
        let content = text("content")?;
        let sig = text("sig")?;
        let created_at = object
            .get("created_at")
            .and_then(Value::as_u64)
            .ok_or(BuzzError::Event("created_at is not an unsigned integer"))?;
        let kind = object
            .get("kind")
            .and_then(Value::as_u64)
            .ok_or(BuzzError::Event("kind is not an unsigned integer"))?;
        let mut tags: Vec<Vec<String>> = Vec::new();
        if let Some(raw_tags) = object.get("tags") {
            let array = raw_tags
                .as_array()
                .ok_or(BuzzError::Event("tags is not an array"))?;
            for tag in array {
                let elements = tag
                    .as_array()
                    .ok_or(BuzzError::Event("a tag is not an array"))?;
                let mut values = Vec::with_capacity(elements.len());
                for element in elements {
                    values.push(
                        element
                            .as_str()
                            .ok_or(BuzzError::Event("a tag value is not a string"))?
                            .to_string(),
                    );
                }
                tags.push(values);
            }
        }
        Ok(Self {
            id,
            pubkey,
            created_at,
            kind,
            tags,
            content,
            sig,
        })
    }

    /// The wire JSON form (`{"id":…,"pubkey":…,…}`).
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "pubkey": self.pubkey,
            "created_at": self.created_at,
            "kind": self.kind,
            "tags": self.tags,
            "content": self.content,
            "sig": self.sig,
        })
    }

    /// Recompute the id this event's content implies.
    pub fn computed_id(&self) -> String {
        sha256_hex(
            Self::id_material(
                &self.pubkey,
                self.created_at,
                self.kind,
                &self.tags,
                &self.content,
            )
            .as_bytes(),
        )
    }

    /// Verify an inbound event before it is dispatched: field lengths, then the id recomputation,
    /// then the BIP-340 signature.
    pub fn verify(&self) -> Result<(), BuzzError> {
        if !is_hex64(&self.id.to_ascii_lowercase()) {
            return Err(BuzzError::Event("id is not 64 hex characters"));
        }
        if !is_hex64(&self.pubkey.to_ascii_lowercase()) {
            return Err(BuzzError::Event("pubkey is not 64 hex characters"));
        }
        if self.sig.len() != SIG_HEX_LEN || !self.sig.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(BuzzError::Event("sig is not 128 hex characters"));
        }
        if self.computed_id() != self.id.to_ascii_lowercase() {
            return Err(BuzzError::EventIdMismatch);
        }
        let pubkey = hex_decode_exact(&self.pubkey, 32)
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .ok_or(BuzzError::Event("pubkey is not 32 bytes"))?;
        let verifying = VerifyingKey::from_bytes(&pubkey.into())
            .map_err(|_| BuzzError::Event("pubkey is not a valid x-only key"))?;
        let sig_bytes = hex_decode_exact(&self.sig, 64)
            .and_then(|bytes| <[u8; 64]>::try_from(bytes).ok())
            .ok_or(BuzzError::Event("sig is not 64 bytes"))?;
        let signature = Signature::from_bytes(&sig_bytes).map_err(|_| BuzzError::EventSignature)?;
        let hash = hex_decode_exact(&self.id, 32)
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .ok_or(BuzzError::Event("id is not 32 bytes"))?;
        verifying
            .verify_prehash(&hash, &signature)
            .map_err(|_| BuzzError::EventSignature)
    }

    /// Every well-formed `["e", target, relay, marker]` tag as `(target, marker)` with raw values
    /// (`_e_tags`, adapter.py:474–481).
    fn e_tags(&self) -> impl Iterator<Item = (&str, &str)> {
        self.tags.iter().filter_map(|tag| {
            if tag.len() >= 2 && tag[0] == "e" {
                Some((
                    tag[1].as_str(),
                    tag.get(3).map(String::as_str).unwrap_or_default(),
                ))
            } else {
                None
            }
        })
    }

    /// Direct parent id from NIP-10 `e` tags: the `reply` marker, then `root`, then the last `e`
    /// (`_event_reply_parent_id`, adapter.py:483–496). Markers are compared case-sensitively here,
    /// exactly as the reference does.
    pub fn reply_parent_id(&self) -> Option<String> {
        let mut reply = None;
        let mut root = None;
        let mut last = None;
        for (target, marker) in self.e_tags() {
            if target.trim().is_empty() {
                continue;
            }
            last = Some(target);
            match marker {
                "reply" => reply = Some(target),
                "root" => root = Some(target),
                _ => {}
            }
        }
        reply.or(root).or(last).map(str::to_string)
    }

    /// The NIP-10 thread root of this event, or `None` when it is top-level
    /// (`_extract_thread_root`, adapter.py:1610–1622). Markers are matched case-insensitively here.
    pub fn thread_root(&self) -> Option<String> {
        let mut root = None;
        let mut reply = None;
        for (target, marker) in self.e_tags() {
            match marker.to_ascii_lowercase().as_str() {
                "root" => root = Some(target),
                "reply" => reply = Some(target),
                "" if reply.is_none() => reply = Some(target),
                _ => {}
            }
        }
        root.or(reply).map(str::to_string)
    }

    /// True when a `p` tag equals `pubkey` (`_p_tagged`, adapter.py:498–503).
    pub fn p_tagged(&self, pubkey: &str) -> bool {
        self.tags
            .iter()
            .any(|tag| tag.len() > 1 && tag[0] == "p" && tag[1].eq_ignore_ascii_case(pubkey))
    }

    /// The channel this event belongs to, from its `h` tag (what the reference subscribes with).
    pub fn channel_tag(&self) -> Option<&str> {
        self.tags
            .iter()
            .find(|tag| tag.len() > 1 && tag[0] == "h")
            .map(|tag| tag[1].as_str())
    }
}

/// Build and sign an event (`build_auth_event`'s signing path, nostr_auth.py:163–177).
pub fn sign_event(
    keys: &BuzzKeypair,
    kind: u64,
    tags: Vec<Vec<String>>,
    content: &str,
    created_at: u64,
) -> Result<NostrEvent, BuzzError> {
    let pubkey = keys.public_key_hex().to_string();
    let id =
        sha256_hex(NostrEvent::id_material(&pubkey, created_at, kind, &tags, content).as_bytes());
    let hash: [u8; 32] = hex_decode_exact(&id, 32)
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or(BuzzError::Event("computed id is not 32 bytes"))?;
    let sig = hex_encode(&keys.sign_hash(&hash)?);
    Ok(NostrEvent {
        id,
        pubkey,
        created_at,
        kind,
        tags,
        content: content.to_string(),
        sig,
    })
}

// ---------------------------------------------------------------------------
// NIP-42 authentication
// ---------------------------------------------------------------------------

/// The `ws(s)://` form of a configured relay URL (`_websocket_url`, adapter.py:976–982):
/// `http` → `ws`, `https` → `wss`, an already-`ws(s)` URL kept, anything else refused.
pub fn relay_websocket_url(relay_url: &str) -> Result<String, BuzzError> {
    let trimmed = relay_url.trim();
    let (scheme, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| BuzzError::RelayUrl(trimmed.to_string()))?;
    let mapped = match scheme.to_ascii_lowercase().as_str() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => return Err(BuzzError::RelayUrl(other.to_string())),
    };
    if rest.is_empty() || rest.starts_with('/') {
        return Err(BuzzError::RelayUrl(trimmed.to_string()));
    }
    // Drop the fragment; keep path and query like the reference's urlunsplit.
    let authority_and_rest = rest.split('#').next().unwrap_or(rest);
    Ok(format!("{mapped}://{authority_and_rest}"))
}

/// Validate a NIP-OA owner-attestation tag (`parse_auth_tag`, nostr_auth.py:151–160): exactly four
/// strings whose first element is `auth`, given as JSON text or as an already-parsed tag.
pub fn parse_auth_tag(raw: &Value) -> Result<Vec<String>, BuzzError> {
    let value = match raw {
        Value::String(text) => serde_json::from_str(text).map_err(|_| BuzzError::AuthTag)?,
        other => other.clone(),
    };
    let array = value.as_array().ok_or(BuzzError::AuthTag)?;
    if array.len() != 4 || array.first().and_then(Value::as_str) != Some("auth") {
        return Err(BuzzError::AuthTag);
    }
    array
        .iter()
        .map(|element| {
            element
                .as_str()
                .map(str::to_string)
                .ok_or(BuzzError::AuthTag)
        })
        .collect()
}

/// Build the kind-22242 AUTH event for `challenge` (`build_auth_event`, nostr_auth.py:163–177):
/// `["relay", url]` + `["challenge", challenge]`, then the optional NIP-OA tag.
pub fn build_auth_event(
    keys: &BuzzKeypair,
    challenge: &str,
    relay_url: &str,
    auth_tag: Option<&[String]>,
    created_at: u64,
) -> Result<NostrEvent, BuzzError> {
    if let Some(tag) = auth_tag
        && (tag.len() != 4 || tag.first().map(String::as_str) != Some("auth"))
    {
        return Err(BuzzError::AuthTag);
    }
    let mut tags = vec![
        vec!["relay".to_string(), relay_url.to_string()],
        vec!["challenge".to_string(), challenge.to_string()],
    ];
    if let Some(tag) = auth_tag {
        tags.push(tag.to_vec());
    }
    sign_event(keys, BUZZ_AUTH_KIND, tags, "", created_at)
}

/// Read one line, mapping a silent or closed socket onto a truthful error.
async fn read_line(
    socket: &dyn LineSocket,
    timeout: Duration,
    what: &'static str,
) -> Result<String, BuzzError> {
    match tokio::time::timeout(timeout, socket.next_line()).await {
        Err(_) => Err(BuzzError::Timeout(what)),
        Ok(Err(error)) => Err(BuzzError::Socket(error.to_string())),
        Ok(Ok(None)) => Err(BuzzError::Socket(format!(
            "connection closed while waiting for {what}"
        ))),
        Ok(Ok(Some(line))) => Ok(line),
    }
}

fn parse_json_frame(line: &str) -> Result<Vec<Value>, BuzzError> {
    let value: Value = serde_json::from_str(line).map_err(|_| BuzzError::MalformedFrame)?;
    value.as_array().cloned().ok_or(BuzzError::MalformedFrame)
}

fn frame_label(frame: &[Value]) -> &str {
    frame.first().and_then(Value::as_str).unwrap_or_default()
}

fn last_string(frame: &[Value]) -> String {
    frame
        .iter()
        .rev()
        .find_map(Value::as_str)
        .unwrap_or(CLOSED_REASON_FALLBACK)
        .to_string()
}

/// The peer's NIP-42 handshake (`_authenticate_websocket`, adapter.py:1003–1036).
///
/// Reads the relay's `["AUTH", challenge]`, sends `["AUTH", signed 22242 event]` and waits for
/// `["OK", id, true, …]`. A different first frame, `OK` with `false`, `NOTICE`/`CLOSED`, silence
/// (`timeout`) or a closed socket are all failures — nothing here reports an unauthenticated
/// connection as authenticated. Returns the accepted event.
pub async fn authenticate(
    socket: &dyn LineSocket,
    keys: &BuzzKeypair,
    relay_url: &str,
    auth_tag: Option<&[String]>,
    created_at: u64,
    timeout: Duration,
) -> Result<NostrEvent, BuzzError> {
    let challenge_line = read_line(socket, timeout, "NIP-42 challenge").await?;
    let challenge_frame = parse_json_frame(&challenge_line)?;
    if frame_label(&challenge_frame) != "AUTH" || challenge_frame.len() < 2 {
        return Err(BuzzError::AuthRejected(
            "relay did not send a NIP-42 AUTH challenge".to_string(),
        ));
    }
    let challenge = challenge_frame[1]
        .as_str()
        .ok_or_else(|| BuzzError::AuthRejected("AUTH challenge is not a string".to_string()))?;
    let event = build_auth_event(keys, challenge, relay_url, auth_tag, created_at)?;
    let frame = json!(["AUTH", event.to_value()]);
    socket
        .send_line(&serde_json::to_string(&frame).unwrap_or_default())
        .await
        .map_err(|error| BuzzError::Socket(error.to_string()))?;
    loop {
        let line = read_line(socket, timeout, "NIP-42 OK").await?;
        let response = match parse_json_frame(&line) {
            Ok(response) => response,
            Err(_) => continue,
        };
        match frame_label(&response) {
            "OK" if response.len() >= 4 && response[1].as_str() == Some(event.id.as_str()) => {
                if response[2].as_bool() == Some(true) {
                    return Ok(event);
                }
                return Err(BuzzError::AuthRejected(last_string(&response)));
            }
            "NOTICE" | "CLOSED" => {
                return Err(BuzzError::AuthRejected(last_string(&response)));
            }
            _ => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// Clock seam (deterministic timestamps for reproducible events)
// ---------------------------------------------------------------------------

/// Source of `created_at` seconds. The adapter consults it for every event it signs.
pub trait BuzzClock: Send + Sync {
    /// Unix seconds.
    fn now_unix(&self) -> u64;
}

/// Wall-clock time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl BuzzClock for SystemClock {
    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0)
    }
}

/// A clock pinned to one instant, so a test can predict the event id a publish will produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedClock(u64);

impl FixedClock {
    /// Pin the clock to `unix_seconds`.
    pub fn new(unix_seconds: u64) -> Self {
        Self(unix_seconds)
    }
}

impl BuzzClock for FixedClock {
    fn now_unix(&self) -> u64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// One watched conversation, carrying the `channels list` metadata the reference reads.
///
/// `name` and `description` are what `_may_reclassify_as_dm` (adapter.py:1516–1521) inspects: a
/// relay-materialized DM is named `DM` with an empty description. Missing metadata fails closed as a
/// group, which is why a [`BuzzChannel`] always states both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuzzChannel {
    /// Channel UUID as it appears in `#h` tags and subscription filters.
    pub id: String,
    /// Human name; `DM` marks the relay's direct-message conversations.
    pub name: String,
    /// Channel description; a DM has none.
    pub description: String,
}

impl BuzzChannel {
    /// A channel named after its id, with no description (treated as a group).
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            description: String::new(),
        }
    }

    /// A channel with an explicit name and description.
    pub fn with_metadata(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
        }
    }

    /// `_may_reclassify_as_dm`: metadata does not rule a DM out (`DM` + empty description).
    fn may_reclassify_as_dm(&self) -> bool {
        self.name.trim() == "DM" && self.description.trim().is_empty()
    }
}

/// Everything the adapter needs besides the socket and the identity.
#[derive(Debug, Clone)]
pub struct BuzzConfig {
    /// Relay base URL (`https://…`, `wss://…`, …); the host dials [`BuzzAdapter::websocket_url`].
    pub relay_url: String,
    /// Conversations to watch, in subscription order. Empty means nothing is subscribed.
    pub channels: Vec<BuzzChannel>,
    /// Optional NIP-OA owner-attestation tag added to the AUTH event (`BUZZ_AUTH_TAG`).
    pub auth_tag: Option<Vec<String>>,
    /// The agent's own display name, as `buzz users get` reports it; enables `@name` gating.
    pub display_name: Option<String>,
    /// Channel messages must address the agent unless this is off; DMs always dispatch.
    pub require_mention: bool,
    /// Hex pubkeys allowed to command the agent; empty means no adapter-level filter.
    pub allowed_pubkeys: Vec<String>,
    /// Hex pubkeys that get a 👀 on an addressed message but are never dispatched.
    pub reaction_only_pubkeys: Vec<String>,
    /// Publish the reference's 👀 acknowledgement after dispatching an event.
    pub send_seen_reaction: bool,
    /// Code-point budget per published message.
    pub max_message_length: usize,
    /// Replay bound for a conversation with no cursor yet (`limit` in the REQ filter).
    pub fetch_limit: usize,
    /// Per-channel de-duplication bound.
    pub seen_cap: usize,
    /// Bound on the relay's silence during AUTH.
    pub auth_timeout: Duration,
    /// Bound on the relay's silence while awaiting `OK` after a publish.
    pub send_ok_timeout: Duration,
}

impl BuzzConfig {
    /// Defaults for every knob but the relay URL; mirror `adapter.py:99–160` and `plugin.yaml`.
    pub fn new(relay_url: impl Into<String>) -> Self {
        Self {
            relay_url: relay_url.into(),
            channels: Vec::new(),
            auth_tag: None,
            display_name: None,
            require_mention: true,
            allowed_pubkeys: Vec::new(),
            reaction_only_pubkeys: Vec::new(),
            send_seen_reaction: true,
            max_message_length: BUZZ_MAX_MESSAGE_LENGTH,
            fetch_limit: BUZZ_FETCH_LIMIT,
            seen_cap: BUZZ_SEEN_CAP,
            auth_timeout: BUZZ_AUTH_TIMEOUT,
            send_ok_timeout: BUZZ_SEND_OK_TIMEOUT,
        }
    }

    /// Watch `channel` in addition to whatever is already configured.
    pub fn with_channel(mut self, channel: BuzzChannel) -> Self {
        self.channels.push(channel);
        self
    }

    /// The agent's own display name, used for `@name` mention gating.
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// Require an address in channels (default true).
    pub fn requiring_mention(mut self, required: bool) -> Self {
        self.require_mention = required;
        self
    }

    /// Restrict dispatch to these pubkeys (hex or npub; entries that are neither are dropped).
    pub fn allowing(mut self, references: &[&str]) -> Self {
        self.allowed_pubkeys = references
            .iter()
            .filter_map(|reference| normalize_user_ref(reference))
            .collect();
        self
    }

    /// Pubkeys that receive a 👀 but are never dispatched (hex or npub).
    pub fn reacting_only(mut self, references: &[&str]) -> Self {
        self.reaction_only_pubkeys = references
            .iter()
            .filter_map(|reference| normalize_user_ref(reference))
            .collect();
        self
    }

    /// Publish the 👀 acknowledgement after each dispatched event (default true).
    pub fn acknowledging_receipts(mut self, enabled: bool) -> Self {
        self.send_seen_reaction = enabled;
        self
    }

    /// The NIP-OA auth tag, given as its JSON text (`BUZZ_AUTH_TAG`).
    pub fn with_auth_tag(mut self, raw: &str) -> Result<Self, BuzzError> {
        let tag = parse_auth_tag(&Value::String(raw.to_string()))?;
        self.auth_tag = Some(tag);
        Ok(self)
    }

    /// Build the non-secret half of the configuration from the reference's `BUZZ_*` environment
    /// surface (`_YAML_BRIDGE`, adapter.py:1103–1113). `BUZZ_PRIVATE_KEY` is deliberately not read
    /// here — the identity comes from [`BuzzKeypair::from_env`].
    pub fn from_env() -> Self {
        let relay = std::env::var(BUZZ_RELAY_URL_ENV).unwrap_or_default();
        let mut config = Self::new(relay);
        if let Ok(list) = std::env::var("BUZZ_CHANNELS") {
            config.channels = list
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(BuzzChannel::new)
                .collect();
        }
        if let Ok(list) = std::env::var("BUZZ_ALLOWED_USERS") {
            config.allowed_pubkeys = list.split(',').filter_map(normalize_user_ref).collect();
        }
        if let Ok(list) = std::env::var("BUZZ_REACTION_ONLY_USERS") {
            config.reaction_only_pubkeys = list.split(',').filter_map(normalize_user_ref).collect();
        }
        if let Ok(flag) = std::env::var("BUZZ_REQUIRE_MENTION") {
            config.require_mention = !matches!(
                flag.trim().to_ascii_lowercase().as_str(),
                "false" | "0" | "no" | "off"
            );
        }
        if let Ok(tag) = std::env::var(BUZZ_AUTH_TAG_ENV)
            && !tag.trim().is_empty()
            && let Ok(tag) = parse_auth_tag(&Value::String(tag))
        {
            config.auth_tag = Some(tag);
        }
        config
    }
}

// ---------------------------------------------------------------------------
// Cursors (`buzz_cursors.py`)
// ---------------------------------------------------------------------------

/// One conversation's persisted receive cursor (`buzz_cursors.py` channel entry).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuzzCursor {
    /// `"dm"` or `"group"`, as the reference stores its classified chat type.
    #[serde(default)]
    pub chat_type: String,
    /// High-water mark: the newest `created_at` seen, used as `since` on the next subscription.
    #[serde(default)]
    pub last_ts: u64,
    /// Recently seen event ids, oldest first, capped at the adapter's `seen_cap`.
    #[serde(default)]
    pub seen: Vec<String>,
}

/// The cursor file: every watched conversation's cursor, keyed to one identity on one relay so
/// another identity's file is ignored (`_cursor_payload` / `_load_cursors`, buzz_cursors.py:21–55).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuzzCursorStore {
    /// The hex pubkey these cursors belong to.
    #[serde(default)]
    pub identity: String,
    /// The relay URL these cursors were collected from.
    #[serde(default)]
    pub relay: String,
    /// Conversation id → cursor.
    #[serde(default)]
    pub channels: BTreeMap<String, BuzzCursor>,
}

impl BuzzCursorStore {
    /// Parse a persisted cursor document; `None` when it is not the expected shape.
    pub fn from_json(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }

    /// The wire form, ready for the host to write atomically to its own state directory.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// True when the document belongs to `identity` on `relay` — the reference's guard against
    /// replaying another identity's de-dupe state, where ids collide.
    pub fn belongs_to(&self, identity: &str, relay: &str) -> bool {
        self.identity == identity && self.relay == relay
    }
}

/// Which chat class a conversation latched to. Latched DMs stay DMs (`_maybe_latch_dm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatType {
    Direct,
    Group,
}

impl ChatType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "dm",
            Self::Group => "group",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "dm" => Some(Self::Direct),
            "group" => Some(Self::Group),
            _ => None,
        }
    }
}

/// Author and content snippet remembered for NIP-10 reply-parent resolution
/// (`_remember_event`, adapter.py:1638–1644).
#[derive(Debug, Clone, PartialEq, Eq)]
struct EventMeta {
    id: String,
    author: String,
    snippet: String,
}

/// One conversation's live receive state (`_new_channel_state`, adapter.py:1244).
#[derive(Debug, Clone)]
struct ChannelCursor {
    chat_type: ChatType,
    last_ts: u64,
    seen: VecDeque<String>,
    seen_set: HashSet<String>,
    meta: VecDeque<EventMeta>,
}

impl ChannelCursor {
    fn new(chat_type: ChatType) -> Self {
        Self {
            chat_type,
            last_ts: 0,
            seen: VecDeque::new(),
            seen_set: HashSet::new(),
            meta: VecDeque::new(),
        }
    }

    /// Record an event id as seen, trimming the oldest beyond `cap` (`_trim_seen`, adapter.py:1588–1595).
    fn mark_seen(&mut self, event_id: &str, cap: usize) {
        if self.seen_set.insert(event_id.to_string()) {
            self.seen.push_back(event_id.to_string());
        }
        while self.seen.len() > cap {
            if let Some(evicted) = self.seen.pop_front() {
                self.seen_set.remove(&evicted);
            }
        }
    }

    /// Remember an event's author and snippet, keeping the newest `cap` entries.
    fn remember(&mut self, event: &NostrEvent, cap: usize) {
        if event.id.is_empty() {
            return;
        }
        self.meta.retain(|entry| entry.id != event.id);
        self.meta.push_back(EventMeta {
            id: event.id.clone(),
            author: event.pubkey.to_ascii_lowercase(),
            snippet: event.content.chars().take(500).collect(),
        });
        while self.meta.len() > cap {
            self.meta.pop_front();
        }
    }

    /// Author + snippet for `event_id` (`_lookup_event_meta`, adapter.py:1662–1668).
    fn lookup(&self, event_id: &str) -> Option<&EventMeta> {
        self.meta.iter().find(|entry| entry.id == event_id)
    }
}

// ---------------------------------------------------------------------------
// Mentions (`_is_mentioned` / `_strip_mention`)
// ---------------------------------------------------------------------------

/// What identifying forms the agent can be addressed by.
struct Identity {
    pubkey_hex: String,
    npub: String,
    display_name: String,
}

impl Identity {
    fn new(keys: &BuzzKeypair, config: &BuzzConfig) -> Self {
        Self {
            pubkey_hex: keys.public_key_hex().to_string(),
            npub: keys.npub().to_string(),
            display_name: config
                .display_name
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_string(),
        }
    }

    /// True when `content` carries `needle` with neither neighbour satisfying `is_boundary`
    /// (the reference's `(?<![…])needle(?![…])`).
    fn bounded_contains(lowered: &str, needle: &str, is_boundary: fn(char) -> bool) -> bool {
        if needle.is_empty() {
            return false;
        }
        for (index, _) in lowered.match_indices(needle) {
            let before_ok = match lowered[..index].chars().next_back() {
                None => true,
                Some(ch) => !is_boundary(ch),
            };
            let after_ok = match lowered[index + needle.len()..].chars().next() {
                None => true,
                Some(ch) => !is_boundary(ch),
            };
            if before_ok && after_ok {
                return true;
            }
        }
        false
    }
}

fn is_hex_boundary(ch: char) -> bool {
    ch.is_ascii_hexdigit()
}

fn is_alnum_boundary(ch: char) -> bool {
    ch.is_alphanumeric()
}

/// True for a character that may follow an `@display name` mention (the reference's
/// `(?=$|[\s,;.!?:)\]}])`).
fn is_name_terminator(ch: char) -> bool {
    ch.is_whitespace() || ",;.!?:)]}".contains(ch)
}

/// `_is_mentioned` (adapter.py:1545–1555): the text addresses the agent by hex pubkey, npub or
/// `@display name`. `\w` in the reference's lookbehind is approximated by [`char::is_alphanumeric`].
fn is_mentioned(content: &str, identity: &Identity) -> bool {
    let lowered = content.to_lowercase();
    if is_hex64(&identity.pubkey_hex)
        && Identity::bounded_contains(&lowered, &identity.pubkey_hex, is_hex_boundary)
    {
        return true;
    }
    if !identity.npub.is_empty()
        && Identity::bounded_contains(&lowered, &identity.npub.to_lowercase(), is_alnum_boundary)
    {
        return true;
    }
    if !identity.display_name.is_empty() {
        let needle = format!("@{}", identity.display_name.to_lowercase());
        for (index, _) in lowered.match_indices(&needle) {
            let before_ok = match lowered[..index].chars().next_back() {
                None => true,
                Some(ch) => !(ch.is_alphanumeric() || ch == '_' || ch == '@'),
            };
            let after_ok = match lowered[index + needle.len()..].chars().next() {
                None => true,
                Some(ch) => is_name_terminator(ch),
            };
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

/// `_strip_mention` (adapter.py:1562–1576): remove a *leading* mention of the agent so
/// `@Vikram /whoami` is still a command; mid-sentence mentions stay.
fn strip_leading_mention(content: &str, identity: &Identity) -> String {
    let text = content.trim();
    let lowered = text.to_lowercase();
    let mut candidates: Vec<String> = Vec::new();
    if !identity.display_name.is_empty() {
        candidates.push(format!("@{}", identity.display_name.to_lowercase()));
    }
    if !identity.npub.is_empty() {
        candidates.push(identity.npub.to_lowercase());
    }
    if !identity.pubkey_hex.is_empty() {
        candidates.push(identity.pubkey_hex.clone());
    }
    for (position, candidate) in candidates.iter().enumerate() {
        let is_display_name = position == 0 && !identity.display_name.is_empty();
        let at_optional = !is_display_name;
        let matched_len = if lowered.starts_with(candidate) {
            candidate.len()
        } else if at_optional && lowered.starts_with(&format!("@{candidate}")) {
            candidate.len() + 1
        } else {
            continue;
        };
        // The reference requires the token to end at a boundary: a name at a terminator, an
        // npub/hex at a non-alphanumeric / non-hex character.
        let after_ok = match lowered[matched_len..].chars().next() {
            None => true,
            Some(ch) if is_display_name => is_name_terminator(ch),
            Some(ch) if position == 1 => !ch.is_alphanumeric(),
            Some(ch) => !ch.is_ascii_hexdigit(),
        };
        if !after_ok {
            continue;
        }
        let rest = text[matched_len..]
            .trim_start_matches(|ch: char| ch.is_whitespace() || ch == ':' || ch == ',');
        return rest.trim().to_string();
    }
    text.to_string()
}

// ---------------------------------------------------------------------------
// NIP-94 attachment metadata → media markers
// ---------------------------------------------------------------------------

/// One structurally valid `imeta` attachment (`_parse_imeta_attachments`, adapter.py:1344–1386).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImetaAttachment {
    url: String,
    filename: String,
    mime_type: String,
    size: u64,
}

impl ImetaAttachment {
    /// The mime prefix decides the message type the reference assigns from the cached media kind
    /// (`_ATTACHMENT_KIND_TYPES`, adapter.py:508).
    fn message_type(&self) -> MessageType {
        if self.mime_type.starts_with("image/") {
            MessageType::Photo
        } else if self.mime_type.starts_with("audio/") {
            MessageType::Audio
        } else if self.mime_type.starts_with("video/") {
            MessageType::Video
        } else {
            MessageType::Document
        }
    }

    /// A display-safe basename (`_safe_attachment_filename`, adapter.py:114–128): no directory part,
    /// no control characters, never `.`/`..`.
    fn safe_filename(&self) -> String {
        let base = self
            .filename
            .replace('\\', "/")
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|ch| !ch.is_control())
            .collect::<String>();
        let base = base.trim().to_string();
        if base.is_empty() || base == "." || base == ".." {
            "attachment.bin".to_string()
        } else {
            base.chars().take(120).collect()
        }
    }
}

/// True for an `https://` URL with an authority and no user info or fragment — the reference's
/// `urlsplit` checks (`_parse_imeta_attachments`, adapter.py:1362–1375). No URL crate is involved,
/// so only the structural properties the reference rejects are checked.
fn is_https_attachment_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    if rest.contains('#') {
        return false;
    }
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let host = authority.split(':').next().unwrap_or_default();
    !host.is_empty() && !url.contains(char::is_whitespace) && !url.starts_with("https://-")
}

/// Parse `imeta` tags into accepted attachments and a count of rejected ones
/// (`_parse_imeta_attachments`, adapter.py:1344–1386).
fn parse_imeta_attachments(tags: &[Vec<String>]) -> (Vec<ImetaAttachment>, usize) {
    let mut accepted: Vec<ImetaAttachment> = Vec::new();
    let mut rejected = 0usize;
    let mut total_declared = 0u64;
    for tag in tags {
        if tag.first().map(String::as_str) != Some("imeta") {
            continue;
        }
        if accepted.len() >= BUZZ_MAX_ATTACHMENTS {
            rejected += 1;
            continue;
        }
        let mut fields: HashMap<&str, &str> = HashMap::new();
        for field in tag.iter().skip(1) {
            if let Some((key, value)) = field.split_once(' ') {
                fields.entry(key).or_insert(value.trim());
            }
        }
        let url = fields.get("url").copied().unwrap_or_default();
        let sha256 = fields.get("x").copied().unwrap_or_default().to_lowercase();
        let size = match fields
            .get("size")
            .copied()
            .unwrap_or_default()
            .parse::<u64>()
        {
            Ok(size) => size,
            Err(_) => {
                rejected += 1;
                continue;
            }
        };
        if !is_https_attachment_url(url)
            || !is_hex64(&sha256)
            || size == 0
            || size > BUZZ_MAX_ATTACHMENT_BYTES
            || total_declared + size > BUZZ_MAX_ATTACHMENT_BYTES
        {
            rejected += 1;
            continue;
        }
        total_declared += size;
        accepted.push(ImetaAttachment {
            url: url.to_string(),
            filename: fields
                .get("filename")
                .copied()
                .unwrap_or_default()
                .to_string(),
            mime_type: fields
                .get("m")
                .copied()
                .unwrap_or_default()
                .chars()
                .take(255)
                .collect(),
            size,
        });
    }
    (accepted, rejected)
}

/// The reference's fixed-width diagnostic for discarded metadata (`_attachment_rejection_note`,
/// adapter.py:1388–1391).
fn attachment_rejection_note(rejected: usize) -> String {
    format!("[{rejected} Buzz attachment(s) rejected as malformed or over limits.]")
}

/// Marker text for attachments this gateway recognized but does not fetch (see the module header).
fn attachment_marker(attachments: &[ImetaAttachment]) -> String {
    let names: Vec<String> = attachments
        .iter()
        .map(|attachment| {
            let name = attachment.safe_filename();
            if attachment.mime_type.is_empty() {
                format!("{name} ({} bytes)", attachment.size)
            } else {
                format!(
                    "{name} ({}, {} bytes)",
                    attachment.mime_type, attachment.size
                )
            }
        })
        .collect();
    format!(
        "[{} Buzz attachment(s) not fetched by this gateway: {}]",
        attachments.len(),
        names.join(", ")
    )
}

// ---------------------------------------------------------------------------
// Frames and the dispatch plan
// ---------------------------------------------------------------------------

/// One parsed relay frame this adapter understands.
#[derive(Debug, Clone, PartialEq)]
enum Frame {
    /// `["EVENT", subscription_id, event]`.
    Event {
        subscription: String,
        event: Box<NostrEvent>,
    },
    /// `["EOSE", subscription_id]`.
    Eose { subscription: String },
    /// `["CLOSED", subscription_id, reason]`.
    Closed {
        subscription: String,
        reason: String,
    },
    /// `["NOTICE", message]`.
    Notice(String),
    /// `["OK", event_id, accepted, message]`.
    Ok {
        event_id: String,
        accepted: bool,
        message: String,
    },
    /// Anything else (`AUTH` mid-stream, `COUNT`, …).
    Ignored,
}

/// Parse one text frame (NIP-01 relay-to-client message).
fn parse_frame(line: &str) -> Result<Frame, BuzzError> {
    let frame = parse_json_frame(line)?;
    let label = frame_label(&frame).to_string();
    match label.as_str() {
        "EVENT" if frame.len() >= 3 => {
            let subscription = frame[1]
                .as_str()
                .ok_or(BuzzError::Event("subscription id is not a string"))?
                .to_string();
            let event = NostrEvent::from_value(&frame[2])?;
            Ok(Frame::Event {
                subscription,
                event: Box::new(event),
            })
        }
        "EOSE" if frame.len() >= 2 => Ok(Frame::Eose {
            subscription: frame[1].as_str().unwrap_or_default().to_string(),
        }),
        "CLOSED" if frame.len() >= 2 => Ok(Frame::Closed {
            subscription: frame[1].as_str().unwrap_or_default().to_string(),
            reason: if frame.len() > 2 {
                last_string(&frame[2..])
            } else {
                CLOSED_REASON_FALLBACK.to_string()
            },
        }),
        "NOTICE" => Ok(Frame::Notice(
            frame
                .last()
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )),
        "OK" if frame.len() >= 3 => Ok(Frame::Ok {
            event_id: frame[1].as_str().unwrap_or_default().to_string(),
            accepted: frame[2].as_bool() == Some(true),
            message: frame
                .get(3)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        _ => Ok(Frame::Ignored),
    }
}

/// What dispatching one verified event decided.
enum DispatchPlan {
    /// Nothing to deliver (duplicate, housekeeping kind, own echo, or gated out).
    Skip,
    /// Deliver this event.
    Deliver(Box<Deliverable>),
    /// Publish a 👀 without dispatching (the `reaction_only_users` path).
    React {
        event_id: String,
        emoji: &'static str,
    },
}

/// Everything the dispatch computed under the lock.
struct Deliverable {
    message: Box<MessageEvent>,
    event_id: String,
}

// ---------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------

/// Adapter state that the wire path mutates: cursors, subscriptions, thread roots, the inbox.
struct Runtime {
    cursors: HashMap<String, ChannelCursor>,
    subscriptions: Vec<(String, String)>,
    threads: VecDeque<(String, Option<String>)>,
    inbox: VecDeque<MessageEvent>,
    eose: HashSet<String>,
    restricted: Vec<String>,
    connected: bool,
    last_error: Option<String>,
    last_notice: Option<String>,
    rejected_events: u64,
}

impl Runtime {
    fn new() -> Self {
        Self {
            cursors: HashMap::new(),
            subscriptions: Vec::new(),
            threads: VecDeque::new(),
            inbox: VecDeque::new(),
            eose: HashSet::new(),
            restricted: Vec::new(),
            connected: false,
            last_error: None,
            last_notice: None,
            rejected_events: 0,
        }
    }
}

/// Buzz adapter: Nostr events over one injected line socket.
///
/// The host owns the socket, the dial and the reconnect loop; [`BuzzAdapter::connect`] performs the
/// NIP-42 handshake and the `REQ` sweep on the socket it is given, [`BuzzAdapter::next_event`] reads
/// and dispatches, [`BuzzAdapter::send`] publishes kind 9 and awaits the relay's `OK`. A cursor
/// snapshot is available through [`BuzzAdapter::cursors`] and is restored with
/// [`BuzzAdapter::restore_cursors`] before `connect`.
pub struct BuzzAdapter {
    socket: SharedLineSocket,
    keys: BuzzKeypair,
    identity: Identity,
    config: BuzzConfig,
    clock: Arc<dyn BuzzClock>,
    runtime: Mutex<Runtime>,
}

impl std::fmt::Debug for BuzzAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuzzAdapter")
            .field("relay", &self.config.relay_url)
            .field("identity", &self.identity.npub)
            .field("channels", &self.config.channels.len())
            .finish_non_exhaustive()
    }
}

impl BuzzAdapter {
    /// Build an adapter over `socket` for `keys` and `config`.
    pub fn new(socket: SharedLineSocket, keys: BuzzKeypair, config: BuzzConfig) -> Self {
        let identity = Identity::new(&keys, &config);
        let mut adapter = Self {
            socket,
            keys,
            identity,
            config,
            clock: Arc::new(SystemClock),
            runtime: Mutex::new(Runtime::new()),
        };
        adapter.seed_channels();
        adapter
    }

    /// Build an adapter from `BUZZ_PRIVATE_KEY` and the `BUZZ_*` environment surface.
    pub fn from_env(socket: SharedLineSocket) -> Result<Self, BuzzError> {
        let keys = BuzzKeypair::from_env()?;
        Ok(Self::new(socket, keys, BuzzConfig::from_env()))
    }

    /// Dial [`BuzzAdapter::websocket_url`] with a real [`crate::socket::WsSocket`] and build the
    /// adapter over it. The connection is the socket's; AUTH and the `REQ` sweep still belong to
    /// [`BuzzAdapter::connect`], which the host calls next (or to
    /// [`BuzzAdapter::connect_session`] directly).
    pub fn dial(keys: BuzzKeypair, config: BuzzConfig) -> Result<Self, BuzzError> {
        let url = relay_websocket_url(&config.relay_url)?;
        let socket =
            WsSocket::connect(&url).map_err(|error| BuzzError::Socket(error.to_string()))?;
        Ok(Self::new(socket.into_shared(), keys, config))
    }

    /// Replace the clock (deterministic `created_at` for tests and replays).
    pub fn with_clock(mut self, clock: Arc<dyn BuzzClock>) -> Self {
        self.clock = clock;
        self
    }

    /// The URL the host must dial (`_websocket_url`, adapter.py:976–982).
    pub fn websocket_url(&self) -> Result<String, BuzzError> {
        relay_websocket_url(&self.config.relay_url)
    }

    /// The identity's `npub` — safe to log; the secret is not.
    pub fn npub(&self) -> &str {
        self.keys.npub()
    }

    /// The identity's hex public key.
    pub fn pubkey_hex(&self) -> &str {
        self.keys.public_key_hex()
    }

    /// Whether [`BuzzAdapter::connect`] succeeded and nothing has failed since.
    pub fn is_connected(&self) -> bool {
        self.lock().connected
    }

    /// The last failure recorded by a connect or a publish, for the host's error surface.
    pub fn last_error(&self) -> Option<String> {
        self.lock().last_error.clone()
    }

    /// Channels the relay permanently rejected (`restricted` / `not a channel member` /
    /// `auth-required`), which are never re-subscribed.
    pub fn restricted_channels(&self) -> Vec<String> {
        self.lock().restricted.clone()
    }

    /// The last `NOTICE` the relay sent, or `None`.
    pub fn last_notice(&self) -> Option<String> {
        self.lock().last_notice.clone()
    }

    /// How many inbound events were dropped for a bad id, signature or shape.
    pub fn rejected_events(&self) -> u64 {
        self.lock().rejected_events
    }

    /// True once any subscription reached `EOSE`.
    pub fn reached_eose(&self) -> bool {
        !self.lock().eose.is_empty()
    }

    /// The subscription ids currently issued, as `(subscription_id, channel_id)`.
    pub fn subscriptions(&self) -> Vec<(String, String)> {
        self.lock().subscriptions.clone()
    }

    /// Never-panicking runtime guard: a poisoned mutex still yields the state.
    fn lock(&self) -> MutexGuard<'_, Runtime> {
        match self.runtime.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Give every configured channel a cursor so the first `REQ` can use it.
    fn seed_channels(&mut self) {
        let mut runtime = self.lock();
        for channel in &self.config.channels {
            runtime
                .cursors
                .entry(channel.id.clone())
                .or_insert_with(|| ChannelCursor::new(ChatType::Group));
        }
    }

    /// Snapshot every watched channel's cursor (`_cursor_payload`, buzz_cursors.py:46–55).
    pub fn cursors(&self) -> BuzzCursorStore {
        let runtime = self.lock();
        let mut channels = BTreeMap::new();
        for (id, cursor) in &runtime.cursors {
            channels.insert(
                id.clone(),
                BuzzCursor {
                    chat_type: cursor.chat_type.as_str().to_string(),
                    last_ts: cursor.last_ts,
                    seen: cursor.seen.iter().cloned().collect(),
                },
            );
        }
        BuzzCursorStore {
            identity: self.keys.public_key_hex().to_string(),
            relay: self.config.relay_url.clone(),
            channels,
        }
    }

    /// Install persisted cursors (`_load_cursors`, buzz_cursors.py:21–44). A document recorded for a
    /// different identity or relay is ignored wholesale (ids collide across identities), and only
    /// watched channels are restored. Returns how many cursors were installed.
    ///
    /// Restoring *before* [`BuzzAdapter::connect`] is what closes the restart gap: the first `REQ`
    /// then carries the saved `since`, so events that arrived while the gateway was down still
    /// dispatch instead of being treated as history.
    pub fn restore_cursors(&self, store: &BuzzCursorStore) -> usize {
        if !store.belongs_to(self.keys.public_key_hex(), &self.config.relay_url) {
            return 0;
        }
        let mut runtime = self.lock();
        let mut installed = 0usize;
        for channel in &self.config.channels {
            let Some(persisted) = store.channels.get(&channel.id) else {
                continue;
            };
            let chat_type =
                ChatType::parse(persisted.chat_type.as_str()).unwrap_or(ChatType::Group);
            let mut cursor = ChannelCursor::new(chat_type);
            cursor.last_ts = persisted.last_ts;
            let keep = persisted.seen.len().saturating_sub(self.config.seen_cap);
            for event_id in persisted.seen.iter().skip(keep) {
                cursor.mark_seen(event_id, self.config.seen_cap);
            }
            runtime.cursors.insert(channel.id.clone(), cursor);
            installed += 1;
        }
        installed
    }

    /// Resolve a reply anchor to the thread root when the trigger was inside a thread
    /// (`_resolve_reply_anchor`, adapter.py:1634–1636), else the anchor unchanged.
    fn resolve_reply_anchor(&self, anchor: &str) -> String {
        let runtime = self.lock();
        runtime
            .threads
            .iter()
            .rev()
            .find(|(event_id, _)| event_id == anchor)
            .and_then(|(_, root)| root.clone())
            .unwrap_or_else(|| anchor.to_string())
    }

    /// Remember an inbound message's thread root, bounded by [`BUZZ_THREAD_ROOT_CACHE`]
    /// (`_record_thread_root`, adapter.py:1624–1633).
    fn record_thread_root(runtime: &mut Runtime, event_id: &str, root: Option<String>) {
        runtime.threads.retain(|(known, _)| known != event_id);
        runtime.threads.push_back((event_id.to_string(), root));
        while runtime.threads.len() > BUZZ_THREAD_ROOT_CACHE {
            runtime.threads.pop_front();
        }
    }

    /// Where an event id's author is remembered, if any channel saw it.
    fn author_of(&self, event_id: &str) -> Option<String> {
        let runtime = self.lock();
        runtime
            .cursors
            .values()
            .find_map(|cursor| cursor.lookup(event_id).map(|meta| meta.author.clone()))
    }

    /// Subscribe to every watched channel (`_subscribe_websocket` + `_send_channel_subscription`,
    /// adapter.py:1039–1070).
    async fn subscribe(&self) -> Result<usize, BuzzError> {
        let watched: Vec<String> = self
            .config
            .channels
            .iter()
            .map(|channel| channel.id.clone())
            .collect();
        let mut issued = Vec::new();
        for (index, channel_id) in watched.iter().enumerate() {
            {
                let runtime = self.lock();
                if runtime.restricted.contains(channel_id) {
                    continue;
                }
            }
            let last_ts = {
                let runtime = self.lock();
                runtime
                    .cursors
                    .get(channel_id)
                    .map(|cursor| cursor.last_ts)
                    .unwrap_or(0)
            };
            let subscription = format!("vikram-buzz-{index}");
            let mut filter = json!({
                "kinds": BUZZ_DISPATCH_KINDS,
                "#h": [channel_id],
            });
            if last_ts > 0 {
                // Nostr `since` is inclusive: same-second events re-fetch and de-dupe by id.
                filter["since"] = json!(last_ts.saturating_sub(1));
            } else {
                filter["limit"] = json!(self.config.fetch_limit);
            }
            let frame = json!(["REQ", subscription, filter]);
            self.socket
                .send_line(&serde_json::to_string(&frame).unwrap_or_default())
                .await
                .map_err(|error| BuzzError::Socket(error.to_string()))?;
            issued.push((subscription, channel_id.clone()));
        }
        {
            let mut runtime = self.lock();
            runtime.subscriptions = issued.clone();
        }
        Ok(issued.len())
    }

    /// Authenticate and subscribe. `Ok(())` means the relay accepted our AUTH and every `REQ` is on
    /// the wire; `false` from [`PlatformAdapter::connect`] always carries a [`Self::last_error`].
    pub async fn connect_session(&self) -> Result<(), BuzzError> {
        let failure = |error: BuzzError| -> BuzzError {
            let mut runtime = self.lock();
            runtime.connected = false;
            runtime.last_error = Some(error.to_string());
            error
        };
        let websocket_url = match self.websocket_url() {
            Ok(url) => url,
            Err(error) => return Err(failure(error)),
        };
        if self.config.channels.is_empty() {
            return Err(failure(BuzzError::RelayUrl(
                "no Buzz channels to watch".to_string(),
            )));
        }
        let created_at = self.clock.now_unix();
        if let Err(error) = authenticate(
            &*self.socket,
            &self.keys,
            &websocket_url,
            self.config.auth_tag.as_deref(),
            created_at,
            self.config.auth_timeout,
        )
        .await
        {
            return Err(failure(error));
        }
        if let Err(error) = self.subscribe().await {
            return Err(failure(error));
        }
        let mut runtime = self.lock();
        runtime.connected = true;
        runtime.last_error = None;
        Ok(())
    }

    /// Read frames until one produces a dispatchable message; `Ok(None)` at end of stream
    /// (`_ws_read_loop` + `_handle_ws_message`, adapter.py:1151–1210).
    ///
    /// Frames that carry no message — `EOSE`, `NOTICE`, `OK` echoes, housekeeping kinds, our own
    /// echoes, duplicates and gated-out traffic — are consumed silently. An `Err` is a real stream
    /// fault: a socket failure, a frame the relay never answered, or a `CLOSED` that is not permanent
    /// for its channel. The host's reconnect loop then re-dials, and [`BuzzAdapter::connect`] resumes
    /// from the cursors, which is the reference's `_websocket_loop` policy.
    pub async fn next_event(&self) -> Result<Option<MessageEvent>, BuzzError> {
        loop {
            if let Some(event) = self.lock().inbox.pop_front() {
                return Ok(Some(event));
            }
            let Some(line) = self
                .socket
                .next_line()
                .await
                .map_err(|error| BuzzError::Socket(error.to_string()))?
            else {
                return Ok(None);
            };
            // A frame that is not a NIP-01 message is ignored, exactly like
            // "_ws_read_loop: ignoring malformed WebSocket frame" (adapter.py:1171).
            let Ok(frame) = parse_frame(&line) else {
                continue;
            };
            self.handle_frame(frame).await?;
        }
    }

    /// Route one parsed frame; a dispatchable message is queued on the inbox
    /// (`_handle_ws_message`, adapter.py:1185–1210).
    async fn handle_frame(&self, frame: Frame) -> Result<(), BuzzError> {
        match frame {
            Frame::Event {
                subscription,
                event,
            } => {
                let channel = {
                    let runtime = self.lock();
                    runtime
                        .subscriptions
                        .iter()
                        .find(|(known, _)| known == &subscription)
                        .map(|(_, channel)| channel.clone())
                };
                let Some(channel) = channel else {
                    return Ok(());
                };
                if let Err(_error) = event.verify() {
                    // A relay event that does not verify is never dispatched; it is counted so the
                    // host can surface a hostile or broken relay instead of silently losing events.
                    self.lock().rejected_events += 1;
                    return Ok(());
                }
                match self.plan_event(&channel, &event) {
                    DispatchPlan::Skip => {}
                    DispatchPlan::React { event_id, emoji } => {
                        self.publish_best_effort(
                            BUZZ_REACTION_KIND,
                            Self::reaction_tags(&event_id),
                            emoji,
                        )
                        .await;
                    }
                    DispatchPlan::Deliver(deliverable) => {
                        let event_id = deliverable.event_id.clone();
                        self.lock().inbox.push_back(*deliverable.message);
                        if self.config.send_seen_reaction {
                            self.publish_best_effort(
                                BUZZ_REACTION_KIND,
                                Self::reaction_tags(&event_id),
                                BUZZ_SEEN_EMOJI,
                            )
                            .await;
                        }
                    }
                }
                Ok(())
            }
            Frame::Closed {
                subscription,
                reason,
            } => {
                let channel = {
                    let runtime = self.lock();
                    runtime
                        .subscriptions
                        .iter()
                        .find(|(known, _)| known == &subscription)
                        .map(|(_, channel)| channel.clone())
                };
                let permanent = PERMANENT_CLOSE_MARKERS
                    .iter()
                    .any(|marker| reason.to_ascii_lowercase().contains(marker));
                match channel {
                    Some(channel) if permanent => {
                        let mut runtime = self.lock();
                        runtime.restricted.push(channel.clone());
                        runtime
                            .subscriptions
                            .retain(|(known, _)| known != &subscription);
                        runtime.cursors.remove(&channel);
                        Ok(())
                    }
                    _ => Err(BuzzError::SubscriptionClosed {
                        subscription,
                        reason,
                    }),
                }
            }
            Frame::Eose { subscription } => {
                self.lock().eose.insert(subscription);
                Ok(())
            }
            Frame::Notice(message) => {
                self.lock().last_notice = Some(message);
                Ok(())
            }
            Frame::Ok { .. } | Frame::Ignored => Ok(()),
        }
    }

    /// The `e`/`p` tags a reaction carries (`send_reaction` → `reactions add`, adapter.py:859–868,
    /// which the CLI turns into a NIP-25 kind-7 event; the author `p` tag is added when known).
    fn reaction_tags(event_id: &str) -> Vec<Vec<String>> {
        vec![vec!["e".to_string(), event_id.to_string()]]
    }

    /// Dispatch one verified event (`_handle_event`, adapter.py:1444–1514), computing everything
    /// under the lock and building the gateway event inside it.
    fn plan_event(&self, channel_id: &str, event: &NostrEvent) -> DispatchPlan {
        let mut runtime = self.lock();
        let (deliverable, thread_root) = {
            let Some(cursor) = runtime.cursors.get_mut(channel_id) else {
                return DispatchPlan::Skip;
            };
            if event.id.is_empty() || cursor.seen_set.contains(&event.id) {
                return DispatchPlan::Skip;
            }
            cursor.mark_seen(&event.id, self.config.seen_cap);
            cursor.last_ts = cursor.last_ts.max(event.created_at);
            if !BUZZ_DISPATCH_KINDS.contains(&event.kind) {
                return DispatchPlan::Skip;
            }
            let pubkey = event.pubkey.to_ascii_lowercase();
            if pubkey.is_empty() {
                return DispatchPlan::Skip;
            }
            let (attachments, rejected_attachments) = parse_imeta_attachments(&event.tags);
            if event.content.trim().is_empty()
                && attachments.is_empty()
                && rejected_attachments == 0
            {
                return DispatchPlan::Skip;
            }
            // Cache before any early return so self-echo and concurrent-author traffic can still be
            // reply parents (`_remember_event`, adapter.py:1638–1644).
            cursor.remember(event, self.config.seen_cap);
            if pubkey == self.identity.pubkey_hex {
                return DispatchPlan::Skip;
            }
            // Reclassify a leaked DM before gating so its first un-mentioned message both latches and
            // dispatches (`_maybe_latch_dm`, adapter.py:1537–1543).
            if cursor.chat_type == ChatType::Group
                && self.is_direct_message_event(channel_id, event, cursor.chat_type)
            {
                cursor.chat_type = ChatType::Direct;
            }
            let is_dm = cursor.chat_type == ChatType::Direct;
            let reply_parent_id = event.reply_parent_id();
            let reply_meta = reply_parent_id
                .as_deref()
                .and_then(|parent| cursor.lookup(parent))
                .map(|meta| (meta.author.clone(), meta.snippet.clone()));
            let reply_to_is_own = reply_meta
                .as_ref()
                .is_some_and(|(author, _)| author == &self.identity.pubkey_hex);
            let addressed = is_mentioned(&event.content, &self.identity)
                || event.p_tagged(&self.identity.pubkey_hex);
            if !is_dm && self.config.require_mention && !addressed && !reply_to_is_own {
                return DispatchPlan::Skip;
            }
            if !self.config.allowed_pubkeys.is_empty()
                && !self.config.allowed_pubkeys.contains(&pubkey)
            {
                let react = self.config.reaction_only_pubkeys.contains(&pubkey)
                    && event.p_tagged(&self.identity.pubkey_hex)
                    && is_mentioned(&event.content, &self.identity);
                return if react {
                    DispatchPlan::React {
                        event_id: event.id.clone(),
                        emoji: BUZZ_SEEN_EMOJI,
                    }
                } else {
                    DispatchPlan::Skip
                };
            }
            let mut text = strip_leading_mention(&event.content, &self.identity);
            let thread_root = event.thread_root();
            // Mixed kinds use document semantics so an audio member is not mistaken for a voice
            // note (adapter.py:1494–1498).
            let message_type = match attachments.split_first() {
                None => MessageType::Text,
                Some((first, rest)) => {
                    let kind = first.message_type();
                    if rest.iter().all(|other| other.message_type() == kind) {
                        kind
                    } else {
                        MessageType::Document
                    }
                }
            };
            if rejected_attachments > 0 {
                text = format!(
                    "{text}\n{}",
                    attachment_rejection_note(rejected_attachments)
                )
                .trim()
                .to_string();
            }
            if !attachments.is_empty() {
                text = format!("{text}\n{}", attachment_marker(&attachments))
                    .trim()
                    .to_string();
            }
            if text.is_empty() {
                text = "(attachment)".to_string();
            }
            let channel_name = self
                .config
                .channels
                .iter()
                .find(|channel| channel.id == channel_id)
                .map(|channel| channel.name.clone())
                .unwrap_or_else(|| channel_id.to_string());
            let platform = Platform::new(BUZZ_PLATFORM).unwrap_or_else(Platform::local);
            let mut source = SessionSource::new(platform, channel_id.to_string());
            source.chat_type = cursor.chat_type.as_str().to_string();
            source.chat_name = Some(channel_name);
            source.user_id = Some(pubkey.clone());
            source.user_name = Some(Self::display_name_for(&pubkey));
            source.thread_id = thread_root.clone();
            source.message_id = Some(event.id.clone());
            let mut message = MessageEvent::new(text, source);
            message.message_type = message_type;
            message.user_id = Some(pubkey.clone());
            message.user_name = Some(Self::display_name_for(&pubkey));
            message.message_id = Some(event.id.clone());
            message.reply_to_message_id = reply_parent_id;
            message.reply_to_text = reply_meta.as_ref().map(|(_, snippet)| snippet.clone());
            message.reply_to_author_id = reply_meta.as_ref().map(|(author, _)| author.clone());
            message.reply_to_is_own_message = reply_to_is_own;
            message.raw_message = Some(event.to_value());
            let deliverable = Deliverable {
                message: Box::new(message),
                event_id: event.id.clone(),
            };
            (deliverable, thread_root)
        };
        // The thread root is remembered so an answer to this message joins its thread instead of
        // nesting a sub-thread under every reply (`_record_thread_root`, adapter.py:1624–1633).
        Self::record_thread_root(&mut runtime, &event.id, thread_root);
        DispatchPlan::Deliver(Box::new(deliverable))
    }

    /// `_is_direct_message_event` (adapter.py:1527–1536): kind 9, from someone else, `p`-tagged to
    /// us, text *not* mentioning us, in a conversation whose metadata does not rule a DM out.
    fn is_direct_message_event(
        &self,
        channel_id: &str,
        event: &NostrEvent,
        chat_type: ChatType,
    ) -> bool {
        if chat_type == ChatType::Direct || event.kind != BUZZ_CHAT_KIND {
            return false;
        }
        let metadata_allows = self
            .config
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .is_some_and(BuzzChannel::may_reclassify_as_dm);
        if !metadata_allows {
            return false;
        }
        let pubkey = event.pubkey.to_ascii_lowercase();
        !pubkey.is_empty()
            && pubkey != self.identity.pubkey_hex
            && event.p_tagged(&self.identity.pubkey_hex)
            && !is_mentioned(&event.content, &self.identity)
    }

    /// A display name for `pubkey`: without the CLI's profile lookup the reference's own fallback is
    /// used, the first 16 characters of the npub (`_resolve_user_name`, adapter.py:1577–1585).
    fn display_name_for(pubkey: &str) -> String {
        match pubkey_to_npub(pubkey) {
            Some(npub) => npub.chars().take(16).collect(),
            None => pubkey.chars().take(16).collect(),
        }
    }

    /// Publish an event and return it once the relay accepted it (`OK: true`).
    async fn publish(
        &self,
        kind: u64,
        tags: Vec<Vec<String>>,
        content: &str,
    ) -> Result<NostrEvent, BuzzError> {
        let event = sign_event(&self.keys, kind, tags, content, self.clock.now_unix())?;
        let frame = json!(["EVENT", event.to_value()]);
        self.socket
            .send_line(&serde_json::to_string(&frame).unwrap_or_default())
            .await
            .map_err(|error| BuzzError::Socket(error.to_string()))?;
        self.await_ok(&event.id, self.config.send_ok_timeout)
            .await?;
        Ok(event)
    }

    /// Publish without awaiting the relay's `OK` — the reference's best-effort reaction path, whose
    /// failures it logs instead of raising (`send_reaction`, adapter.py:859–868). A write failure is
    /// recorded on [`BuzzAdapter::last_error`] rather than dropped.
    async fn publish_best_effort(&self, kind: u64, tags: Vec<Vec<String>>, content: &str) {
        let Ok(event) = sign_event(&self.keys, kind, tags, content, self.clock.now_unix()) else {
            return;
        };
        let frame = json!(["EVENT", event.to_value()]);
        if let Err(error) = self
            .socket
            .send_line(&serde_json::to_string(&frame).unwrap_or_default())
            .await
        {
            self.lock().last_error = Some(error.to_string());
        }
    }

    /// Read frames until the relay answers `["OK", event_id, …]`, queueing any inbound event that
    /// arrives meanwhile (`await_response` idiom; adapter.py's WS transport cannot interleave a read).
    async fn await_ok(&self, event_id: &str, timeout: Duration) -> Result<(), BuzzError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(BuzzError::Timeout("OK"))?;
            let line = match tokio::time::timeout(remaining, self.socket.next_line()).await {
                Err(_) => return Err(BuzzError::Timeout("OK")),
                Ok(Err(error)) => return Err(BuzzError::Socket(error.to_string())),
                Ok(Ok(None)) => {
                    return Err(BuzzError::Socket(
                        "connection closed while waiting for OK".to_string(),
                    ));
                }
                Ok(Ok(Some(line))) => line,
            };
            let frame = match parse_frame(&line) {
                Ok(frame) => frame,
                Err(_) => continue,
            };
            if let Frame::Ok {
                event_id: accepted_id,
                accepted,
                message,
            } = &frame
                && accepted_id.eq_ignore_ascii_case(event_id)
            {
                return if *accepted {
                    Ok(())
                } else {
                    Err(BuzzError::PublishRejected(message.clone()))
                };
            }
            self.handle_frame(frame).await?;
        }
    }

    /// Publish a NIP-25 reaction and report whether the relay accepted it
    /// (`send_reaction`, adapter.py:859–868).
    pub async fn send_reaction(&self, message_id: &str, emoji: &str) -> bool {
        if message_id.trim().is_empty() || emoji.trim().is_empty() {
            return false;
        }
        let mut tags = Self::reaction_tags(message_id);
        if let Some(author) = self.author_of(message_id) {
            tags.push(vec!["p".to_string(), author]);
        }
        match self.publish(BUZZ_REACTION_KIND, tags, emoji).await {
            Ok(_) => true,
            Err(error) => {
                self.lock().last_error = Some(error.to_string());
                false
            }
        }
    }

    /// The tags for an outgoing kind-9 message: `["h", channel]`, the NIP-10 reply tags and a `p`
    /// tag for the parent author when known.
    fn message_tags(&self, channel_id: &str, reply_to: Option<&str>) -> Vec<Vec<String>> {
        let mut tags = vec![vec!["h".to_string(), channel_id.to_string()]];
        let Some(anchor) = reply_to.map(str::trim).filter(|anchor| !anchor.is_empty()) else {
            return tags;
        };
        let parent = self.resolve_reply_anchor(anchor);
        let parent_author = self.author_of(&parent);
        let relay_hint = self.config.relay_url.trim();
        let e_tag = |target: &str, marker: &str| -> Vec<String> {
            vec![
                "e".to_string(),
                target.to_string(),
                relay_hint.to_string(),
                marker.to_string(),
            ]
        };
        if parent == anchor {
            // A reply to a top-level message: the reference reads a lone `reply` tag as the root.
            tags.push(e_tag(&parent, "reply"));
        } else {
            tags.push(e_tag(&parent, "root"));
            tags.push(e_tag(anchor, "reply"));
        }
        if let Some(author) = parent_author {
            tags.push(vec!["p".to_string(), author]);
        }
        tags
    }

    /// Map a relay rejection onto the gateway's failure taxonomy.
    fn error_kind_for(reason: &str) -> &'static str {
        let lowered = reason.to_ascii_lowercase();
        if lowered.contains("rate") || lowered.contains("slow down") {
            "rate_limited"
        } else if lowered.contains("auth-required")
            || lowered.contains("blocked")
            || lowered.contains("restricted")
            || lowered.contains("not a channel member")
            || lowered.contains("not allowed")
        {
            "forbidden"
        } else if lowered.contains("invalid")
            || lowered.contains("too long")
            || lowered.contains("bad")
        {
            "bad_format"
        } else if lowered.contains("not found") {
            "not_found"
        } else {
            "unknown"
        }
    }
}

#[async_trait]
impl PlatformAdapter for BuzzAdapter {
    /// NIP-42 AUTH then one `REQ` per watched channel. `false` means the relay is not usable;
    /// [`BuzzAdapter::last_error`] says why. `is_reconnect` changes nothing: a reconnect re-AUTHs and
    /// re-subscribes from the persisted cursors, exactly like the reference's WS reconnect path
    /// (`_websocket_loop`, adapter.py:1125–1180).
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        self.connect_session().await.is_ok()
    }

    /// Mark the session down and drop queued events. The socket is injected, so closing it is the
    /// host's call; the cursors stay so the host can persist them with [`BuzzAdapter::cursors`].
    async fn disconnect(&mut self) {
        let mut runtime = self.lock();
        runtime.connected = false;
        runtime.inbox.clear();
        runtime.subscriptions.clear();
    }

    /// Publish the reply as chunked kind-9 events and await each `OK`.
    ///
    /// The anchor is `metadata.thread_id`, then `metadata.reply_to_message_id`, then `reply_to`
    /// (adapter.py:826–836), resolved to the thread root so an answer inside a thread joins it
    /// instead of nesting. A relay `OK: false` — or silence — is a failed [`SendResult`]; nothing is
    /// reported as delivered that the relay did not accept.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        metadata: Option<Value>,
    ) -> SendResult {
        if content.is_empty() {
            return SendResult::failed("Empty message", Some("bad_format"));
        }
        let channel = chat_id.trim();
        if channel.is_empty() {
            return SendResult::failed("Buzz send needs a channel id", Some("bad_format"));
        }
        let meta = metadata.unwrap_or(Value::Null);
        let anchor = meta
            .get("thread_id")
            .and_then(Value::as_str)
            .or_else(|| meta.get("reply_to_message_id").and_then(Value::as_str))
            .or(reply_to)
            .map(str::to_string);
        let tags = self.message_tags(channel, anchor.as_deref());
        let chunks = truncate_message(content, self.config.max_message_length, codepoint_len);
        if chunks.is_empty() {
            return SendResult::failed("Empty message", Some("bad_format"));
        }
        let mut delivered: Vec<String> = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let chunk_tags = if delivered.is_empty() {
                tags.clone()
            } else {
                vec![vec!["h".to_string(), channel.to_string()]]
            };
            match self.publish(BUZZ_CHAT_KIND, chunk_tags, chunk).await {
                Ok(event) => {
                    {
                        let mut runtime = self.lock();
                        if let Some(cursor) = runtime.cursors.get_mut(channel) {
                            cursor.mark_seen(&event.id, self.config.seen_cap);
                            cursor.last_ts = cursor.last_ts.max(event.created_at);
                            cursor.remember(&event, self.config.seen_cap);
                        }
                    }
                    delivered.push(event.id);
                }
                Err(error) => {
                    self.lock().last_error = Some(error.to_string());
                    let mut result = match error {
                        BuzzError::PublishRejected(reason) => {
                            SendResult::failed(reason.clone(), Some(Self::error_kind_for(&reason)))
                        }
                        BuzzError::Timeout(what) => SendResult::failed(
                            format!("buzz relay timed out: {what}"),
                            Some("transient"),
                        )
                        .retryable(None),
                        other => {
                            SendResult::failed(other.to_string(), Some("transient")).retryable(None)
                        }
                    };
                    result.continuation_message_ids = delivered;
                    return result;
                }
            }
        }
        match delivered.last().cloned() {
            Some(last) => SendResult::ok_split(last, delivered),
            None => SendResult::failed("buzz delivered no chunks", Some("unknown")),
        }
    }

    /// Name from the configured channel metadata, class from the latched cursor (the reference's
    /// `get_chat_info`, adapter.py:961–970, with `channels get` unreachable without the CLI).
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        let runtime = self.lock();
        let chat_type = runtime
            .cursors
            .get(chat_id)
            .map(|cursor| cursor.chat_type.as_str().to_string())
            .unwrap_or_else(|| "group".to_string());
        drop(runtime);
        let name = self
            .config
            .channels
            .iter()
            .find(|channel| channel.id == chat_id)
            .map(|channel| channel.name.clone())
            .unwrap_or_else(|| chat_id.to_string());
        ChatInfo { name, chat_type }
    }

    /// Buzz renders Markdown (`register`'s `platform_hint`: "Markdown IS supported").
    fn supports_code_blocks(&self) -> bool {
        true
    }

    /// `send` chunks to [`BUZZ_MAX_MESSAGE_LENGTH`], so the router must not truncate.
    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        self.config.max_message_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::SocketError;

    // -----------------------------------------------------------------------
    // Golden vectors, generated with `python3` against this workspace's
    // `research/hermes-platforms/buzz/nostr_auth.py` (BIP-340 signing with
    // `auxiliary_randomness = 32 × 0x00`, which is the deterministic mode this
    // port uses). The zero-key vector's signature is BIP-340 test vector 0.
    // -----------------------------------------------------------------------

    /// BIP-340 test-vector secret key 3 and everything derived from it.
    const GOLDEN_SECRET_HEX: &str =
        "0000000000000000000000000000000000000000000000000000000000000003";
    /// The same secret as bech32 (`nostr_auth.py` has no nsec encoder; the fixture was encoded by hand).
    const GOLDEN_SECRET_NSEC: &str =
        "nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqps52s3re";
    /// `public_key_hex(GOLDEN_SECRET_HEX)`.
    const GOLDEN_PUBKEY_HEX: &str =
        "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9";
    /// `hex_to_npub(GOLDEN_PUBKEY_HEX)`.
    const GOLDEN_NPUB: &str = "npub1lycg5qvjtrp3qjf5f7zl382j9x6nrjz9sdhenvyxq8c3808qxmus6gq266";
    /// BIP-340 test vector 0: sign `00…00` with key 3 and zero aux randomness.
    const BIP340_VECTOR0_SIG: &str = "e907831f80848d1069a5371b402410364bdf1c5f8307b0084c55f1ce2dca821525f66a4a85ea8b71e482a74f382d2ce5ebeee8fdb2172f477df4900d310536c0";
    /// A second identity (`public_key_hex` of [`OTHER_SECRET_HEX`]) for inbound traffic.
    const OTHER_SECRET_HEX: &str =
        "b7e151628aed2a6abf7158809cf4f3c762e7160f38b4da56a784d9045190cfef";
    /// `public_key_hex(OTHER_SECRET_HEX)`.
    const OTHER_PUBKEY_HEX: &str =
        "dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659";
    /// `hex_to_npub(OTHER_PUBKEY_HEX)`.
    const OTHER_NPUB: &str = "npub1mlcawle2vuw97dscxundkg6phev0atsa5t0vakzrys8hk5pt5evssm7a0a";
    /// The channel these fixtures live in.
    const CHANNEL: &str = "11111111-2222-3333-4444-555555555555";
    /// The configured relay (as `BuzzConfig::relay_url` holds it).
    const RELAY_URL: &str = "https://relay.buzz.example.com";
    /// The `ws://` form used in the AUTH event (`_websocket_url`).
    const RELAY_WS_URL: &str = "wss://relay.buzz.example.com";
    /// The fixed `created_at` every fixture was generated at.
    const NOW: u64 = 1_700_000_000;
    /// The AUTH challenge the golden auth event answers.
    const CHALLENGE: &str = "challenge-abc123";
    /// The exact id material the reference serialized (byte-for-byte).
    const GOLDEN_EVENT_SERIALIZED: &str = r#"[0,"f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9",1700000000,9,[["h","11111111-2222-3333-4444-555555555555"],["p","aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]],"hello from vikram"]"#;
    /// The signed event the reference produced for that material.
    const GOLDEN_EVENT_JSON: &str = r#"{"id":"1a739aa9751e4e09efe12997e8bb5e6065fd141249d43bc23d54904420fe1beb","pubkey":"f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9","created_at":1700000000,"kind":9,"tags":[["h","11111111-2222-3333-4444-555555555555"],["p","aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]],"content":"hello from vikram","sig":"b2cf84b94c36b3e1db929df3d7148e53b62711b957a21d1b840d7a200a408b7d4ec3d52b702395d5e1426a5c1856cb607b30d7aaa773f029b1264b03ab7f992c"}"#;
    /// Id of the same event shape with non-ASCII content (`héllo 日本 🐝`), created at `NOW + 1`.
    const GOLDEN_UTF8_ID: &str = "1770d707338c3d2f26ff37f1c431f3e21bdd45dbbdbfae1f990793a41eb9a9d4";
    /// Signature of that event — proves `ensure_ascii=False` serialization agreement.
    const GOLDEN_UTF8_SIG: &str = "b5c19cbeb520c3b4428f972b42d2b7ca87dec10c0b60ecb49f44345ed5b071b8a3c8a736a31487a0da46b0fdedaa0e38f5bcb8c15471b42729599ff54cc4a56d";
    /// `nostr_auth.build_auth_event(challenge=CHALLENGE, relay_url=RELAY_WS_URL, created_at=NOW)`.
    const GOLDEN_AUTH_JSON: &str = r#"{"id":"ee76119a6124bc07a044c7e13e051384d45d67699c74b134d7ed3c936b8ef7c8","pubkey":"f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9","created_at":1700000000,"kind":22242,"tags":[["relay","wss://relay.buzz.example.com"],["challenge","challenge-abc123"]],"content":"","sig":"75f93901bb5d32d5114f65b9eb1c178150e1c526576b9c3b0a26b01ae6239b24b649e332a8621e077fed03476c73c83435ca2d05073b4b6a7eedf674d06271bd"}"#;
    /// The same auth event carrying `["auth","owner-pubkey","attestation","signature"]`.
    const GOLDEN_AUTH_TAGGED_JSON: &str = r#"{"id":"d7e80886576c2939a3f89caceb6e446fce3fb7d1382e130fa58b6938bce326bc","pubkey":"f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9","created_at":1700000000,"kind":22242,"tags":[["relay","wss://relay.buzz.example.com"],["challenge","challenge-abc123"],["auth","owner-pubkey","attestation","signature"]],"content":"","sig":"5f5a401304040c27557b6c06a1179b149493d33344f70c65dcb9606abecb9e344fd35d3904611113b601a0c69c526e5d06b1e5099a3555fcbf022de9ab4faa73"}"#;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    struct MockSocket {
        sent: Mutex<Vec<String>>,
        incoming: Mutex<VecDeque<String>>,
        stall: Mutex<bool>,
    }

    impl MockSocket {
        fn new(incoming: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                incoming: Mutex::new(incoming.into()),
                stall: Mutex::new(false),
            })
        }

        /// A socket that never answers, for the timeout paths.
        fn stalling() -> Arc<Self> {
            let socket = Self::new(Vec::new());
            socket.stall();
            socket
        }

        /// Stop answering: the next read hangs until the adapter's own timeout fires.
        fn stall(&self) {
            *self.stall.lock().unwrap() = true;
        }

        fn script(&self, lines: Vec<String>) {
            let mut queue = self.incoming.lock().unwrap();
            for line in lines {
                queue.push_back(line);
            }
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }

        /// Every frame we wrote, parsed, so ordering of object keys cannot flake a test.
        fn sent_frames(&self) -> Vec<Value> {
            self.sent()
                .iter()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }

        fn sent_of(&self, label: &str) -> Vec<Value> {
            self.sent_frames()
                .into_iter()
                .filter(|frame| frame.get(0).and_then(Value::as_str) == Some(label))
                .collect()
        }
    }

    #[async_trait]
    impl LineSocket for MockSocket {
        async fn send_line(&self, line: &str) -> Result<(), SocketError> {
            self.sent.lock().unwrap().push(line.to_string());
            Ok(())
        }

        async fn next_line(&self) -> Result<Option<String>, SocketError> {
            let stalling = *self.stall.lock().unwrap();
            if stalling {
                tokio::time::sleep(Duration::from_secs(60)).await;
                return Ok(None);
            }
            Ok(self.incoming.lock().unwrap().pop_front())
        }
    }

    fn keys() -> BuzzKeypair {
        BuzzKeypair::from_private_key(GOLDEN_SECRET_HEX).expect("golden key")
    }

    fn other_keys() -> BuzzKeypair {
        BuzzKeypair::from_private_key(OTHER_SECRET_HEX).expect("second key")
    }

    fn quick(mut config: BuzzConfig) -> BuzzConfig {
        config.auth_timeout = Duration::from_millis(50);
        config.send_ok_timeout = Duration::from_millis(50);
        config
    }

    fn test_config() -> BuzzConfig {
        quick(
            BuzzConfig::new(RELAY_URL)
                .with_channel(BuzzChannel::with_metadata(CHANNEL, "general", "a channel"))
                .with_display_name("Vikram")
                .acknowledging_receipts(false),
        )
    }

    fn dm_config() -> BuzzConfig {
        quick(
            BuzzConfig::new(RELAY_URL)
                .with_channel(BuzzChannel::with_metadata("dm-channel", "DM", ""))
                .with_display_name("Vikram")
                .acknowledging_receipts(false),
        )
    }

    fn adapter_over(socket: &Arc<MockSocket>, config: BuzzConfig) -> BuzzAdapter {
        BuzzAdapter::new(Arc::clone(socket) as SharedLineSocket, keys(), config)
            .with_clock(Arc::new(FixedClock::new(NOW)))
    }

    fn auth_exchange(accepted: bool, message: &str) -> Vec<String> {
        auth_exchange_with("challenge-abc123", accepted, message)
    }

    fn auth_exchange_with(challenge: &str, accepted: bool, message: &str) -> Vec<String> {
        let event =
            build_auth_event(&keys(), challenge, RELAY_WS_URL, None, NOW).expect("auth event");
        vec![
            json!(["AUTH", challenge]).to_string(),
            json!(["OK", event.id, accepted, message]).to_string(),
        ]
    }

    /// Connect an adapter against a mock relay that accepts our AUTH.
    async fn connected(socket: &Arc<MockSocket>, config: BuzzConfig) -> BuzzAdapter {
        let adapter = adapter_over(socket, config);
        socket.script(auth_exchange(true, ""));
        assert!(
            adapter.connect_session().await.is_ok(),
            "connect failed: {:?}",
            adapter.last_error()
        );
        adapter
    }

    fn subscription_id(index: usize) -> String {
        format!("vikram-buzz-{index}")
    }

    fn event_frame(subscription: &str, event: &NostrEvent) -> String {
        json!(["EVENT", subscription, event.to_value()]).to_string()
    }

    fn ok_frame(event_id: &str, accepted: bool, message: &str) -> String {
        json!(["OK", event_id, accepted, message]).to_string()
    }

    fn from_other(kind: u64, tags: Vec<Vec<String>>, content: &str, created_at: u64) -> NostrEvent {
        sign_event(&other_keys(), kind, tags, content, created_at).expect("signed inbound event")
    }

    fn channel_tag() -> Vec<String> {
        vec!["h".to_string(), CHANNEL.to_string()]
    }

    fn p_tag_self() -> Vec<String> {
        vec!["p".to_string(), GOLDEN_PUBKEY_HEX.to_string()]
    }

    /// Bech32-encode an arbitrary 5-bit payload with a fresh checksum, for rejection fixtures.
    fn encode_values(hrp: &str, data: &[u8]) -> String {
        let mut values = bech32_hrp_expand(hrp);
        values.extend_from_slice(data);
        values.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let polymod = bech32_polymod(&values) ^ 1;
        let mut out = format!("{hrp}1");
        for value in data {
            out.push(BECH32_CHARSET[*value as usize] as char);
        }
        for index in 0..6 {
            out.push(BECH32_CHARSET[((polymod >> (5 * (5 - index))) & 31) as usize] as char);
        }
        out
    }

    // -----------------------------------------------------------------------
    // Key handling and bech32
    // -----------------------------------------------------------------------

    #[test]
    fn golden_key_derivation_matches_the_reference() {
        let keys = keys();
        assert_eq!(keys.public_key_hex(), GOLDEN_PUBKEY_HEX);
        assert_eq!(keys.npub(), GOLDEN_NPUB);

        // The nsec form decodes to the same identity.
        let from_nsec = BuzzKeypair::from_private_key(GOLDEN_SECRET_NSEC).expect("nsec key");
        assert_eq!(from_nsec.public_key_hex(), GOLDEN_PUBKEY_HEX);
        assert_eq!(from_nsec.npub(), GOLDEN_NPUB);

        // Uppercase hex is accepted; the derived identity is the same.
        let upper =
            BuzzKeypair::from_private_key(&GOLDEN_SECRET_HEX.to_uppercase()).expect("hex key");
        assert_eq!(upper.public_key_hex(), GOLDEN_PUBKEY_HEX);

        // BIP-340 test vector 0: the same signature `nostr_auth.schnorr_sign` produces with zero aux.
        let signature = keys.sign_hash(&[0u8; 32]).expect("signature");
        assert_eq!(hex_encode(&signature), BIP340_VECTOR0_SIG);
    }

    #[test]
    fn bech32_npub_round_trip() {
        assert_eq!(
            pubkey_to_npub(GOLDEN_PUBKEY_HEX).as_deref(),
            Some(GOLDEN_NPUB)
        );
        assert_eq!(
            npub_to_pubkey_hex(GOLDEN_NPUB).as_deref(),
            Some(GOLDEN_PUBKEY_HEX)
        );
        // Uppercase npub decodes too (bech32 allows an all-uppercase form).
        assert_eq!(
            npub_to_pubkey_hex(&GOLDEN_NPUB.to_uppercase()).as_deref(),
            Some(GOLDEN_PUBKEY_HEX)
        );
        assert_eq!(pubkey_to_npub("not-a-pubkey"), None);
        assert_eq!(npub_to_pubkey_hex("npub1"), None);
    }

    #[test]
    fn user_references_normalize_hex_and_npub() {
        assert_eq!(
            normalize_user_ref(OTHER_PUBKEY_HEX).as_deref(),
            Some(OTHER_PUBKEY_HEX)
        );
        assert_eq!(
            normalize_user_ref(&format!("  {}  ", OTHER_NPUB)).as_deref(),
            Some(OTHER_PUBKEY_HEX)
        );
        assert_eq!(normalize_user_ref(""), None);
        assert_eq!(normalize_user_ref("alice"), None);
        assert_eq!(normalize_user_ref(&"g".repeat(64)), None);

        // The allow-list builder runs every entry through it.
        let config = BuzzConfig::new(RELAY_URL).allowing(&[OTHER_NPUB, "junk"]);
        assert_eq!(config.allowed_pubkeys, vec![OTHER_PUBKEY_HEX.to_string()]);
    }

    #[test]
    fn nsec_rejections_are_specific() {
        // Mixed case: bech32 forbids it outright.
        let mixed = format!(
            "nsec1{}A",
            &GOLDEN_SECRET_NSEC[6..GOLDEN_SECRET_NSEC.len() - 1]
        );
        assert_eq!(
            decode_private_key(&mixed),
            Err(BuzzError::bech32("nsec", "cannot mix upper- and lowercase"))
        );

        // A one-character change breaks the checksum.
        let mut broken = GOLDEN_SECRET_NSEC.to_string();
        broken.pop();
        broken.push('q');
        assert_eq!(
            decode_private_key(&broken),
            Err(BuzzError::bech32("nsec", "has an invalid checksum"))
        );

        // The wrong hrp is refused. Note the reference reaches this through `_decode_nsec` only for
        // a string that starts with `nsec1`: anything else is treated as hex, so an npub is
        // `KeyFormat` — the prefix check is exercised directly on the decoder.
        assert_eq!(decode_private_key(GOLDEN_NPUB), Err(BuzzError::KeyFormat));
        assert_eq!(
            bech32_decode_32(GOLDEN_NPUB, "nsec"),
            Err(BuzzError::bech32("nsec", "uses the wrong prefix"))
        );
        assert_eq!(
            BuzzKeypair::from_private_key(GOLDEN_NPUB).unwrap_err(),
            BuzzError::KeyFormat
        );

        // 'b' is not in the bech32 charset.
        let mut invalid_char = GOLDEN_SECRET_NSEC.to_string();
        let index = invalid_char.len() - 2;
        invalid_char.replace_range(index..index + 1, "b");
        assert_eq!(
            decode_private_key(&invalid_char),
            Err(BuzzError::bech32("nsec", "has an invalid character"))
        );

        // A payload shorter than 32 bytes.
        let short = encode_values("nsec", &convert_bits_8_to_5(&[7u8; 20]));
        assert_eq!(
            decode_private_key(&short),
            Err(BuzzError::bech32(
                "nsec",
                "must encode a value that is exactly 32 bytes"
            ))
        );

        // Non-zero padding in the final 5-bit group.
        let mut padded = convert_bits_8_to_5(&[3u8; 32]);
        let last = padded.len() - 1;
        padded[last] = (padded[last] & 0x10) | 0x01;
        let padded = encode_values("nsec", &padded);
        assert_eq!(
            decode_private_key(&padded),
            Err(BuzzError::bech32("nsec", "payload has non-zero padding"))
        );

        // Hex that is not a key at all.
        assert_eq!(decode_private_key("deadbeef"), Err(BuzzError::KeyFormat));
        assert_eq!(
            decode_private_key(&"0".repeat(64)),
            Err(BuzzError::KeyRange)
        );
    }

    #[test]
    fn debug_output_never_carries_the_secret() {
        let rendered = format!("{:?}", keys());
        assert!(
            rendered.contains(GOLDEN_NPUB),
            "npub is the display identity"
        );
        assert!(!rendered.contains(GOLDEN_SECRET_HEX));
        assert!(!rendered.contains("nsec1"));
    }

    // -----------------------------------------------------------------------
    // Events: id + signature goldens
    // -----------------------------------------------------------------------

    #[test]
    fn golden_event_id_and_signature_are_reproduced_byte_for_byte() {
        let tags = vec![channel_tag(), vec!["p".to_string(), "aa".repeat(32)]];
        let material = NostrEvent::id_material(
            GOLDEN_PUBKEY_HEX,
            NOW,
            BUZZ_CHAT_KIND,
            &tags,
            "hello from vikram",
        );
        assert_eq!(material, GOLDEN_EVENT_SERIALIZED);

        let event =
            sign_event(&keys(), BUZZ_CHAT_KIND, tags, "hello from vikram", NOW).expect("signed");
        let golden: Value = serde_json::from_str(GOLDEN_EVENT_JSON).expect("golden json");
        assert_eq!(event.to_value(), golden);
        assert_eq!(
            event.id,
            "1a739aa9751e4e09efe12997e8bb5e6065fd141249d43bc23d54904420fe1beb"
        );
        assert_eq!(
            event.sig,
            "b2cf84b94c36b3e1db929df3d7148e53b62711b957a21d1b840d7a200a408b7d4ec3d52b702395d5e1426a5c1856cb607b30d7aaa773f029b1264b03ab7f992c"
        );
        event.verify().expect("golden event verifies");
        assert_eq!(event.computed_id(), event.id);
    }

    #[test]
    fn utf8_content_serializes_like_the_reference() {
        let event = sign_event(
            &keys(),
            BUZZ_CHAT_KIND,
            vec![channel_tag()],
            "héllo 日本 🐝",
            NOW + 1,
        )
        .expect("signed");
        assert_eq!(event.id, GOLDEN_UTF8_ID);
        assert_eq!(event.sig, GOLDEN_UTF8_SIG);
        event.verify().expect("verifies");
        // The id material keeps the characters raw (`ensure_ascii=False`).
        assert!(
            NostrEvent::id_material(
                GOLDEN_PUBKEY_HEX,
                NOW + 1,
                BUZZ_CHAT_KIND,
                &[channel_tag()],
                "héllo 日本 🐝"
            )
            .contains("héllo 日本 🐝")
        );
    }

    #[test]
    fn verification_rejects_tampering_and_wrong_lengths() {
        let golden: NostrEvent = {
            let value: Value = serde_json::from_str(GOLDEN_EVENT_JSON).expect("golden json");
            NostrEvent::from_value(&value).expect("parse")
        };
        golden.verify().expect("pristine event verifies");

        // Changed content no longer matches the id.
        let mut tampered = golden.clone();
        tampered.content.push('!');
        assert_eq!(tampered.verify(), Err(BuzzError::EventIdMismatch));

        // A rewritten id is caught by the recomputation.
        let mut rewritten = golden.clone();
        rewritten.id = "00".repeat(32);
        assert_eq!(rewritten.verify(), Err(BuzzError::EventIdMismatch));

        // A flipped signature nibble fails BIP-340 verification.
        let mut bad_sig = golden.clone();
        let flipped = if bad_sig.sig.starts_with('b') {
            "c"
        } else {
            "b"
        };
        bad_sig.sig.replace_range(0..1, flipped);
        assert_eq!(bad_sig.verify(), Err(BuzzError::EventSignature));

        // Wrong lengths never reach the curve code.
        let mut short_sig = golden.clone();
        short_sig.sig.truncate(126);
        assert!(matches!(short_sig.verify(), Err(BuzzError::Event(_))));
        let mut short_pubkey = golden.clone();
        short_pubkey.pubkey.truncate(62);
        assert!(matches!(short_pubkey.verify(), Err(BuzzError::Event(_))));
        let mut short_id = golden.clone();
        short_id.id.truncate(62);
        assert!(matches!(short_id.verify(), Err(BuzzError::Event(_))));

        // A pubkey that is not a valid x-only point never verifies (it fails the id recomputation
        // first, and the point parse if the id is rewritten to match).
        let mut bad_pubkey = golden.clone();
        bad_pubkey.pubkey = "ff".repeat(32);
        assert!(bad_pubkey.verify().is_err());
        bad_pubkey.id = bad_pubkey.computed_id();
        assert!(bad_pubkey.verify().is_err());
    }

    #[test]
    fn event_parsing_requires_nip01_shape() {
        let value: Value = serde_json::from_str(GOLDEN_EVENT_JSON).expect("golden json");
        let event = NostrEvent::from_value(&value).expect("parse");
        assert_eq!(event.kind, BUZZ_CHAT_KIND);
        assert_eq!(event.created_at, NOW);
        assert_eq!(event.channel_tag(), Some(CHANNEL));
        assert!(!event.p_tagged(GOLDEN_PUBKEY_HEX));

        assert!(matches!(
            NostrEvent::from_value(&json!("nope")),
            Err(BuzzError::Event("not an object"))
        ));
        assert!(matches!(
            NostrEvent::from_value(&json!({"id": "x"})),
            Err(BuzzError::Event("pubkey is not a string"))
        ));
        assert!(matches!(
            NostrEvent::from_value(&json!({"id": "a", "pubkey": "b", "content": "c", "sig": "d"})),
            Err(BuzzError::Event("created_at is not an unsigned integer"))
        ));
        // A non-string tag value would silently change the id if coerced, so it is refused.
        assert!(matches!(
            NostrEvent::from_value(&json!({
                "id": "a", "pubkey": "b", "content": "c", "sig": "d", "created_at": 1, "kind": 9,
                "tags": [["e", 5]]
            })),
            Err(BuzzError::Event("a tag value is not a string"))
        ));
    }

    #[test]
    fn nip10_tags_are_read_like_the_reference() {
        let event = sign_event(
            &other_keys(),
            BUZZ_CHAT_KIND,
            vec![
                channel_tag(),
                vec![
                    "e".into(),
                    "root-id".into(),
                    RELAY_URL.into(),
                    "root".into(),
                ],
                vec![
                    "e".into(),
                    "parent-id".into(),
                    RELAY_URL.into(),
                    "reply".into(),
                ],
                p_tag_self(),
            ],
            "@Vikram and also 日本",
            NOW,
        )
        .expect("signed");

        assert_eq!(event.reply_parent_id().as_deref(), Some("parent-id"));
        assert_eq!(event.thread_root().as_deref(), Some("root-id"));
        assert!(event.p_tagged(GOLDEN_PUBKEY_HEX));
        assert!(!event.p_tagged(OTHER_PUBKEY_HEX));

        // A lone `reply` tag started the thread at that parent, so it is the root.
        let lone = sign_event(
            &other_keys(),
            BUZZ_CHAT_KIND,
            vec![
                channel_tag(),
                vec!["e".into(), "parent-id".into(), "".into(), "reply".into()],
            ],
            "hi",
            NOW,
        )
        .expect("signed");
        assert_eq!(lone.reply_parent_id().as_deref(), Some("parent-id"));
        assert_eq!(lone.thread_root().as_deref(), Some("parent-id"));

        // Unmarked positional e-tag: parent and root both come from it.
        let positional = sign_event(
            &other_keys(),
            BUZZ_CHAT_KIND,
            vec![channel_tag(), vec!["e".into(), "only-id".into()]],
            "hi",
            NOW,
        )
        .expect("signed");
        assert_eq!(positional.reply_parent_id().as_deref(), Some("only-id"));
        assert_eq!(positional.thread_root().as_deref(), Some("only-id"));
    }

    // -----------------------------------------------------------------------
    // NIP-42
    // -----------------------------------------------------------------------

    #[test]
    fn golden_auth_event_matches_the_reference() {
        let event = build_auth_event(&keys(), CHALLENGE, RELAY_WS_URL, None, NOW).expect("auth");
        let golden: Value = serde_json::from_str(GOLDEN_AUTH_JSON).expect("golden");
        assert_eq!(event.to_value(), golden);
        assert_eq!(event.kind, BUZZ_AUTH_KIND);
        assert_eq!(event.content, "");
        assert_eq!(
            event.tags,
            vec![
                vec!["relay".to_string(), RELAY_WS_URL.to_string()],
                vec!["challenge".to_string(), CHALLENGE.to_string()],
            ]
        );
        event.verify().expect("auth event verifies");

        // The optional NIP-OA owner-attestation tag.
        let tag = vec![
            "auth".to_string(),
            "owner-pubkey".to_string(),
            "attestation".to_string(),
            "signature".to_string(),
        ];
        let tagged =
            build_auth_event(&keys(), CHALLENGE, RELAY_WS_URL, Some(&tag), NOW).expect("auth");
        let golden_tagged: Value =
            serde_json::from_str(GOLDEN_AUTH_TAGGED_JSON).expect("golden tagged");
        assert_eq!(tagged.to_value(), golden_tagged);
        assert_eq!(tagged.tags.len(), 3);
    }

    #[test]
    fn auth_tags_are_validated() {
        let parsed = parse_auth_tag(&json!(r#"["auth","a","b","c"]"#)).expect("json text");
        assert_eq!(parsed, vec!["auth", "a", "b", "c"]);
        let parsed = parse_auth_tag(&json!(["auth", "a", "b", "c"])).expect("tag value");
        assert_eq!(parsed, vec!["auth", "a", "b", "c"]);

        assert_eq!(parse_auth_tag(&json!("not json")), Err(BuzzError::AuthTag));
        assert_eq!(
            parse_auth_tag(&json!(["auth", "a", "b"])),
            Err(BuzzError::AuthTag)
        );
        assert_eq!(
            parse_auth_tag(&json!(["owner", "a", "b", "c"])),
            Err(BuzzError::AuthTag)
        );
        assert_eq!(
            parse_auth_tag(&json!(["auth", "a", "b", 5])),
            Err(BuzzError::AuthTag)
        );

        // The builder refuses an ill-formed tag rather than signing it.
        let bad = vec!["auth".to_string(), "only-two".to_string()];
        assert_eq!(
            build_auth_event(&keys(), CHALLENGE, RELAY_WS_URL, Some(&bad), NOW).unwrap_err(),
            BuzzError::AuthTag
        );

        // Configuration path: `BUZZ_AUTH_TAG` arrives as JSON text.
        let config = BuzzConfig::new(RELAY_URL)
            .with_auth_tag(r#"["auth","a","b","c"]"#)
            .expect("auth tag");
        assert_eq!(
            config.auth_tag,
            Some(vec!["auth".into(), "a".into(), "b".into(), "c".into()])
        );
        assert!(BuzzConfig::new(RELAY_URL).with_auth_tag("nope").is_err());
    }

    #[tokio::test]
    async fn nip42_transcript_challenge_auth_ok() {
        let socket = MockSocket::new(vec![
            json!(["AUTH", CHALLENGE]).to_string(),
            json!([
                "OK",
                serde_json::from_str::<Value>(GOLDEN_AUTH_JSON).expect("golden")["id"],
                true,
                ""
            ])
            .to_string(),
        ]);
        // A second, unrelated frame between the AUTH challenge and our OK is ignored.
        socket.script(vec![json!(["NOTICE", "hello"]).to_string()]);
        let event = authenticate(
            &*socket as &dyn LineSocket,
            &keys(),
            RELAY_WS_URL,
            None,
            NOW,
            Duration::from_millis(50),
        )
        .await
        .expect("authenticated");

        let sent = socket.sent();
        assert_eq!(sent.len(), 1, "one AUTH frame, got {sent:?}");
        let frame: Value = serde_json::from_str(&sent[0]).expect("frame json");
        let golden: Value = serde_json::from_str(GOLDEN_AUTH_JSON).expect("golden");
        assert_eq!(frame, json!(["AUTH", golden]));
        assert_eq!(
            event.id,
            "ee76119a6124bc07a044c7e13e051384d45d67699c74b134d7ed3c936b8ef7c8"
        );
    }

    #[tokio::test]
    async fn nip42_rejections_are_truthful() {
        let timeout = Duration::from_millis(50);

        // No AUTH challenge at all.
        let socket = MockSocket::new(vec![json!(["NOTICE", "no auth here"]).to_string()]);
        assert_eq!(
            authenticate(
                &*socket as &dyn LineSocket,
                &keys(),
                RELAY_WS_URL,
                None,
                NOW,
                timeout
            )
            .await,
            Err(BuzzError::AuthRejected(
                "relay did not send a NIP-42 AUTH challenge".to_string()
            ))
        );

        // A challenge that is not a string.
        let socket = MockSocket::new(vec![json!(["AUTH", 7]).to_string()]);
        assert_eq!(
            authenticate(
                &*socket as &dyn LineSocket,
                &keys(),
                RELAY_WS_URL,
                None,
                NOW,
                timeout
            )
            .await,
            Err(BuzzError::AuthRejected(
                "AUTH challenge is not a string".to_string()
            ))
        );

        // OK false carries the relay's reason.
        let socket = MockSocket::new(vec![]);
        socket.script(auth_exchange(false, "invalid: bad signature"));
        assert_eq!(
            authenticate(
                &*socket as &dyn LineSocket,
                &keys(),
                RELAY_WS_URL,
                None,
                NOW,
                timeout
            )
            .await,
            Err(BuzzError::AuthRejected(
                "invalid: bad signature".to_string()
            ))
        );

        // CLOSED before the OK.
        let socket = MockSocket::new(vec![
            json!(["AUTH", CHALLENGE]).to_string(),
            json!(["CLOSED", "challenge-abc123", "auth-required: go away"]).to_string(),
        ]);
        assert_eq!(
            authenticate(
                &*socket as &dyn LineSocket,
                &keys(),
                RELAY_WS_URL,
                None,
                NOW,
                timeout
            )
            .await,
            Err(BuzzError::AuthRejected(
                "auth-required: go away".to_string()
            ))
        );
        // An OK for a different event id is not our OK.
        let socket = MockSocket::new(vec![
            json!(["AUTH", CHALLENGE]).to_string(),
            ok_frame("ff", true, ""),
        ]);
        assert_eq!(
            authenticate(
                &*socket as &dyn LineSocket,
                &keys(),
                RELAY_WS_URL,
                None,
                NOW,
                timeout
            )
            .await,
            Err(BuzzError::Socket(
                "connection closed while waiting for NIP-42 OK".to_string()
            ))
        );

        // The relay answers the challenge but never the AUTH event.
        let socket = MockSocket::new(vec![json!(["AUTH", CHALLENGE]).to_string()]);
        let error = authenticate(
            &*socket as &dyn LineSocket,
            &keys(),
            RELAY_WS_URL,
            None,
            NOW,
            timeout,
        )
        .await;
        assert_eq!(
            error,
            Err(BuzzError::Socket(
                "connection closed while waiting for NIP-42 OK".to_string()
            ))
        );

        // A silent relay times out instead of hanging.
        let socket = MockSocket::stalling();
        assert_eq!(
            authenticate(
                &*socket as &dyn LineSocket,
                &keys(),
                RELAY_WS_URL,
                None,
                NOW,
                timeout
            )
            .await,
            Err(BuzzError::Timeout("NIP-42 challenge"))
        );

        // A closed socket is a socket error, not a silent success.
        let socket = MockSocket::new(vec![]);
        assert_eq!(
            authenticate(
                &*socket as &dyn LineSocket,
                &keys(),
                RELAY_WS_URL,
                None,
                NOW,
                timeout
            )
            .await,
            Err(BuzzError::Socket(
                "connection closed while waiting for NIP-42 challenge".to_string()
            ))
        );
    }

    #[test]
    fn relay_urls_map_onto_websocket_urls() {
        assert_eq!(relay_websocket_url(RELAY_URL).as_deref(), Ok(RELAY_WS_URL));
        assert_eq!(
            relay_websocket_url("http://relay.example.com/path?x=1#frag").as_deref(),
            Ok("ws://relay.example.com/path?x=1")
        );
        assert_eq!(
            relay_websocket_url("wss://relay.example.com").as_deref(),
            Ok("wss://relay.example.com")
        );
        assert!(matches!(
            relay_websocket_url("ftp://relay"),
            Err(BuzzError::RelayUrl(_))
        ));
        assert!(matches!(
            relay_websocket_url("relay.example.com"),
            Err(BuzzError::RelayUrl(_))
        ));
        assert!(matches!(
            relay_websocket_url("https://"),
            Err(BuzzError::RelayUrl(_))
        ));
    }

    // -----------------------------------------------------------------------
    // Connect / subscription
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn connect_authenticates_then_sends_the_reference_filters() {
        let config = quick(
            BuzzConfig::new(RELAY_URL)
                .with_channel(BuzzChannel::new("chan-a"))
                .with_channel(BuzzChannel::new("chan-b"))
                .acknowledging_receipts(false),
        );
        let socket = MockSocket::new(vec![]);
        let adapter = adapter_over(&socket, config);
        let store = BuzzCursorStore {
            identity: keys().public_key_hex().to_string(),
            relay: RELAY_URL.to_string(),
            channels: BTreeMap::from([(
                "chan-a".to_string(),
                BuzzCursor {
                    chat_type: "group".to_string(),
                    last_ts: 1_700_000_500,
                    seen: Vec::new(),
                },
            )]),
        };
        assert_eq!(adapter.restore_cursors(&store), 1);
        socket.script(auth_exchange(true, ""));
        adapter.connect_session().await.expect("connected");
        assert!(adapter.is_connected());

        // AUTH first, then one REQ per channel.
        let frames = socket.sent_frames();
        assert_eq!(frames[0][0], "AUTH");
        let requests = socket.sent_of("REQ");
        assert_eq!(requests.len(), 2);
        // A cursor with a high-water mark resumes one second early (Nostr `since` is inclusive).
        assert_eq!(
            requests[0],
            json!([
                "REQ",
                "vikram-buzz-0",
                {"kinds": [9, 45001, 45003], "#h": ["chan-a"], "since": 1_700_000_499}
            ])
        );
        // A channel with no mark replays a bounded window instead.
        assert_eq!(
            requests[1],
            json!([
                "REQ",
                "vikram-buzz-1",
                {"kinds": [9, 45001, 45003], "#h": ["chan-b"], "limit": 50}
            ])
        );
        assert_eq!(
            adapter.subscriptions(),
            vec![
                ("vikram-buzz-0".to_string(), "chan-a".to_string()),
                ("vikram-buzz-1".to_string(), "chan-b".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn connect_fails_truthfully_without_a_relay_or_channels() {
        // No channels: nothing to subscribe to.
        let socket = MockSocket::new(vec![]);
        let adapter = adapter_over(&socket, quick(BuzzConfig::new(RELAY_URL)));
        assert!(adapter.connect_session().await.is_err());
        assert!(!adapter.is_connected());
        assert!(
            adapter
                .last_error()
                .is_some_and(|error| error.contains("no Buzz channels"))
        );
        assert!(socket.sent().is_empty());

        // A relay that rejects our AUTH.
        let socket = MockSocket::new(vec![]);
        socket.script(auth_exchange(false, "invalid: cannot verify"));
        let adapter = adapter_over(&socket, test_config());
        assert!(adapter.connect_session().await.is_err());
        assert!(!adapter.is_connected());
        assert!(
            adapter
                .last_error()
                .is_some_and(|error| error.contains("invalid: cannot verify"))
        );
        // No REQ was sent on an unauthenticated connection.
        assert!(socket.sent_of("REQ").is_empty());
    }

    #[tokio::test]
    async fn a_foreign_cursor_document_is_ignored() {
        let socket = MockSocket::new(vec![]);
        let adapter = adapter_over(&socket, test_config());
        let foreign = BuzzCursorStore {
            identity: OTHER_PUBKEY_HEX.to_string(),
            relay: RELAY_URL.to_string(),
            channels: BTreeMap::from([(
                CHANNEL.to_string(),
                BuzzCursor {
                    chat_type: "dm".to_string(),
                    last_ts: NOW,
                    seen: vec!["aa".repeat(32)],
                },
            )]),
        };
        assert_eq!(adapter.restore_cursors(&foreign), 0);
        // The channel exists only because it is configured; nothing from the document was applied.
        let cursor = adapter.cursors();
        let channel = cursor.channels.get(CHANNEL).expect("seeded cursor");
        assert_eq!(channel.last_ts, 0);
        assert!(channel.seen.is_empty());
        assert_eq!(channel.chat_type, "group");

        // Same identity, other relay: also ignored.
        let other_relay = BuzzCursorStore {
            identity: keys().public_key_hex().to_string(),
            relay: "https://elsewhere.example.com".to_string(),
            channels: foreign.channels.clone(),
        };
        assert_eq!(adapter.restore_cursors(&other_relay), 0);
    }

    // -----------------------------------------------------------------------
    // Frame routing: EVENT / EOSE / CLOSED / NOTICE
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn event_frames_are_parsed_and_dispatched() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let event = from_other(
            BUZZ_CHAT_KIND,
            vec![channel_tag(), p_tag_self()],
            "@Vikram hello there",
            NOW,
        );
        socket.script(vec![event_frame(&subscription_id(0), &event)]);

        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert_eq!(dispatched.text, "hello there");
        assert_eq!(dispatched.message_id.as_deref(), Some(event.id.as_str()));
        assert_eq!(dispatched.source.chat_id, CHANNEL);
        assert_eq!(dispatched.source.chat_name.as_deref(), Some("general"));
        assert_eq!(dispatched.source.chat_type, "group");
        assert_eq!(dispatched.user_id.as_deref(), Some(OTHER_PUBKEY_HEX));
        assert_eq!(dispatched.user_name.as_deref(), Some(&OTHER_NPUB[..16]));
        assert_eq!(dispatched.source.platform.as_str(), BUZZ_PLATFORM);
        assert!(dispatched.raw_message.is_some());
    }

    #[tokio::test]
    async fn eose_and_notice_are_recorded_and_ignored() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        socket.script(vec![
            json!(["EOSE", subscription_id(0)]).to_string(),
            json!(["NOTICE", "welcome to the relay"]).to_string(),
            json!(["COUNT", subscription_id(0), {"count": 3}]).to_string(),
        ]);
        assert!(adapter.next_event().await.expect("no error").is_none());
        assert!(adapter.reached_eose());
        assert_eq!(
            adapter.last_notice().as_deref(),
            Some("welcome to the relay")
        );
    }

    #[tokio::test]
    async fn closed_drops_a_permanently_rejected_channel_but_fails_otherwise() {
        // Permanent: the channel is dropped and the stream continues.
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        socket.script(vec![
            json!([
                "CLOSED",
                subscription_id(0),
                "restricted: not a channel member"
            ])
            .to_string(),
            json!(["EOSE", subscription_id(0)]).to_string(),
        ]);
        assert!(
            adapter
                .next_event()
                .await
                .expect("no stream error")
                .is_none()
        );
        assert_eq!(adapter.restricted_channels(), vec![CHANNEL.to_string()]);
        assert!(adapter.subscriptions().is_empty());
        // Its cursor is gone, so a late event for it cannot be mistaken for a live conversation.
        assert!(!adapter.cursors().channels.contains_key(CHANNEL));

        // Not permanent: the stream fails so the host reconnects.
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        socket.script(vec![
            json!(["CLOSED", subscription_id(0), "error: relay is restarting"]).to_string(),
        ]);
        assert_eq!(
            adapter.next_event().await,
            Err(BuzzError::SubscriptionClosed {
                subscription: subscription_id(0),
                reason: "error: relay is restarting".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn malformed_frames_and_bad_events_do_not_stop_the_stream() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let good = from_other(
            BUZZ_CHAT_KIND,
            vec![channel_tag(), p_tag_self()],
            "@Vikram ok",
            NOW + 1,
        );
        let mut bad_sig = good.clone();
        bad_sig.created_at = NOW + 2;
        bad_sig.content = "@Vikram forged".to_string();
        // Re-derive the id but keep the old signature: the id happens to match nothing else, so the
        // signature check is what rejects this one.
        let mut forged = bad_sig.clone();
        forged.id = forged.computed_id();

        socket.script(vec![
            "not json at all".to_string(),
            "{}".to_string(),
            json!(["EVENT", subscription_id(0)]).to_string(),
            json!(["EVENT", "unknown-subscription", good.to_value()]).to_string(),
            event_frame(&subscription_id(0), &forged),
            event_frame(&subscription_id(0), &good),
        ]);
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert_eq!(dispatched.text, "ok");
        assert_eq!(adapter.rejected_events(), 1);
    }

    // -----------------------------------------------------------------------
    // Dispatch: dedup, cursors, gating, DMs, echoes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn duplicate_event_ids_dispatch_once() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let event = from_other(
            BUZZ_CHAT_KIND,
            vec![channel_tag(), p_tag_self()],
            "@Vikram once",
            NOW,
        );
        socket.script(vec![
            event_frame(&subscription_id(0), &event),
            event_frame(&subscription_id(0), &event),
        ]);
        assert!(adapter.next_event().await.expect("no error").is_some());
        assert!(adapter.next_event().await.expect("no error").is_none());
    }

    #[tokio::test]
    async fn cursors_advance_and_a_restart_resumes_from_them() {
        let socket = MockSocket::new(vec![]);
        let config = test_config();
        let adapter = connected(&socket, config.clone()).await;
        let event = from_other(
            BUZZ_CHAT_KIND,
            vec![channel_tag(), p_tag_self()],
            "@Vikram first",
            NOW + 10,
        );
        socket.script(vec![event_frame(&subscription_id(0), &event)]);
        assert!(adapter.next_event().await.expect("no error").is_some());

        let store = adapter.cursors();
        assert!(store.belongs_to(keys().public_key_hex(), RELAY_URL));
        let cursor = store.channels.get(CHANNEL).expect("cursor");
        assert_eq!(cursor.last_ts, NOW + 10);
        assert_eq!(cursor.seen, vec![event.id.clone()]);
        // The persisted document is exactly the reference's shape.
        assert_eq!(
            store.to_json(),
            json!({
                "identity": GOLDEN_PUBKEY_HEX,
                "relay": RELAY_URL,
                "channels": {CHANNEL: {"chat_type": "group", "last_ts": NOW + 10, "seen": [event.id]}},
            })
        );

        // Restart: the cursor is restored, the subscription resumes one second early and the
        // replayed event is a duplicate rather than a second dispatch.
        let socket = MockSocket::new(vec![]);
        let restarted = adapter_over(&socket, config);
        assert_eq!(
            restarted.restore_cursors(
                &BuzzCursorStore::from_json(&store.to_json()).expect("round trip")
            ),
            1
        );
        socket.script(auth_exchange(true, ""));
        restarted.connect_session().await.expect("connected");
        let requests = socket.sent_of("REQ");
        assert_eq!(requests[0][2]["since"], json!(NOW + 9));

        socket.script(vec![
            event_frame(&subscription_id(0), &event),
            event_frame(
                &subscription_id(0),
                &from_other(
                    BUZZ_CHAT_KIND,
                    vec![channel_tag(), p_tag_self()],
                    "@Vikram second",
                    NOW + 20,
                ),
            ),
        ]);
        let dispatched = restarted
            .next_event()
            .await
            .expect("no error")
            .expect("new event");
        assert_eq!(dispatched.text, "second");
        assert_eq!(
            restarted
                .cursors()
                .channels
                .get(CHANNEL)
                .expect("cursor")
                .last_ts,
            NOW + 20
        );
    }

    #[tokio::test]
    async fn mention_gating_and_leading_mention_stripping() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;

        // Un-addressed traffic in a channel is not dispatched.
        socket.script(vec![event_frame(
            &subscription_id(0),
            &from_other(BUZZ_CHAT_KIND, vec![channel_tag()], "just chatting", NOW),
        )]);
        assert!(adapter.next_event().await.expect("no error").is_none());

        // `@Name` addresses us; the leading mention is stripped so a command survives.
        socket.script(vec![event_frame(
            &subscription_id(0),
            &from_other(
                BUZZ_CHAT_KIND,
                vec![channel_tag()],
                "@Vikram /whoami",
                NOW + 1,
            ),
        )]);
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert_eq!(dispatched.text, "/whoami");
        assert!(dispatched.is_command());

        // A p tag addresses us without any mention.
        socket.script(vec![event_frame(
            &subscription_id(0),
            &from_other(
                BUZZ_CHAT_KIND,
                vec![channel_tag(), p_tag_self()],
                "ping",
                NOW + 2,
            ),
        )]);
        assert_eq!(
            adapter
                .next_event()
                .await
                .expect("no error")
                .expect("event")
                .text,
            "ping"
        );

        // An npub mention works too.
        socket.script(vec![event_frame(
            &subscription_id(0),
            &from_other(
                BUZZ_CHAT_KIND,
                vec![channel_tag()],
                &format!("{OTHER_NPUB} — actually {GOLDEN_NPUB} are you there?"),
                NOW + 3,
            ),
        )]);
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert!(
            dispatched.text.contains(GOLDEN_NPUB),
            "a mid-sentence mention stays"
        );

        // `email@Vikram` is not a mention: the reference's lookbehind requires a non-word char.
        socket.script(vec![event_frame(
            &subscription_id(0),
            &from_other(
                BUZZ_CHAT_KIND,
                vec![channel_tag()],
                "write to email@Vikram please",
                NOW + 4,
            ),
        )]);
        assert!(adapter.next_event().await.expect("no error").is_none());
    }

    #[tokio::test]
    async fn a_reply_to_our_own_message_is_addressed_even_without_a_mention() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let own = sign_event(
            &keys(),
            BUZZ_CHAT_KIND,
            vec![channel_tag()],
            "my own answer",
            NOW,
        )
        .expect("own event");
        let reply = from_other(
            BUZZ_CHAT_KIND,
            vec![
                channel_tag(),
                vec!["e".into(), own.id.clone(), RELAY_URL.into(), "reply".into()],
                p_tag_self(),
            ],
            "and what about this?",
            NOW + 1,
        );
        socket.script(vec![
            event_frame(&subscription_id(0), &own),
            event_frame(&subscription_id(0), &reply),
        ]);
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert_eq!(dispatched.text, "and what about this?");
        assert_eq!(
            dispatched.reply_to_message_id.as_deref(),
            Some(own.id.as_str())
        );
        assert_eq!(dispatched.reply_to_text.as_deref(), Some("my own answer"));
        assert_eq!(
            dispatched.reply_to_author_id.as_deref(),
            Some(GOLDEN_PUBKEY_HEX)
        );
        assert!(dispatched.reply_to_is_own_message);
    }

    #[tokio::test]
    async fn self_echo_is_skipped() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let echo = sign_event(
            &keys(),
            BUZZ_CHAT_KIND,
            vec![channel_tag()],
            "@Vikram my own words",
            NOW,
        )
        .expect("own event");
        socket.script(vec![event_frame(&subscription_id(0), &echo)]);
        assert!(adapter.next_event().await.expect("no error").is_none());
        // The echo still advanced the cursor: it is our own event, not an unseen one.
        assert_eq!(
            adapter
                .cursors()
                .channels
                .get(CHANNEL)
                .expect("cursor")
                .last_ts,
            NOW
        );
    }

    #[tokio::test]
    async fn dms_dispatch_without_a_mention_and_the_latch_sticks() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, dm_config()).await;
        // Kind 9, from someone else, p-tagged to us, text without a mention → a DM.
        socket.script(vec![event_frame(
            &subscription_id(0),
            &from_other(
                BUZZ_CHAT_KIND,
                vec![channel_tag(), p_tag_self()],
                "are you there?",
                NOW,
            ),
        )]);
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert_eq!(dispatched.source.chat_type, "dm");
        assert_eq!(
            adapter.get_chat_info("dm-channel").await.chat_type,
            "dm",
            "the conversation latched to dm"
        );

        // The latch sticks: a later message with no p tag and no mention still dispatches.
        socket.script(vec![event_frame(
            &subscription_id(0),
            &from_other(BUZZ_CHAT_KIND, vec![channel_tag()], "following up", NOW + 1),
        )]);
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert_eq!(dispatched.text, "following up");
        assert_eq!(dispatched.source.chat_type, "dm");
    }

    #[tokio::test]
    async fn a_named_channel_keeps_p_tagged_traffic_as_group_traffic() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        // Same shape as the DM case, but the channel has a name and a description, so it stays a
        // group conversation and only the p tag addresses us.
        socket.script(vec![
            event_frame(
                &subscription_id(0),
                &from_other(
                    BUZZ_CHAT_KIND,
                    vec![channel_tag(), p_tag_self()],
                    "hey",
                    NOW,
                ),
            ),
            event_frame(
                &subscription_id(0),
                &from_other(BUZZ_CHAT_KIND, vec![channel_tag()], "unaddressed", NOW + 1),
            ),
        ]);
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert_eq!(dispatched.source.chat_type, "group");
        assert_eq!(dispatched.text, "hey");
        assert!(adapter.next_event().await.expect("no error").is_none());
        assert_eq!(adapter.get_chat_info(CHANNEL).await.chat_type, "group");
    }

    #[tokio::test]
    async fn housekeeping_kinds_only_advance_the_cursor() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let housekeeping = from_other(44_100, vec![channel_tag(), p_tag_self()], "", NOW + 5);
        socket.script(vec![event_frame(&subscription_id(0), &housekeeping)]);
        assert!(adapter.next_event().await.expect("no error").is_none());
        assert_eq!(
            adapter
                .cursors()
                .channels
                .get(CHANNEL)
                .expect("cursor")
                .last_ts,
            NOW + 5
        );
    }

    #[tokio::test]
    async fn an_allowlist_gates_dispatch_and_reaction_only_senders_get_an_ack() {
        let config = quick(
            BuzzConfig::new(RELAY_URL)
                .with_channel(BuzzChannel::with_metadata(CHANNEL, "general", "a channel"))
                .with_display_name("Vikram")
                .acknowledging_receipts(false)
                .allowing(&[&"cc".repeat(32)])
                .reacting_only(&[OTHER_PUBKEY_HEX]),
        );
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, config).await;
        socket.script(vec![event_frame(
            &subscription_id(0),
            &from_other(
                BUZZ_CHAT_KIND,
                vec![channel_tag(), p_tag_self()],
                "@Vikram hello",
                NOW,
            ),
        )]);
        // Not dispatched from a non-allowed sender...
        assert!(adapter.next_event().await.expect("no error").is_none());
        // ...but a reaction-only sender that addresses us gets the reference's 👀.
        let reactions: Vec<Value> = socket
            .sent_of("EVENT")
            .into_iter()
            .filter(|frame| frame[1]["kind"] == json!(BUZZ_REACTION_KIND))
            .collect();
        assert_eq!(
            reactions.len(),
            1,
            "expected one reaction, got {reactions:?}"
        );
        assert_eq!(reactions[0][1]["content"], json!(BUZZ_SEEN_EMOJI));
        assert_eq!(reactions[0][1]["tags"][0][0], json!("e"));
    }

    // -----------------------------------------------------------------------
    // Media markers
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn imeta_attachments_become_markers_and_message_types() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let digest = "b".repeat(64);
        let event = from_other(
            BUZZ_CHAT_KIND,
            vec![
                channel_tag(),
                p_tag_self(),
                vec![
                    "imeta".to_string(),
                    format!("url https://relay.buzz.example.com/media/{digest}.png"),
                    format!("x {digest}"),
                    "size 12".to_string(),
                    "filename shot.png".to_string(),
                    "m image/png".to_string(),
                ],
            ],
            "@Vikram look",
            NOW,
        );
        socket.script(vec![event_frame(&subscription_id(0), &event)]);
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert_eq!(dispatched.message_type, MessageType::Photo);
        assert_eq!(
            dispatched.text,
            "look\n[1 Buzz attachment(s) not fetched by this gateway: shot.png (image/png, 12 bytes)]"
        );
        assert!(dispatched.media_urls.is_empty(), "nothing was downloaded");

        // Mixed attachment kinds fall back to document semantics.
        let audio = vec![
            "imeta".to_string(),
            format!("url https://relay.buzz.example.com/media/{digest}.mp3"),
            format!("x {digest}"),
            "size 5".to_string(),
            "filename clip.mp3".to_string(),
            "m audio/mpeg".to_string(),
        ];
        let image = vec![
            "imeta".to_string(),
            format!("url https://relay.buzz.example.com/media/{digest}.png"),
            format!("x {digest}"),
            "size 5".to_string(),
            "filename shot.png".to_string(),
            "m image/png".to_string(),
        ];
        let mixed = from_other(
            BUZZ_CHAT_KIND,
            vec![channel_tag(), p_tag_self(), image, audio],
            "@Vikram both of these",
            NOW + 1,
        );
        socket.script(vec![event_frame(&subscription_id(0), &mixed)]);
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert_eq!(dispatched.message_type, MessageType::Document);
    }

    #[tokio::test]
    async fn malformed_imeta_metadata_is_reported_as_rejected() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let event = from_other(
            BUZZ_CHAT_KIND,
            vec![
                channel_tag(),
                p_tag_self(),
                // Plain http and a bad digest: the reference refuses both.
                vec![
                    "imeta".to_string(),
                    "url http://relay.buzz.example.com/media/x.png".to_string(),
                    format!("x {}", "z".repeat(64)),
                    "size 12".to_string(),
                ],
            ],
            "@Vikram look",
            NOW,
        );
        socket.script(vec![event_frame(&subscription_id(0), &event)]);
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        assert_eq!(dispatched.message_type, MessageType::Text);
        assert_eq!(
            dispatched.text,
            "look\n[1 Buzz attachment(s) rejected as malformed or over limits.]"
        );
    }

    // -----------------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn send_publishes_a_kind_9_reply_with_nip10_tags() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let parent = from_other(
            BUZZ_CHAT_KIND,
            vec![channel_tag(), p_tag_self()],
            "@Vikram what is the plan?",
            NOW,
        );
        socket.script(vec![event_frame(&subscription_id(0), &parent)]);
        assert!(adapter.next_event().await.expect("no error").is_some());

        let expected = sign_event(
            &keys(),
            BUZZ_CHAT_KIND,
            vec![
                vec!["h".to_string(), CHANNEL.to_string()],
                vec![
                    "e".to_string(),
                    parent.id.clone(),
                    RELAY_URL.to_string(),
                    "reply".to_string(),
                ],
                vec!["p".to_string(), OTHER_PUBKEY_HEX.to_string()],
            ],
            "the plan is on",
            NOW,
        )
        .expect("expected event");
        socket.script(vec![ok_frame(&expected.id, true, "")]);

        let result = adapter
            .send(CHANNEL, "the plan is on", Some(&parent.id), None)
            .await;
        assert!(result.success, "send failed: {:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some(expected.id.as_str()));

        let published: Vec<Value> = socket
            .sent_of("EVENT")
            .into_iter()
            .filter(|frame| frame[1]["kind"] == json!(BUZZ_CHAT_KIND))
            .collect();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0][1], expected.to_value());
        // The published id is marked seen, so its echo is not dispatched.
        assert!(
            adapter
                .cursors()
                .channels
                .get(CHANNEL)
                .expect("cursor")
                .seen
                .contains(&expected.id)
        );
    }

    #[tokio::test]
    async fn send_joins_the_thread_root_instead_of_nesting() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let root = from_other(
            BUZZ_CHAT_KIND,
            vec![channel_tag(), p_tag_self()],
            "@Vikram start",
            NOW,
        );
        let in_thread = from_other(
            BUZZ_CHAT_KIND,
            vec![
                channel_tag(),
                vec!["e".into(), root.id.clone(), "".into(), "root".into()],
                vec!["e".into(), "middle-id".into(), "".into(), "reply".into()],
                p_tag_self(),
            ],
            "@Vikram follow up",
            NOW + 1,
        );
        socket.script(vec![
            event_frame(&subscription_id(0), &root),
            event_frame(&subscription_id(0), &in_thread),
        ]);
        assert!(adapter.next_event().await.expect("no error").is_some());
        let dispatched = adapter
            .next_event()
            .await
            .expect("no error")
            .expect("event");
        // The session is scoped to the ROOT, not the message that triggered the turn.
        assert_eq!(
            dispatched.source.thread_id.as_deref(),
            Some(root.id.as_str())
        );

        // The anchor is the trigger's id; `_resolve_reply_anchor` turns it into the thread root, so
        // the reply carries the root and the parent it actually answers.
        let expected = sign_event(
            &keys(),
            BUZZ_CHAT_KIND,
            vec![
                vec!["h".to_string(), CHANNEL.to_string()],
                vec![
                    "e".to_string(),
                    root.id.clone(),
                    RELAY_URL.to_string(),
                    "root".to_string(),
                ],
                vec![
                    "e".to_string(),
                    in_thread.id.clone(),
                    RELAY_URL.to_string(),
                    "reply".to_string(),
                ],
                vec!["p".to_string(), OTHER_PUBKEY_HEX.to_string()],
            ],
            "answering inside the thread",
            NOW,
        )
        .expect("expected event");
        socket.script(vec![ok_frame(&expected.id, true, "")]);
        let trigger_id = in_thread.id.clone();
        let result = adapter
            .send(
                CHANNEL,
                "answering inside the thread",
                Some(&trigger_id),
                None,
            )
            .await;
        assert!(result.success, "send failed: {:?}", result.error);
        let published: Vec<Value> = socket
            .sent_of("EVENT")
            .into_iter()
            .filter(|frame| frame[1]["kind"] == json!(BUZZ_CHAT_KIND))
            .collect();
        assert_eq!(published[0][1], expected.to_value());
        assert_eq!(published[0][1]["tags"][1][1], json!(root.id));
        assert_eq!(published[0][1]["tags"][1][3], json!("root"));
        assert_eq!(published[0][1]["tags"][2][1], json!(trigger_id));
        assert_eq!(published[0][1]["tags"][2][3], json!("reply"));
    }

    #[tokio::test]
    async fn send_chunks_long_content_and_awaits_each_ok() {
        let socket = MockSocket::new(vec![]);
        let config = test_config();
        let max = config.max_message_length;
        let adapter = connected(&socket, config).await;
        let content = "x".repeat(max + 100);
        let chunks = truncate_message(&content, max, codepoint_len);
        assert_eq!(chunks.len(), 2, "the fixture must need two chunks");

        let mut expected_ids = Vec::new();
        for chunk in &chunks {
            let expected = sign_event(&keys(), BUZZ_CHAT_KIND, vec![channel_tag()], chunk, NOW)
                .expect("expected event");
            socket.script(vec![ok_frame(&expected.id, true, "")]);
            expected_ids.push(expected.id);
        }
        let result = adapter.send(CHANNEL, &content, None, None).await;
        assert!(result.success, "send failed: {:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some(expected_ids[1].as_str()));
        assert_eq!(result.continuation_message_ids, expected_ids);
        let published: Vec<Value> = socket
            .sent_of("EVENT")
            .into_iter()
            .filter(|frame| frame[1]["kind"] == json!(BUZZ_CHAT_KIND))
            .collect();
        assert_eq!(published.len(), 2);
        // Later chunks stay in the channel without repeating the reply tags.
        assert_eq!(published[1][1]["tags"], json!([["h", CHANNEL]]));
    }

    #[tokio::test]
    async fn send_surfaces_a_relay_rejection_as_a_failure() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let expected = sign_event(
            &keys(),
            BUZZ_CHAT_KIND,
            vec![channel_tag()],
            "blocked text",
            NOW,
        )
        .expect("expected event");
        socket.script(vec![ok_frame(
            &expected.id,
            false,
            "blocked: not a channel member",
        )]);

        let result = adapter.send(CHANNEL, "blocked text", None, None).await;
        assert!(!result.success);
        assert_eq!(
            result.error.as_deref(),
            Some("blocked: not a channel member")
        );
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(result.message_id.is_none());
    }

    #[tokio::test]
    async fn send_reports_a_silent_relay_as_retryable() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        // The relay stops answering: the publish is never acknowledged, so it is not a delivery.
        socket.stall();
        let result = adapter.send(CHANNEL, "into the void", None, None).await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(
            result
                .error
                .is_some_and(|error| error.contains("timed out"))
        );
    }

    #[tokio::test]
    async fn send_refuses_empty_input_without_touching_the_socket() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let sent_before = socket.sent().len();
        assert!(!adapter.send(CHANNEL, "", None, None).await.success);
        assert!(!adapter.send("  ", "hello", None, None).await.success);
        assert_eq!(socket.sent().len(), sent_before);
    }

    #[tokio::test]
    async fn send_anchors_on_metadata_before_the_reply_argument() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let metadata = json!({"reply_to_message_id": "stream-anchor"});
        let expected = sign_event(
            &keys(),
            BUZZ_CHAT_KIND,
            vec![
                vec!["h".to_string(), CHANNEL.to_string()],
                vec![
                    "e".to_string(),
                    "stream-anchor".to_string(),
                    RELAY_URL.to_string(),
                    "reply".to_string(),
                ],
            ],
            "chunk",
            NOW,
        )
        .expect("expected event");
        socket.script(vec![ok_frame(&expected.id, true, "")]);
        let result = adapter
            .send(CHANNEL, "chunk", Some("other-anchor"), Some(metadata))
            .await;
        assert!(result.success, "send failed: {:?}", result.error);
    }

    // -----------------------------------------------------------------------
    // Reactions
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reactions_publish_kind_7_and_report_acceptance() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let target = from_other(
            BUZZ_CHAT_KIND,
            vec![channel_tag(), p_tag_self()],
            "@Vikram hi",
            NOW,
        );
        socket.script(vec![event_frame(&subscription_id(0), &target)]);
        assert!(adapter.next_event().await.expect("no error").is_some());

        let expected = sign_event(
            &keys(),
            BUZZ_REACTION_KIND,
            vec![
                vec!["e".to_string(), target.id.clone()],
                vec!["p".to_string(), OTHER_PUBKEY_HEX.to_string()],
            ],
            "\u{1f44d}",
            NOW,
        )
        .expect("expected reaction");
        socket.script(vec![ok_frame(&expected.id, true, "")]);
        assert!(adapter.send_reaction(&target.id, "\u{1f44d}").await);

        let reactions: Vec<Value> = socket
            .sent_of("EVENT")
            .into_iter()
            .filter(|frame| frame[1]["kind"] == json!(BUZZ_REACTION_KIND))
            .collect();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0][1], expected.to_value());

        // A rejected reaction is reported as not delivered.
        let rejected = sign_event(
            &keys(),
            BUZZ_REACTION_KIND,
            vec![
                vec!["e".to_string(), target.id.clone()],
                vec!["p".to_string(), OTHER_PUBKEY_HEX.to_string()],
            ],
            "\u{1f44d}",
            NOW,
        )
        .expect("expected reaction");
        socket.script(vec![ok_frame(
            &rejected.id,
            false,
            "rate-limited: slow down",
        )]);
        assert!(!adapter.send_reaction(&target.id, "\u{1f44d}").await);
        assert!(
            adapter
                .last_error()
                .is_some_and(|error| error.contains("rate-limited"))
        );

        // Blank inputs never publish.
        let before = socket.sent().len();
        assert!(!adapter.send_reaction("", "\u{1f44d}").await);
        assert!(!adapter.send_reaction(&target.id, "").await);
        assert_eq!(socket.sent().len(), before);
    }

    #[tokio::test]
    async fn a_delivered_event_gets_the_seen_acknowledgement() {
        let config = quick(
            BuzzConfig::new(RELAY_URL)
                .with_channel(BuzzChannel::with_metadata(CHANNEL, "general", "a channel"))
                .with_display_name("Vikram"),
        );
        assert!(
            config.send_seen_reaction,
            "the reference's 👀 is on by default"
        );
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, config).await;
        let event = from_other(
            BUZZ_CHAT_KIND,
            vec![channel_tag(), p_tag_self()],
            "@Vikram hi",
            NOW,
        );
        socket.script(vec![event_frame(&subscription_id(0), &event)]);
        assert!(adapter.next_event().await.expect("no error").is_some());

        let reactions: Vec<Value> = socket
            .sent_of("EVENT")
            .into_iter()
            .filter(|frame| frame[1]["kind"] == json!(BUZZ_REACTION_KIND))
            .collect();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0][1]["content"], json!(BUZZ_SEEN_EMOJI));
        assert_eq!(reactions[0][1]["tags"][0][1], json!(event.id));
    }

    // -----------------------------------------------------------------------
    // Adapter surface
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn chat_info_and_capabilities() {
        let socket = MockSocket::new(vec![]);
        let adapter = connected(&socket, test_config()).await;
        let info = adapter.get_chat_info(CHANNEL).await;
        assert_eq!(info.name, "general");
        assert_eq!(info.chat_type, "group");
        let unknown = adapter.get_chat_info("unknown-channel").await;
        assert_eq!(unknown.name, "unknown-channel");
        assert_eq!(unknown.chat_type, "group");

        assert!(PlatformAdapter::supports_code_blocks(&adapter));
        assert!(PlatformAdapter::splits_long_messages(&adapter));
        assert_eq!(
            PlatformAdapter::max_message_length(&adapter),
            BUZZ_MAX_MESSAGE_LENGTH
        );
        assert_eq!(adapter.pubkey_hex(), GOLDEN_PUBKEY_HEX);
        assert_eq!(adapter.npub(), GOLDEN_NPUB);
        // No typing indicator and no edit path exist without the CLI.
        adapter.send_typing(CHANNEL, None).await;
        assert!(
            !adapter
                .edit_message(CHANNEL, "id", "text", false)
                .await
                .success
        );
    }

    #[tokio::test]
    async fn disconnect_keeps_the_cursors_for_the_host_to_persist() {
        let socket = MockSocket::new(vec![]);
        let mut adapter = connected(&socket, test_config()).await;
        socket.script(vec![event_frame(
            &subscription_id(0),
            &from_other(
                BUZZ_CHAT_KIND,
                vec![channel_tag(), p_tag_self()],
                "@Vikram hi",
                NOW + 3,
            ),
        )]);
        assert!(adapter.next_event().await.expect("no error").is_some());
        adapter.disconnect().await;
        assert!(!adapter.is_connected());
        assert!(adapter.subscriptions().is_empty());
        assert_eq!(
            adapter
                .cursors()
                .channels
                .get(CHANNEL)
                .expect("cursor")
                .last_ts,
            NOW + 3
        );
    }

    #[test]
    fn mention_boundaries_match_the_reference_regexes() {
        let identity = Identity::new(&keys(), &test_config());
        assert!(is_mentioned("@Vikram hi", &identity));
        assert!(is_mentioned("hi @Vikram", &identity));
        assert!(is_mentioned("hi @Vikram!", &identity));
        assert!(!is_mentioned("hi @Vikramite", &identity));
        assert!(!is_mentioned("mail me at email@Vikram", &identity));
        assert!(!is_mentioned("@@Vikram", &identity));
        assert!(is_mentioned(GOLDEN_NPUB, &identity));
        assert!(is_mentioned(GOLDEN_PUBKEY_HEX, &identity));
        assert!(!is_mentioned(&format!("ff{GOLDEN_PUBKEY_HEX}"), &identity));
        assert!(!is_mentioned("nothing here", &identity));

        assert_eq!(
            strip_leading_mention("@Vikram /whoami", &identity),
            "/whoami"
        );
        assert_eq!(
            strip_leading_mention("@Vikram: /whoami", &identity),
            "/whoami"
        );
        assert_eq!(
            strip_leading_mention(&format!("@{GOLDEN_NPUB} hello"), &identity),
            "hello"
        );
        assert_eq!(
            strip_leading_mention("hey @Vikram", &identity),
            "hey @Vikram"
        );
        assert_eq!(strip_leading_mention("  @vikram   bye  ", &identity), "bye");
    }

    #[test]
    fn attachment_parsing_enforces_the_reference_limits() {
        let digest = "c".repeat(64);
        let valid = |size: &str, url: &str| {
            vec![
                "imeta".to_string(),
                format!("url {url}"),
                format!("x {digest}"),
                format!("size {size}"),
                "filename a.txt".to_string(),
                "m text/plain".to_string(),
            ]
        };
        let good_url = format!("https://relay.buzz.example.com/media/{digest}.txt");
        let (accepted, rejected) = parse_imeta_attachments(&[valid("10", &good_url)]);
        assert_eq!(accepted.len(), 1);
        assert_eq!(rejected, 0);
        assert_eq!(accepted[0].message_type(), MessageType::Document);
        assert_eq!(accepted[0].safe_filename(), "a.txt");

        // http, a bad digest, a zero size and an oversized one are all refused.
        let (accepted, rejected) = parse_imeta_attachments(&[
            valid("10", "http://relay.buzz.example.com/media/x.txt"),
            valid("0", &good_url),
            valid(&(BUZZ_MAX_ATTACHMENT_BYTES + 1).to_string(), &good_url),
            valid("ten", &good_url),
        ]);
        assert!(accepted.is_empty());
        assert_eq!(rejected, 4);

        // Only the first four attachments are considered; the rest are counted as rejected.
        let many: Vec<Vec<String>> = (0..6).map(|_| valid("1", &good_url)).collect();
        let (accepted, rejected) = parse_imeta_attachments(&many);
        assert_eq!(accepted.len(), BUZZ_MAX_ATTACHMENTS);
        assert_eq!(rejected, 2);
    }
}
