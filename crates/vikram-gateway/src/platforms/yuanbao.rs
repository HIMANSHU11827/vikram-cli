//! Yuanbao (Tencent 元宝) adapter — `ConnMsg` protobuf frames over a binary WebSocket.
//!
//! Rust port of `research/hermes-platforms/yuanbao/` (`yuanbao_proto.py`, `adapter.py`,
//! `register.py`, `gateway_env.py`, `yuanbao_media.py`). The wire format is hand-written here
//! exactly as the reference hand-writes it — field numbers, varint rules, 64-bit two's-complement
//! negative ints, repeated fields and embedded messages — with no protobuf runtime and no new
//! dependency (`prost` is deliberately not used).
//!
//! # Ported behaviour (with the reference it comes from)
//!
//! - **Codec** (`yuanbao_proto.py`): every message type the reference actually reads or writes.
//!   - `ConnMsg { Head head = 1; bytes data = 2 }` — `encode_conn_msg[_full]` / `decode_conn_msg`
//!     (proto lines 245–262).
//!   - `Head { 1 cmd_type, 2 cmd, 3 seq_no, 4 msg_id, 5 module, 6 need_ack, 10 status }` —
//!     `_encode_head` / `_decode_head` (lines 228–242). `cmd_type` 0/1/2/3 = request / response /
//!     push / push-ack (line 20).
//!   - `AuthBindReq { 1 biz_id, 2 AuthInfo{1 uid,2 source,3 token}, 3 DeviceInfo{1 app_version,
//!     2 app_operation_system, 10 instance_id, 24 bot_version}, 5 env_name }` and
//!     `AuthBindRsp { 1 code, 2 message, 3 connectId }` (lines 442–458, and the `_extract_connect_id`
//!     reader at adapter.py:1698–1711).
//!   - `PingReq` / `PingRsp` — empty messages (`encode_ping`, line 461).
//!   - `PushAck` — echoes `cmd` / `msg_id` / `module` of the push head (`encode_push_ack`, line 466).
//!   - `DirectedPush`, i.e. a `ConnMsg` with `cmd_type = Push` whose `data` is an
//!     `InboundMessagePush` payload, decoded into [`PushMsg`] (`_INBOUND_PUSH_SPEC`,
//!     `decode_inbound_push`, lines 345–366) together with its `MsgBodyElement` / `MsgContent` /
//!     `ImageInfo` / `ext_map` sub-messages (lines 300–338).
//!   - Outbound `SendC2CMessageReq` / `SendGroupMessageReq` (lines 413–436), `SendPrivateHeartbeatReq`
//!     / `SendGroupHeartbeatReq` (lines 476–488), all wrapped by `encode_biz_msg`
//!     (service = `yuanbao_openclaw_proxy`, `head.cmd` = method, `head.module` = service; lines
//!     273–290).
//! - **Signing** (`SignManager`): `HMAC-SHA256(key = app_secret, msg = nonce + timestamp + app_id +
//!   app_secret).hexdigest()` (adapter.py:169–172) over a Beijing-time `%Y-%m-%dT%H:%M:%S+08:00`
//!   stamp (adapter.py:174–177), POSTed as `{app_key, nonce, signature, timestamp}` to
//!   `{api_domain}/api/v5/robotLogic/sign-token` (adapter.py:130–134, 194–223) with the reference's
//!   headers, its `10099` retry and its `duration`-based cache.
//! - **Handshake** (`ConnectionManager.open` / `_authenticate`): sign token → `AUTH_BIND`
//!   (`biz_id = "ybBot"`, `source` from the token or `"bot"`, `uid` = bot id) → wait for the
//!   `auth-bind` response, take `connectId` from field 3 and treat a non-zero `code` as a failure
//!   (adapter.py:1588–1711).
//! - **Heartbeat** (`_heartbeat_loop`): a `PING` every [`HEARTBEAT_INTERVAL`], each answered by a
//!   `ping` response, with [`HEARTBEAT_TIMEOUT_THRESHOLD`] consecutive misses meaning "reconnect"
//!   (adapter.py:1713–1755). [`HeartbeatState`] is that policy as a pure state machine.
//! - **Push handling** (`_handle_frame`): `push` frames are ACKed when `head.need_ack` is set,
//!   their payload decoded, the bot's own messages dropped (`SkipSelfMiddleware`), message ids
//!   deduplicated for [`DEDUP_TTL`] (`DedupMiddleware` / `MessageDeduplicator`), and the rest turned
//!   into [`MessageEvent`]s with the reference's chat routing (`group:<code>` / `direct:<account>`),
//!   text extraction (`ExtractContentMiddleware._extract_text`), `@`-mention parsing
//!   (`GroupAtGuardMiddleware._iter_bot_mentions`), quoted-reply context from `cloud_custom_data`
//!   (`QuoteContextMiddleware`) and message-type classification
//!   (`ClassifyMessageTypeMiddleware`).
//! - **Sending** (`MessageSender.send_text` / `send_text_chunk` / `_send_msg_body`): trim, chunk to
//!   [`YUANBAO_MAX_MESSAGE_LENGTH`] code points, route `group:<code>` to `SendGroupMessage` (with the
//!   quoted message as `ref_msg_id`) and everything else to `SendC2CMessage`, request id
//!   `grp_<seq>` / `c2c_<seq>`, await the matching response frame, then emit a `FINISH` reply
//!   heartbeat (adapter.py:2434–2485, 2522–2543).
//! - **Outbound media** (`MediaSendHandler.handle` + `yuanbao_media.py`): `genUploadInfo` for
//!   temporary COS credentials (POST `{api_domain}/api/resource/genUploadInfo` with the reference's
//!   `X-Token` / `X-ID` / `X-Source` / `X-Route-Env` headers and `{fileName, fileId, docFrom,
//!   docOpenId}` body), a `q-sign-algorithm=sha1` HMAC-SHA1 authorization over the COS accelerate
//!   host, the `PUT` of the bytes with `Authorization` / `Content-Type` /
//!   `x-cos-security-token`, then the `TIMImageElem` (`build_image_msg_body`) or `TIMFileElem`
//!   (`build_file_msg_body`) message dispatched over the existing binary socket — with the caption
//!   appended as a `TIMTextElem` and `reply_to` as the group `ref_msg_id`. Validation is the
//!   reference's own: empty bytes and [`YUANBAO_MEDIA_MAX_SIZE_MB`] caps, checked before any I/O.
//!   The pure pieces ([`cos_sign`], [`cos_upload_plan`], [`parse_cos_credentials`],
//!   [`image_msg_body`], [`file_msg_body`], [`parse_image_size`]) are exposed so they are testable
//!   byte-for-byte against the reference.
//! - **Reply heartbeats** (`HeartbeatManager`): `RUNNING = 1` / `FINISH = 2` frames
//!   (`WS_HEARTBEAT_RUNNING/FINISH`, lines 35–36) via [`YuanbaoAdapter::send_heartbeat`].
//! - **Reconnect policy** (`_do_reconnect`): exponential backoff `min(2^attempt, 60)` s over
//!   [`MAX_RECONNECT_ATTEMPTS`] attempts, and the permanent close codes that must never reconnect
//!   (`NO_RECONNECT_CLOSE_CODES`, adapter.py:105).
//!
//! # Deliberate omissions (honest scope, not stubbed)
//!
//! Nothing below is faked: every one of these is absent rather than simulated, so no caller can
//! mistake a no-op for a delivered message.
//!
//! - **Media outbound from a remote URL**: `ImageUrlHandler` downloads the URL with
//!   `yuanbao_media.download_url` (SSRF-guarded, redirect-validated, size-capped streaming GET) and
//!   then uploads those bytes to COS. Nothing in this gateway hands a URL to the platform, and the
//!   download side is not ported here, so [`PlatformAdapter::send_media`] refuses a
//!   [`MediaSource::Url`] with a typed failure and issues no request at all rather than uploading
//!   something it never saw.
//! - **Native video / voice elements**: `TIMVideoFileElem` / `TIMSoundElem` body builders do not
//!   exist in the reference — `send_direct` sends every non-image attachment through
//!   `DocumentHandler` (adapter.py:2437–2439), so a video or audio payload travels as a
//!   `TIMFileElem`. Nothing is fabricated into an element the reference never writes.
//! - **Stickers**: `plugins/platforms/yuanbao/yuanbao_sticker.py` (sticker catalogue,
//!   `build_face_msg_body`, `build_sticker_msg_body`) is not part of the port, and inbound
//!   `TIMFaceElem` payloads are rendered as an `[emoji: name]` text marker only.
//! - **Inbound media download**: `MediaResolveMiddleware` — `resourceId` → `/api/resource/v1/download`
//!   exchange, SSRF-guarded fetch, `cache_image/video/document_from_bytes`, and the
//!   `PatchAnchorsMiddleware` rewrite of `[kind|ybres:RID]` anchors to local paths. Inbound media is
//!   surfaced as its `[image]` / `[voice]` / `[video]` / `[file: name]` marker text (with the
//!   `|ybres:<rid>` anchor when the URL carries a `resourceId`) for the agent to act on itself.
//! - **Forwarded chat records**: `elem_type 1009` deep parsing — `ext_map["wexin_forward_msg_…"]`
//!   is base64 `ForwardMsgData` protobuf (`decode_forward_msg_data`,
//!   `ForwardedRecordsParseMiddleware`). Only the summary text the element already carries is used.
//! - **Share / link cards**: `elem_type 1010` / `1007` rendering (`_format_shared_link`,
//!   `_format_link_understanding`) — these fall back to `[unsupported message type]`.
//! - **Group RPCs**: `query_group_info`, `get_group_member_list` and the member cache that backs
//!   outbound `@nickname` → `TIMCustomElem(1002)` rewriting (`_build_msg_body_with_mentions`). Group
//!   sends carry a single text element.
//! - **Multi-part debounce**: the 1.5 s per-sender `_inbound_buffer` / `_inbound_timers`
//!   aggregation in `ConnectionManager._push_to_inbound`.
//! - **Group dispatch queue, recall guard, slow-response notifier**: `_group_queues`,
//!   `RecallGuardMiddleware` (transcript redaction), `SlowResponseNotifier`. The runner owns
//!   ordering, transcripts and timers.
//! - **Access policy / pairing / home channel**: `AccessPolicy`, `AutoSetHomeMiddleware`,
//!   `OwnAccessPolicyMixin` and the `YUANBAO_DM_POLICY` / `YUANBAO_GROUP_POLICY` environment
//!   surface — `crate::access` and `crate::pairing` own admission in this gateway.
//! - **Background timers and profile scoping**: no task is spawned. The host owns the socket, the
//!   reconnect loop (the policy helpers [`reconnect_delay`] / [`should_reconnect`] are exported) and
//!   the ping cadence — it calls [`YuanbaoAdapter::ping_frame`] every
//!   [`YuanbaoConfig::heartbeat_interval`], [`YuanbaoAdapter::pong_received`] on a `ping` response
//!   and [`YuanbaoAdapter::pong_missed`] on a [`PONG_TIMEOUT`] miss. The `_active_instance` /
//!   `_active_instances` registry is not reproduced.
//! - **Cron wrapper stripping** (`strip_cron_wrapper`): the scheduler in `crate::delivery` owns that
//!   text shaping for every platform.
//!
//! # Three intentional divergences from the reference
//!
//! - `_parse_fields` silently short-slices a truncated frame (`data[pos:pos+length]`), so a cut-off
//!   frame decodes as a shorter message. This port returns [`CodecError::Truncated`] instead: a frame
//!   that arrived incomplete is a transport fault, not a message.
//! - `_dispatch_encoded` reports success for any response frame without looking at `head.status`.
//!   [`YuanbaoAdapter::send`] treats a non-zero `status` as a failed send, because a rejected request
//!   must not be counted as delivered. [`PlatformAdapter::send_media`] does the same.
//! - A media send with no sign token is refused locally, before `genUploadInfo`: the reference would
//!   POST with an empty `X-Token` and fail on a guaranteed `401`, which costs a round trip and reads
//!   as a credential problem at the far end rather than at this one.

use async_trait::async_trait;
use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest as sha1_digest};
use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource, platform_names};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::socket::BinarySocket;
use crate::transport::{HttpRequest, SharedTransport};

// ---------------------------------------------------------------------------
// Protocol constants (yuanbao_proto.py:15–36, adapter.py:93–124)
// ---------------------------------------------------------------------------

/// Default WebSocket gateway (adapter.py `DEFAULT_WS_GATEWAY_URL`).
pub const YUANBAO_WS_URL: &str = "wss://bot-wss.yuanbao.tencent.com/wss/connection";

/// Default API domain used for the sign-token exchange (adapter.py `DEFAULT_API_DOMAIN`).
pub const YUANBAO_API_DOMAIN: &str = "https://bot.yuanbao.tencent.com";

/// Sign-token path (`SignManager.TOKEN_PATH`).
pub const SIGN_TOKEN_PATH: &str = "/api/v5/robotLogic/sign-token";

/// How often a `PING` is sent while connected (`HEARTBEAT_INTERVAL_SECONDS`).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// How long a `PONG` may take before it counts as a miss (adapter.py:1636, 10 s).
pub const PONG_TIMEOUT: Duration = Duration::from_secs(10);

/// Consecutive missed pongs before the connection is considered dead (`HEARTBEAT_TIMEOUT_THRESHOLD`).
pub const HEARTBEAT_TIMEOUT_THRESHOLD: u32 = 2;

/// How long the `AUTH_BIND` response may take (`AUTH_TIMEOUT_SECONDS`).
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a business request may take for its response (`DEFAULT_SEND_TIMEOUT`).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Reconnect attempts before giving up (`MAX_RECONNECT_ATTEMPTS`).
pub const MAX_RECONNECT_ATTEMPTS: u32 = 100;

/// Close codes that must never be reconnected (`NO_RECONNECT_CLOSE_CODES`).
pub const NO_RECONNECT_CLOSE_CODES: [u16; 6] = [4012, 4013, 4014, 4018, 4019, 4021];

/// Sign-token retryable business code (`SignManager.RETRYABLE_CODE`).
pub const RETRYABLE_TOKEN_CODE: i64 = 10_099;

/// Sign-token attempts after the first (`SignManager.MAX_RETRIES`).
pub const MAX_TOKEN_RETRIES: u32 = 3;

/// Delay between sign-token retries (`SignManager.RETRY_DELAY_S`).
pub const TOKEN_RETRY_DELAY: Duration = Duration::from_secs(1);

/// A cached sign token is refreshed this long before it expires (`CACHE_REFRESH_MARGIN_S`).
pub const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// Single-message character limit (`YuanbaoAdapter.MAX_TEXT_CHUNK`).
pub const YUANBAO_MAX_MESSAGE_LENGTH: usize = 4000;

/// How long an inbound message id stays "already seen" (`MessageDeduplicator(ttl_seconds=300)`).
pub const DEDUP_TTL: Duration = Duration::from_secs(300);

/// Upper bound on remembered message ids, so a long-lived socket cannot grow without limit.
pub const DEDUP_CAPACITY: usize = 4096;

/// `cmd_type` for an upstream request (`CMD_TYPE["Request"]`).
pub const CMD_TYPE_REQUEST: u64 = 0;
/// `cmd_type` for a response to a request (`CMD_TYPE["Response"]`).
pub const CMD_TYPE_RESPONSE: u64 = 1;
/// `cmd_type` for a downstream push (`CMD_TYPE["Push"]`).
pub const CMD_TYPE_PUSH: u64 = 2;
/// `cmd_type` for a push acknowledgement (`CMD_TYPE["PushAck"]`).
pub const CMD_TYPE_PUSH_ACK: u64 = 3;

/// `head.cmd` of the auth handshake (`CMD["AuthBind"]`).
pub const CMD_AUTH_BIND: &str = "auth-bind";
/// `head.cmd` of the heartbeat request (`CMD["Ping"]`).
pub const CMD_PING: &str = "ping";
/// `head.cmd` of the kickout frame (`CMD["Kickout"]`).
pub const CMD_KICKOUT: &str = "kickout";
/// `head.module` of the connection layer (`MODULE["ConnAccess"]`).
pub const MODULE_CONN_ACCESS: &str = "conn_access";
/// Business service every payload is wrapped in (`_BIZ_PKG`).
pub const BIZ_PACKAGE: &str = "yuanbao_openclaw_proxy";

/// `auth_info.biz_id` sent on `AUTH_BIND` (adapter.py `_authenticate`).
pub const AUTH_BIZ_ID: &str = "ybBot";

/// Fixed `device_info.instance_id` (`HERMES_INSTANCE_ID`, yuanbao_proto.py:34).
pub const INSTANCE_ID: u64 = 17;

/// Reply heartbeat meaning "still working" (`WS_HEARTBEAT_RUNNING`).
pub const WS_HEARTBEAT_RUNNING: i64 = 1;
/// Reply heartbeat meaning "finished" (`WS_HEARTBEAT_FINISH`).
pub const WS_HEARTBEAT_FINISH: i64 = 2;

/// `TIMTextElem` — a plain text element.
pub const TIM_TEXT_ELEM: &str = "TIMTextElem";
/// `TIMImageElem` — an image element.
pub const TIM_IMAGE_ELEM: &str = "TIMImageElem";
/// `TIMSoundElem` — a voice element.
pub const TIM_SOUND_ELEM: &str = "TIMSoundElem";
/// `TIMVideoFileElem` — a video element.
pub const TIM_VIDEO_FILE_ELEM: &str = "TIMVideoFileElem";
/// `TIMFileElem` — a file/document element.
pub const TIM_FILE_ELEM: &str = "TIMFileElem";
/// `TIMCustomElem` — a JSON-in-`data` element (mentions, cards, forward records).
pub const TIM_CUSTOM_ELEM: &str = "TIMCustomElem";
/// `TIMFaceElem` — a sticker/emoji element.
pub const TIM_FACE_ELEM: &str = "TIMFaceElem";

/// `elem_type` of an `@`-mention inside a `TIMCustomElem` (`GroupAtGuardMiddleware`).
pub const ELEM_TYPE_MENTION: i64 = 1002;
/// `elem_type` of a forwarded chat record inside a `TIMCustomElem`.
pub const ELEM_TYPE_CHAT_RECORD: i64 = 1009;

/// Placeholder the reference emits for a custom element it cannot render.
pub const UNSUPPORTED_ELEMENT: &str = "[unsupported message type]";

/// `genUploadInfo` path for temporary COS credentials (`yuanbao_media.UPLOAD_INFO_PATH`).
pub const UPLOAD_INFO_PATH: &str = "/api/resource/genUploadInfo";

/// The COS global-accelerate host suffix `upload_to_cos` PUTs to (`{bucket}.cos.accelerate…`).
pub const COS_ACCELERATE_HOST_SUFFIX: &str = ".cos.accelerate.myqcloud.com";

/// How long a COS signature lives when the credentials carry no future `expiredTime`
/// (`upload_to_cos`'s `expire_seconds` default).
pub const COS_SIGNATURE_TTL_SECONDS: i64 = 3600;

/// Outbound media size cap (`YuanbaoAdapter.MEDIA_MAX_SIZE_MB`), in megabytes.
pub const YUANBAO_MEDIA_MAX_SIZE_MB: u64 = 50;

/// TIM image format number for an unknown or absent MIME type (`get_image_format` → `255`).
pub const TIM_IMAGE_FORMAT_UNKNOWN: u64 = 255;

/// The image variant `build_image_msg_body` always writes: `type 1` = original.
pub const TIM_IMAGE_VARIANT_ORIGINAL: u64 = 1;

// ---------------------------------------------------------------------------
// Protobuf wire primitives
// ---------------------------------------------------------------------------

/// protobuf wire type 0: base-128 varint.
pub const WIRE_VARINT: u8 = 0;
/// protobuf wire type 1: 8-byte fixed field (stored, never interpreted).
pub const WIRE_FIXED64: u8 = 1;
/// protobuf wire type 2: length-delimited bytes / string / embedded message.
pub const WIRE_LEN: u8 = 2;
/// protobuf wire type 5: 4-byte fixed field (stored, never interpreted).
pub const WIRE_FIXED32: u8 = 5;

/// Everything that can go wrong while reading a frame. A malformed frame is never a panic and
/// never silently shortened: truncated input is reported as [`CodecError::Truncated`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// A field wanted more bytes than the frame carried.
    #[error("truncated protobuf field at offset {offset}: need {needed} more byte(s)")]
    Truncated {
        /// Offset in the frame where the missing bytes should have started.
        offset: usize,
        /// How many bytes the field declared but did not have.
        needed: usize,
    },
    /// A varint ran past the 10 bytes protobuf allows.
    #[error("varint longer than 10 bytes at offset {offset}")]
    VarintTooLong {
        /// Offset of the byte that made the varint too long.
        offset: usize,
    },
    /// Wire types 3/4 (deprecated groups) and anything else this codec does not know.
    #[error("unsupported protobuf wire type {wire_type} at offset {offset}")]
    UnsupportedWireType {
        /// Offset of the tag that carried the wire type.
        offset: usize,
        /// The wire type as read from the tag's low three bits.
        wire_type: u8,
    },
    /// A tag whose field number does not fit in `u32`.
    #[error("field number {number} does not fit in u32")]
    FieldNumberTooLarge {
        /// The offending field number.
        number: u64,
    },
}

/// Encode `value` as a base-128 varint.
pub fn encode_varint(value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    push_varint(value, &mut out);
    out
}

/// Encode a signed value the way protobuf `int32`/`int64` do: as its 64-bit two's-complement
/// varint, so `-1` is ten bytes (`ff ff ff ff ff ff ff ff ff 01`), exactly like
/// `_encode_varint`'s `value &= 0xFFFFFFFFFFFFFFFF`.
pub fn encode_i64_varint(value: i64) -> Vec<u8> {
    encode_varint(value as u64)
}

fn push_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let bits = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(bits);
            return;
        }
        out.push(bits | 0x80);
    }
}

/// Decode one varint starting at `*pos`, advancing `*pos` past it (`_decode_varint`).
pub fn decode_varint(data: &[u8], pos: &mut usize) -> Result<u64, CodecError> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let Some(&byte) = data.get(*pos) else {
            return Err(CodecError::Truncated {
                offset: *pos,
                needed: 1,
            });
        };
        *pos += 1;
        if shift >= 64 {
            return Err(CodecError::VarintTooLong { offset: *pos - 1 });
        }
        result |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
}

/// One parsed field: its number, wire type and raw value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    number: u32,
    wire_type: u8,
    value: FieldValue,
}

/// A field's raw value; fixed-width types keep their bytes so they can be skipped unread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    /// A wire-type-0 varint.
    Varint(u64),
    /// A length-delimited or fixed-width payload, exactly as it appeared.
    Bytes(Vec<u8>),
}

impl Field {
    /// The field number (tag >> 3).
    pub fn number(&self) -> u32 {
        self.number
    }

    /// The raw wire type (tag & 0x07).
    pub fn wire_type(&self) -> u8 {
        self.wire_type
    }

    /// The raw value.
    pub fn value(&self) -> &FieldValue {
        &self.value
    }

    /// The varint value, when this field is a varint.
    pub fn as_varint(&self) -> Option<u64> {
        match self.value {
            FieldValue::Varint(value) => Some(value),
            FieldValue::Bytes(_) => None,
        }
    }

    /// The payload bytes, when this field is length-delimited or fixed-width.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match &self.value {
            FieldValue::Varint(_) => None,
            FieldValue::Bytes(bytes) => Some(bytes),
        }
    }

    /// The payload decoded as UTF-8, when it is valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.as_bytes()?).ok()
    }
}

/// Parse a message body into its fields, in wire order (`_parse_fields`).
///
/// Unknown field numbers are simply returned and skipped by the message decoders; only a structural
/// fault (truncation, a bad varint, an unknown wire type) is an error.
pub fn parse_fields(data: &[u8]) -> Result<Vec<Field>, CodecError> {
    let mut fields = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let tag_offset = pos;
        let tag = decode_varint(data, &mut pos)?;
        let wire_type = (tag & 0x07) as u8;
        let number = tag >> 3;
        let number =
            u32::try_from(number).map_err(|_| CodecError::FieldNumberTooLarge { number })?;
        let value = match wire_type {
            WIRE_VARINT => FieldValue::Varint(decode_varint(data, &mut pos)?),
            WIRE_LEN => {
                let length = decode_varint(data, &mut pos)?;
                let length = usize::try_from(length).map_err(|_| CodecError::Truncated {
                    offset: pos,
                    needed: usize::MAX,
                })?;
                let end = pos.checked_add(length).ok_or(CodecError::Truncated {
                    offset: pos,
                    needed: length,
                })?;
                let bytes = data.get(pos..end).ok_or(CodecError::Truncated {
                    offset: pos,
                    needed: length,
                })?;
                pos = end;
                FieldValue::Bytes(bytes.to_vec())
            }
            known @ (WIRE_FIXED64 | WIRE_FIXED32) => {
                let width = if known == WIRE_FIXED64 { 8 } else { 4 };
                let end = pos.checked_add(width).ok_or(CodecError::Truncated {
                    offset: pos,
                    needed: width,
                })?;
                let bytes = data.get(pos..end).ok_or(CodecError::Truncated {
                    offset: pos,
                    needed: width,
                })?;
                pos = end;
                FieldValue::Bytes(bytes.to_vec())
            }
            other => {
                return Err(CodecError::UnsupportedWireType {
                    offset: tag_offset,
                    wire_type: other,
                });
            }
        };
        fields.push(Field {
            number,
            wire_type,
            value,
        });
    }
    Ok(fields)
}

fn push_tag(number: u32, wire_type: u8, out: &mut Vec<u8>) {
    push_varint((u64::from(number) << 3) | u64::from(wire_type), out);
}

/// Append field `number` as a varint.
pub fn put_varint(number: u32, value: u64, out: &mut Vec<u8>) {
    push_tag(number, WIRE_VARINT, out);
    push_varint(value, out);
}

/// Append field `number` as length-delimited bytes.
pub fn put_bytes(number: u32, value: &[u8], out: &mut Vec<u8>) {
    push_tag(number, WIRE_LEN, out);
    push_varint(value.len() as u64, out);
    out.extend_from_slice(value);
}

/// Append field `number` as a UTF-8 string (`_s`), always — even when empty.
pub fn put_string(number: u32, value: &str, out: &mut Vec<u8>) {
    put_bytes(number, value.as_bytes(), out);
}

/// A parsed message body with the reference's accessor semantics: the first matching field wins,
/// and a wire-type mismatch reads as the default rather than as an error (`_first`, `_get_string`,
/// `_get_varint`, `_get_bytes`, `_get_repeated_bytes`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
    entries: Vec<Field>,
}

impl Fields {
    /// Parse a message body.
    pub fn parse(data: &[u8]) -> Result<Self, CodecError> {
        Ok(Self {
            entries: parse_fields(data)?,
        })
    }

    /// Every field in wire order, unknown fields included.
    pub fn entries(&self) -> &[Field] {
        &self.entries
    }

    /// The first field with this number, whatever its wire type.
    pub fn first(&self, number: u32) -> Option<&Field> {
        self.entries.iter().find(|field| field.number == number)
    }

    /// The first length-delimited field `number` as a string; `""` when absent or mistyped.
    pub fn string(&self, number: u32) -> String {
        self.entries
            .iter()
            .find(|field| field.number == number && field.wire_type == WIRE_LEN)
            .and_then(Field::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// The first varint field `number`; `0` when absent or mistyped.
    pub fn varint(&self, number: u32) -> u64 {
        self.entries
            .iter()
            .find(|field| field.number == number && field.wire_type == WIRE_VARINT)
            .and_then(Field::as_varint)
            .unwrap_or_default()
    }

    /// The first length-delimited payload of field `number`; empty when absent.
    pub fn bytes(&self, number: u32) -> &[u8] {
        self.entries
            .iter()
            .find(|field| field.number == number && field.wire_type == WIRE_LEN)
            .and_then(Field::as_bytes)
            .unwrap_or_default()
    }

    /// Every length-delimited payload of field `number`, in wire order.
    pub fn repeated_bytes(&self, number: u32) -> Vec<&[u8]> {
        self.entries
            .iter()
            .filter(|field| field.number == number && field.wire_type == WIRE_LEN)
            .filter_map(Field::as_bytes)
            .collect()
    }

    /// Every occurrence of field `number` parsed as an embedded message (`_parse_repeated`).
    pub fn repeated_messages(&self, number: u32) -> Result<Vec<Fields>, CodecError> {
        self.repeated_bytes(number)
            .into_iter()
            .map(Self::parse)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// ConnMsg / Head
// ---------------------------------------------------------------------------

/// The connection-layer header of every frame.
///
/// `yuanbao_proto.py:223–224`: `message Head { uint32 cmd_type = 1; string cmd = 2; uint32 seq_no =
/// 3; string msg_id = 4; string module = 5; bool need_ack = 6; …; int32 status = 10; }`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    /// 0 request, 1 response, 2 push, 3 push-ack.
    pub cmd_type: u64,
    /// Method name (`auth-bind`, `ping`, `send_c2c_message`, the push command, …).
    pub cmd: String,
    /// Monotonic sequence number.
    pub seq_no: u64,
    /// Message id; doubles as the request id of a business call.
    pub msg_id: String,
    /// Service name (`conn_access` for the connection layer, the biz package otherwise).
    pub module: String,
    /// Whether the peer expects a push acknowledgement.
    pub need_ack: bool,
    /// Response status; non-zero means the request was refused.
    pub status: u64,
}

/// One WebSocket frame: a header plus its business payload.
///
/// `message ConnMsg { Head head = 1; bytes data = 2; }` (yuanbao_proto.py:225).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnMsg {
    /// The connection-layer header.
    pub head: Head,
    /// The serialized business payload (empty for `ping`).
    pub data: Vec<u8>,
}

/// Encode a header, mirroring `_encode_head`: zero/empty fields are omitted, and `status` is written
/// as a 64-bit two's-complement varint (`status & 0xFFFFFFFFFFFFFFFF`).
pub fn encode_head(head: &Head) -> Vec<u8> {
    let mut out = Vec::new();
    if head.cmd_type != 0 {
        put_varint(1, head.cmd_type, &mut out);
    }
    if !head.cmd.is_empty() {
        put_string(2, &head.cmd, &mut out);
    }
    if head.seq_no != 0 {
        put_varint(3, head.seq_no, &mut out);
    }
    if !head.msg_id.is_empty() {
        put_string(4, &head.msg_id, &mut out);
    }
    if !head.module.is_empty() {
        put_string(5, &head.module, &mut out);
    }
    if head.need_ack {
        put_varint(6, 1, &mut out);
    }
    if head.status != 0 {
        put_varint(10, head.status, &mut out);
    }
    out
}

/// Decode a header (`_decode_head`). Absent fields take their zero value.
pub fn decode_head(data: &[u8]) -> Result<Head, CodecError> {
    let fields = Fields::parse(data)?;
    Ok(Head {
        cmd_type: fields.varint(1),
        cmd: fields.string(2),
        seq_no: fields.varint(3),
        msg_id: fields.string(4),
        module: fields.string(5),
        need_ack: fields.varint(6) != 0,
        status: fields.varint(10),
    })
}

/// Encode a full frame (`encode_conn_msg_full`): the head is always written, the payload only when
/// it is non-empty.
pub fn encode_conn_msg(msg: &ConnMsg) -> Vec<u8> {
    let mut out = Vec::new();
    let head = encode_head(&msg.head);
    put_bytes(1, &head, &mut out);
    if !msg.data.is_empty() {
        put_bytes(2, &msg.data, &mut out);
    }
    out
}

/// Decode a frame (`decode_conn_msg`).
pub fn decode_conn_msg(data: &[u8]) -> Result<ConnMsg, CodecError> {
    let fields = Fields::parse(data)?;
    Ok(ConnMsg {
        head: decode_head(fields.bytes(1))?,
        data: fields.bytes(2).to_vec(),
    })
}

/// Wrap a business payload into a request frame (`encode_biz_msg`): `head.cmd` is the method,
/// `head.module` the service, `head.msg_id` the request id.
pub fn encode_biz_request(
    service: &str,
    method: &str,
    req_id: &str,
    body: &[u8],
    seq_no: u64,
) -> Vec<u8> {
    encode_conn_msg(&ConnMsg {
        head: Head {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: method.to_string(),
            seq_no,
            msg_id: req_id.to_string(),
            module: service.to_string(),
            need_ack: false,
            status: 0,
        },
        data: body.to_vec(),
    })
}

// ---------------------------------------------------------------------------
// AuthBind
// ---------------------------------------------------------------------------

/// `AuthInfo { 1 uid, 2 source, 3 token }` (yuanbao_proto.py:448).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthInfo {
    /// The bot's account id.
    pub uid: String,
    /// Token source (`bot` / `web`).
    pub source: String,
    /// The sign token.
    pub token: String,
}

/// `DeviceInfo { 1 app_version, 2 app_operation_system, 10 instance_id, 24 bot_version }`
/// (yuanbao_proto.py:449).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Client version.
    pub app_version: String,
    /// Operating system name.
    pub operation_system: String,
    /// Fixed instance id ([`INSTANCE_ID`]).
    pub instance_id: u64,
    /// Bot version.
    pub bot_version: String,
}

/// `AuthBindReq` (yuanbao_proto.py:448–451).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthBindReq {
    /// `ybBot` ([`AUTH_BIZ_ID`]).
    pub biz_id: String,
    /// Credentials.
    pub auth_info: AuthInfo,
    /// Client identity.
    pub device_info: DeviceInfo,
    /// Routing environment for non-production backends (`YUANBAO_ROUTE_ENV`).
    pub env_name: String,
}

/// `AuthBindRsp { 1 code, 2 message, 3 connectId }` (adapter.py `_extract_connect_id`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthBindRsp {
    /// 0 on success.
    pub code: u64,
    /// Failure text.
    pub message: String,
    /// The connection id the gateway assigned.
    pub connect_id: String,
}

/// Encode an `AUTH_BIND` request frame (`encode_auth_bind`). The frame is a connection-layer
/// request: `head.module` is [`MODULE_CONN_ACCESS`] and `head.msg_id` is the generated message id.
pub fn encode_auth_bind(request: &AuthBindReq, msg_id: &str, seq_no: u64) -> Vec<u8> {
    // AuthInfo always writes all three fields, exactly like `_s(1, uid) + _s(2, source) + _s(3, token)`.
    let mut auth = Vec::new();
    put_string(1, &request.auth_info.uid, &mut auth);
    put_string(2, &request.auth_info.source, &mut auth);
    put_string(3, &request.auth_info.token, &mut auth);
    // DeviceInfo: instance_id is kind "S" and therefore always present.
    let mut device = Vec::new();
    if !request.device_info.app_version.is_empty() {
        put_string(1, &request.device_info.app_version, &mut device);
    }
    if !request.device_info.operation_system.is_empty() {
        put_string(2, &request.device_info.operation_system, &mut device);
    }
    put_string(
        10,
        &request.device_info.instance_id.to_string(),
        &mut device,
    );
    if !request.device_info.bot_version.is_empty() {
        put_string(24, &request.device_info.bot_version, &mut device);
    }
    let mut body = Vec::new();
    put_string(1, &request.biz_id, &mut body);
    put_bytes(2, &auth, &mut body);
    put_bytes(3, &device, &mut body);
    if !request.env_name.is_empty() {
        put_string(5, &request.env_name, &mut body);
    }
    encode_biz_request(MODULE_CONN_ACCESS, CMD_AUTH_BIND, msg_id, &body, seq_no)
}

/// Decode an `AuthBindReq` payload (not the frame).
pub fn decode_auth_bind(data: &[u8]) -> Result<AuthBindReq, CodecError> {
    let fields = Fields::parse(data)?;
    let auth = Fields::parse(fields.bytes(2))?;
    let device = Fields::parse(fields.bytes(3))?;
    Ok(AuthBindReq {
        biz_id: fields.string(1),
        auth_info: AuthInfo {
            uid: auth.string(1),
            source: auth.string(2),
            token: auth.string(3),
        },
        device_info: DeviceInfo {
            app_version: device.string(1),
            operation_system: device.string(2),
            // Written as `str(INSTANCE_ID)` (field 10 is a string in the reference's encoder);
            // a varint form is tolerated on read.
            instance_id: device
                .string(10)
                .trim()
                .parse()
                .unwrap_or_else(|_| device.varint(10)),
            bot_version: device.string(24),
        },
        env_name: fields.string(5),
    })
}

/// Encode an `AuthBindRsp` payload (used by the mock gateway in tests and by fake servers).
pub fn encode_auth_bind_rsp(response: &AuthBindRsp) -> Vec<u8> {
    let mut out = Vec::new();
    if response.code != 0 {
        put_varint(1, response.code, &mut out);
    }
    if !response.message.is_empty() {
        put_string(2, &response.message, &mut out);
    }
    if !response.connect_id.is_empty() {
        put_string(3, &response.connect_id, &mut out);
    }
    out
}

/// Decode an `AuthBindRsp` payload (`_extract_connect_id`'s reader).
pub fn decode_auth_bind_rsp(data: &[u8]) -> Result<AuthBindRsp, CodecError> {
    let fields = Fields::parse(data)?;
    Ok(AuthBindRsp {
        code: fields.varint(1),
        message: fields.string(2),
        connect_id: fields.string(3),
    })
}

/// A complete `auth-bind` response frame (head plus payload).
pub fn encode_auth_bind_response(response: &AuthBindRsp, msg_id: &str, seq_no: u64) -> Vec<u8> {
    encode_conn_msg(&ConnMsg {
        head: Head {
            cmd_type: CMD_TYPE_RESPONSE,
            cmd: CMD_AUTH_BIND.to_string(),
            seq_no,
            msg_id: msg_id.to_string(),
            module: MODULE_CONN_ACCESS.to_string(),
            need_ack: false,
            status: 0,
        },
        data: encode_auth_bind_rsp(response),
    })
}

// ---------------------------------------------------------------------------
// Ping / PingRsp / PushAck
// ---------------------------------------------------------------------------

/// `PingReq` — an empty message (yuanbao_proto.py:462).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PingReq;

/// `PingRsp` — an empty message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PingRsp;

/// Encode a `ping` request frame (`encode_ping`). The payload is empty.
pub fn encode_ping(msg_id: &str, seq_no: u64) -> Vec<u8> {
    encode_conn_msg(&ConnMsg {
        head: Head {
            cmd_type: CMD_TYPE_REQUEST,
            cmd: CMD_PING.to_string(),
            seq_no,
            msg_id: msg_id.to_string(),
            module: MODULE_CONN_ACCESS.to_string(),
            need_ack: false,
            status: 0,
        },
        data: Vec::new(),
    })
}

/// Accept a `PingReq` payload, skipping any unknown field.
pub fn decode_ping_req(data: &[u8]) -> Result<PingReq, CodecError> {
    Fields::parse(data)?;
    Ok(PingReq)
}

/// Accept a `PingRsp` payload, skipping any unknown field.
pub fn decode_ping_rsp(data: &[u8]) -> Result<PingRsp, CodecError> {
    Fields::parse(data)?;
    Ok(PingRsp)
}

/// Encode a push acknowledgement that echoes the push head's `cmd` / `msg_id` / `module`
/// (`encode_push_ack`).
pub fn encode_push_ack(head: &Head, seq_no: u64) -> Vec<u8> {
    encode_conn_msg(&ConnMsg {
        head: Head {
            cmd_type: CMD_TYPE_PUSH_ACK,
            cmd: head.cmd.clone(),
            seq_no,
            msg_id: head.msg_id.clone(),
            module: head.module.clone(),
            need_ack: false,
            status: 0,
        },
        data: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Inbound push payload (InboundMessagePush) — `DirectedPush.data`
// ---------------------------------------------------------------------------

/// `MsgContent.ImageInfo { 1 type, 2 size, 3 width, 4 height, 5 url }` (yuanbao_proto.py:305).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    /// Image variant (`1` = original).
    #[serde(rename = "type")]
    pub kind: u64,
    /// Size in bytes.
    pub size: u64,
    /// Pixel width.
    pub width: u64,
    /// Pixel height.
    pub height: u64,
    /// Download URL (frequently a `resourceId` placeholder).
    pub url: String,
}

/// `MsgContent` (yuanbao_proto.py:295–306) — the payload of one message body element.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsgContent {
    /// `1` — plain text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// `2` — media uuid.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub uuid: String,
    /// `3` — TIM image format number.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub image_format: u64,
    /// `4` — raw payload; JSON for `TIMCustomElem`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data: String,
    /// `5` — description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub desc: String,
    /// `6` — extension blob.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ext: String,
    /// `7` — sound data.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sound: String,
    /// `8` — image variants.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_info_array: Vec<ImageInfo>,
    /// `9` — index.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub index: u64,
    /// `10` — resource URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// `11` — file size.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub file_size: u64,
    /// `12` — file name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file_name: String,
    /// `999` — `map<string, string>`; key `wexin_forward_msg_<id>_<userid>` holds base64
    /// `ForwardMsgData` (an omitted feature — see the module header).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ext_map: BTreeMap<String, String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// `MsgBodyElement { 1 msg_type, 2 msg_content }` (yuanbao_proto.py:329–330).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsgBodyElement {
    /// Element discriminator, e.g. [`TIM_TEXT_ELEM`].
    pub msg_type: String,
    /// The element payload.
    #[serde(default)]
    pub msg_content: MsgContent,
}

impl MsgBodyElement {
    /// A plain text element (`_text_elem`).
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            msg_type: TIM_TEXT_ELEM.to_string(),
            msg_content: MsgContent {
                text: text.into(),
                ..MsgContent::default()
            },
        }
    }
}

/// `ImMsgSeq { 1 msg_seq, 2 msg_id }` inside a recall callback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallSeq {
    /// Sequence number of the recalled message.
    pub msg_seq: u64,
    /// Id of the recalled message.
    pub msg_id: String,
}

/// A decoded `InboundMessagePush` payload — the body of a `DirectedPush` frame.
///
/// Field numbers are `_INBOUND_PUSH_SPEC` (yuanbao_proto.py:345–356). Empty values are dropped by
/// the reference; here they are kept as their zero value so a round trip is lossless.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushMsg {
    /// `1` — callback command; used by recall callbacks (`Group.CallbackAfterRecallMsg`, …).
    pub callback_command: String,
    /// `2` — sender account.
    pub from_account: String,
    /// `3` — recipient account.
    pub to_account: String,
    /// `4` — sender nickname.
    pub sender_nickname: String,
    /// `5` — group id.
    pub group_id: String,
    /// `6` — group code (the routable group handle).
    pub group_code: String,
    /// `7` — group name.
    pub group_name: String,
    /// `8` — message sequence number.
    pub msg_seq: u64,
    /// `9` — random value.
    pub msg_random: u64,
    /// `10` — message timestamp (ms).
    pub msg_time: u64,
    /// `11` — message key.
    pub msg_key: String,
    /// `12` — message id.
    pub msg_id: String,
    /// `13` — message body.
    pub msg_body: Vec<MsgBodyElement>,
    /// `14` — JSON extension; carries the quoted message (`quote`).
    pub cloud_custom_data: String,
    /// `15` — event timestamp.
    pub event_time: u64,
    /// `16` — the bot owner's account id.
    pub bot_owner_id: String,
    /// `17` — recalled messages (repeat of [`RecallSeq`]).
    pub recall_msg_seq_list: Vec<RecallSeq>,
    /// `18` — claw message type.
    pub claw_msg_type: u64,
    /// `19` — group code a DM originated from.
    pub private_from_group_code: String,
    /// `20` — `LogInfoExt.trace_id` (field 1 of the embedded message).
    pub trace_id: String,
}

/// An `@`-mention parsed out of a `TIMCustomElem` whose `data` is JSON with `elem_type == 1002`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    /// The mentioned account id.
    pub user_id: String,
    /// The display text used for the mention (`@vikram`).
    pub text: String,
}

impl PushMsg {
    /// The id to deduplicate on: `msg_id`, else `msg_key`.
    pub fn event_id(&self) -> Option<&str> {
        if !self.msg_id.is_empty() {
            Some(&self.msg_id)
        } else if !self.msg_key.is_empty() {
            Some(&self.msg_key)
        } else {
            None
        }
    }

    /// Whether this push is a recall callback.
    pub fn is_recall(&self) -> bool {
        matches!(
            self.callback_command.as_str(),
            "Group.CallbackAfterRecallMsg" | "C2C.CallbackAfterMsgWithDraw"
        )
    }

    /// The `@`-mentions carried by this message (`GroupAtGuardMiddleware._iter_bot_mentions`).
    pub fn mentions(&self) -> Vec<Mention> {
        self.msg_body
            .iter()
            .filter(|element| element.msg_type == TIM_CUSTOM_ELEM)
            .filter_map(|element| {
                let custom = parse_custom_elem(&element.msg_content.data)?;
                if json_i64(&custom, "elem_type") != Some(ELEM_TYPE_MENTION) {
                    return None;
                }
                Some(Mention {
                    user_id: json_string(&custom, "user_id"),
                    text: json_string(&custom, "text"),
                })
            })
            .collect()
    }

    /// Whether this message mentions `bot_id`.
    pub fn addressed_to(&self, bot_id: &str) -> bool {
        !bot_id.is_empty()
            && self
                .mentions()
                .iter()
                .any(|mention| mention.user_id == bot_id)
    }

    /// The display text of the message (`ExtractContentMiddleware._extract_text`): text elements
    /// verbatim, media as `[kind|ybres:RID]` anchors, mentions and cards from their JSON, unknown
    /// elements as `[ElemType]`. Parts are space-joined and a leading full-width slash is rewritten
    /// to ASCII.
    pub fn text(&self) -> String {
        let parts: Vec<String> = self.msg_body.iter().filter_map(element_text).collect();
        rewrite_slash_command(&parts.join(" "))
    }

    /// Quoted-message id and text from `cloud_custom_data` (`QuoteContextMiddleware`).
    pub fn quote(&self) -> (Option<String>, Option<String>) {
        let Ok(parsed) = serde_json::from_str::<Value>(&self.cloud_custom_data) else {
            return (None, None);
        };
        let Some(quote) = parsed.get("quote").filter(|quote| quote.is_object()) else {
            return (None, None);
        };
        let quote_id = json_string(quote, "id");
        let desc = json_string(quote, "desc");
        let sender = if json_string(quote, "sender_nickname").is_empty() {
            json_string(quote, "sender_id")
        } else {
            json_string(quote, "sender_nickname")
        };
        let text = if desc.is_empty() {
            None
        } else if sender.is_empty() {
            Some(desc)
        } else {
            Some(format!("{sender}: {desc}"))
        };
        (
            if quote_id.is_empty() {
                None
            } else {
                Some(quote_id)
            },
            text,
        )
    }
}

fn json_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn json_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn parse_custom_elem(data: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(data).ok()?;
    parsed.is_object().then_some(parsed)
}

/// Rewrite a leading full-width slash (Chinese IME) to ASCII so commands match
/// (`ExtractContentMiddleware._rewrite_slash_command`).
fn rewrite_slash_command(text: &str) -> String {
    let trimmed = text.trim();
    match trimmed.strip_prefix('\u{ff0f}') {
        Some(rest) => format!("/{rest}"),
        None => trimmed.to_string(),
    }
}

/// The `resourceId` query parameter of a Yuanbao resource URL, or `""`
/// (`ExtractContentMiddleware._parse_resource_id`).
pub fn resource_id_from_url(url: &str) -> String {
    let Some((_, query)) = url.split_once('?') else {
        return String::new();
    };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key.eq_ignore_ascii_case("resourceId") {
            return value.trim().to_string();
        }
    }
    String::new()
}

/// The image URL the reference prefers: variant index 1 (medium), falling back to index 0
/// (`_pick_image_url`).
fn pick_image_url(content: &MsgContent) -> String {
    let images = &content.image_info_array;
    let chosen = if images.len() > 1 {
        images.get(1).or_else(|| images.first())
    } else {
        images.first()
    };
    chosen
        .map(|image| image.url.trim().to_string())
        .unwrap_or_default()
}

/// One element's contribution to [`PushMsg::text`].
fn element_text(element: &MsgBodyElement) -> Option<String> {
    let content = &element.msg_content;
    match element.msg_type.as_str() {
        TIM_TEXT_ELEM => (!content.text.is_empty()).then(|| content.text.clone()),
        TIM_IMAGE_ELEM => {
            let rid = resource_id_from_url(&pick_image_url(content));
            Some(if rid.is_empty() {
                "[image]".to_string()
            } else {
                format!("[image|ybres:{rid}]")
            })
        }
        TIM_SOUND_ELEM => {
            let rid = resource_id_from_url(content.url.trim());
            Some(if rid.is_empty() {
                "[voice]".to_string()
            } else {
                format!("[voice|ybres:{rid}]")
            })
        }
        TIM_VIDEO_FILE_ELEM => {
            let rid = resource_id_from_url(content.url.trim());
            Some(if rid.is_empty() {
                "[video]".to_string()
            } else {
                format!("[video|ybres:{rid}]")
            })
        }
        TIM_FILE_ELEM => {
            let rid = resource_id_from_url(content.url.trim());
            let name = content.file_name.trim();
            Some(match (name.is_empty(), rid.is_empty()) {
                (false, false) => format!("[file:{name}|ybres:{rid}]"),
                (false, true) => format!("[file: {name}]"),
                (true, false) => format!("[file|ybres:{rid}]"),
                (true, true) => "[file]".to_string(),
            })
        }
        TIM_CUSTOM_ELEM => Some(custom_elem_text(&content.data)),
        TIM_FACE_ELEM => {
            let name = parse_custom_elem(&content.data)
                .map(|custom| json_string(&custom, "name"))
                .unwrap_or_default();
            Some(if name.is_empty() {
                "[emoji]".to_string()
            } else {
                format!("[emoji: {name}]")
            })
        }
        "" => None,
        other => Some(format!("[{other}]")),
    }
}

/// `ExtractContentMiddleware._custom_elem_text`: `1002` mention, `1009` chat record, otherwise the
/// unsupported placeholder. `1007` link cards and `1010` share cards are an omission (module header).
fn custom_elem_text(data: &str) -> String {
    if data.is_empty() {
        return UNSUPPORTED_ELEMENT.to_string();
    }
    let Some(custom) = parse_custom_elem(data) else {
        // Malformed JSON is passed through verbatim, exactly like the reference.
        return data.to_string();
    };
    match json_i64(&custom, "elem_type") {
        Some(ELEM_TYPE_MENTION) => {
            let text = json_string(&custom, "text");
            if text.is_empty() {
                "[mention]".to_string()
            } else {
                text
            }
        }
        Some(ELEM_TYPE_CHAT_RECORD) => {
            let text = json_string(&custom, "text");
            if text.is_empty() {
                "[chat record]".to_string()
            } else {
                text
            }
        }
        _ => UNSUPPORTED_ELEMENT.to_string(),
    }
}

/// Encode a `MsgContent` (`_encode_msg_content`): the field spec order first, then the repeated
/// image array, then the `ext_map` entries.
pub fn encode_msg_content(content: &MsgContent) -> Vec<u8> {
    let mut out = Vec::new();
    if !content.text.is_empty() {
        put_string(1, &content.text, &mut out);
    }
    if !content.uuid.is_empty() {
        put_string(2, &content.uuid, &mut out);
    }
    if !content.data.is_empty() {
        put_string(4, &content.data, &mut out);
    }
    if !content.desc.is_empty() {
        put_string(5, &content.desc, &mut out);
    }
    if !content.ext.is_empty() {
        put_string(6, &content.ext, &mut out);
    }
    if !content.sound.is_empty() {
        put_string(7, &content.sound, &mut out);
    }
    if !content.url.is_empty() {
        put_string(10, &content.url, &mut out);
    }
    if !content.file_name.is_empty() {
        put_string(12, &content.file_name, &mut out);
    }
    if content.image_format != 0 {
        put_varint(3, content.image_format, &mut out);
    }
    if content.index != 0 {
        put_varint(9, content.index, &mut out);
    }
    if content.file_size != 0 {
        put_varint(11, content.file_size, &mut out);
    }
    for image in &content.image_info_array {
        put_bytes(8, &encode_image_info(image), &mut out);
    }
    for (key, value) in &content.ext_map {
        let mut entry = Vec::new();
        if !key.is_empty() {
            put_string(1, key, &mut entry);
        }
        if !value.is_empty() {
            put_string(2, value, &mut entry);
        }
        put_bytes(999, &entry, &mut out);
    }
    out
}

/// Encode an `ImageInfo` (`_IMAGE_INFO_SPEC`).
pub fn encode_image_info(image: &ImageInfo) -> Vec<u8> {
    let mut out = Vec::new();
    if image.kind != 0 {
        put_varint(1, image.kind, &mut out);
    }
    if image.size != 0 {
        put_varint(2, image.size, &mut out);
    }
    if image.width != 0 {
        put_varint(3, image.width, &mut out);
    }
    if image.height != 0 {
        put_varint(4, image.height, &mut out);
    }
    if !image.url.is_empty() {
        put_string(5, &image.url, &mut out);
    }
    out
}

/// Decode a `MsgContent` (`_decode_msg_content`).
pub fn decode_msg_content(data: &[u8]) -> Result<MsgContent, CodecError> {
    let fields = Fields::parse(data)?;
    let mut content = MsgContent {
        text: fields.string(1),
        uuid: fields.string(2),
        data: fields.string(4),
        desc: fields.string(5),
        ext: fields.string(6),
        sound: fields.string(7),
        url: fields.string(10),
        file_name: fields.string(12),
        image_format: fields.varint(3),
        index: fields.varint(9),
        file_size: fields.varint(11),
        image_info_array: Vec::new(),
        ext_map: BTreeMap::new(),
    };
    for image in fields.repeated_messages(8)? {
        content.image_info_array.push(ImageInfo {
            kind: image.varint(1),
            size: image.varint(2),
            width: image.varint(3),
            height: image.varint(4),
            url: image.string(5),
        });
    }
    for entry in fields.repeated_messages(999)? {
        let key = entry.string(1);
        if !key.is_empty() {
            content.ext_map.insert(key, entry.string(2));
        }
    }
    Ok(content)
}

/// Encode a `MsgBodyElement` (`_encode_msg_body_element`).
pub fn encode_msg_body_element(element: &MsgBodyElement) -> Vec<u8> {
    let content = encode_msg_content(&element.msg_content);
    let mut out = Vec::new();
    if !element.msg_type.is_empty() {
        put_string(1, &element.msg_type, &mut out);
    }
    if !content.is_empty() {
        put_bytes(2, &content, &mut out);
    }
    out
}

/// Decode a `MsgBodyElement` from already-parsed fields.
fn decode_msg_body_element(fields: &Fields) -> Result<MsgBodyElement, CodecError> {
    let content = fields.bytes(2);
    Ok(MsgBodyElement {
        msg_type: fields.string(1),
        msg_content: if content.is_empty() {
            MsgContent::default()
        } else {
            decode_msg_content(content)?
        },
    })
}

/// Encode an `InboundMessagePush` payload (`_INBOUND_PUSH_SPEC` order).
///
/// `callback_command` is written unconditionally — the reference's field kind is `"S"`, which always
/// encodes, so an empty value is still `0a 00`.
pub fn encode_push_msg(push: &PushMsg) -> Vec<u8> {
    let mut out = Vec::new();
    put_string(1, &push.callback_command, &mut out);
    if !push.from_account.is_empty() {
        put_string(2, &push.from_account, &mut out);
    }
    if !push.to_account.is_empty() {
        put_string(3, &push.to_account, &mut out);
    }
    if !push.sender_nickname.is_empty() {
        put_string(4, &push.sender_nickname, &mut out);
    }
    if !push.group_id.is_empty() {
        put_string(5, &push.group_id, &mut out);
    }
    if !push.group_code.is_empty() {
        put_string(6, &push.group_code, &mut out);
    }
    if !push.group_name.is_empty() {
        put_string(7, &push.group_name, &mut out);
    }
    if push.msg_seq != 0 {
        put_varint(8, push.msg_seq, &mut out);
    }
    if push.msg_random != 0 {
        put_varint(9, push.msg_random, &mut out);
    }
    if push.msg_time != 0 {
        put_varint(10, push.msg_time, &mut out);
    }
    if !push.msg_key.is_empty() {
        put_string(11, &push.msg_key, &mut out);
    }
    if !push.msg_id.is_empty() {
        put_string(12, &push.msg_id, &mut out);
    }
    for element in &push.msg_body {
        put_bytes(13, &encode_msg_body_element(element), &mut out);
    }
    if !push.cloud_custom_data.is_empty() {
        put_string(14, &push.cloud_custom_data, &mut out);
    }
    if push.event_time != 0 {
        put_varint(15, push.event_time, &mut out);
    }
    if !push.bot_owner_id.is_empty() {
        put_string(16, &push.bot_owner_id, &mut out);
    }
    for recalled in &push.recall_msg_seq_list {
        let mut entry = Vec::new();
        if recalled.msg_seq != 0 {
            put_varint(1, recalled.msg_seq, &mut entry);
        }
        if !recalled.msg_id.is_empty() {
            put_string(2, &recalled.msg_id, &mut entry);
        }
        put_bytes(17, &entry, &mut out);
    }
    if push.claw_msg_type != 0 {
        put_varint(18, push.claw_msg_type, &mut out);
    }
    if !push.private_from_group_code.is_empty() {
        put_string(19, &push.private_from_group_code, &mut out);
    }
    if !push.trace_id.is_empty() {
        let mut trace = Vec::new();
        put_string(1, &push.trace_id, &mut trace);
        put_bytes(20, &trace, &mut out);
    }
    out
}

/// Decode an `InboundMessagePush` payload (`decode_inbound_push`), keeping unknown fields out of the
/// result instead of failing on them.
pub fn decode_push_msg(data: &[u8]) -> Result<PushMsg, CodecError> {
    let fields = Fields::parse(data)?;
    let mut push = PushMsg {
        callback_command: fields.string(1),
        from_account: fields.string(2),
        to_account: fields.string(3),
        sender_nickname: fields.string(4),
        group_id: fields.string(5),
        group_code: fields.string(6),
        group_name: fields.string(7),
        msg_seq: fields.varint(8),
        msg_random: fields.varint(9),
        msg_time: fields.varint(10),
        msg_key: fields.string(11),
        msg_id: fields.string(12),
        msg_body: Vec::new(),
        cloud_custom_data: fields.string(14),
        event_time: fields.varint(15),
        bot_owner_id: fields.string(16),
        recall_msg_seq_list: Vec::new(),
        claw_msg_type: fields.varint(18),
        private_from_group_code: fields.string(19),
        trace_id: String::new(),
    };
    for element in fields.repeated_messages(13)? {
        push.msg_body.push(decode_msg_body_element(&element)?);
    }
    for recalled in fields.repeated_messages(17)? {
        push.recall_msg_seq_list.push(RecallSeq {
            msg_seq: recalled.varint(1),
            msg_id: recalled.string(2),
        });
    }
    let trace = fields.bytes(20);
    if !trace.is_empty() {
        push.trace_id = Fields::parse(trace)?.string(1);
    }
    Ok(push)
}

// ---------------------------------------------------------------------------
// Outbound business messages
// ---------------------------------------------------------------------------

/// `SendC2CMessageReq` (yuanbao_proto.py:413–425).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendC2CMessageReq {
    /// Request id, also the frame's `msg_id`.
    pub msg_id: String,
    /// The recipient account.
    pub to_account: String,
    /// The bot's account.
    pub from_account: String,
    /// Random value (0 = omitted).
    pub msg_random: u64,
    /// Message body.
    pub msg_body: Vec<MsgBodyElement>,
    /// Group the DM originated from.
    pub group_code: String,
    /// Sequence number (`None` = omitted).
    pub msg_seq: Option<u64>,
    /// Trace id, wrapped in a `LogInfoExt` (`None`/empty = omitted).
    pub trace_id: String,
}

/// `SendGroupMessageReq` (yuanbao_proto.py:428–436).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendGroupMessageReq {
    /// Request id, also the frame's `msg_id`.
    pub msg_id: String,
    /// The group code.
    pub group_code: String,
    /// The bot's account.
    pub from_account: String,
    /// Usually empty.
    pub to_account: String,
    /// Random value.
    pub random: String,
    /// Message body.
    pub msg_body: Vec<MsgBodyElement>,
    /// Message being quoted.
    pub ref_msg_id: String,
    /// Sequence number (`None` = omitted).
    pub msg_seq: Option<u64>,
    /// Trace id, wrapped in a `LogInfoExt`.
    pub trace_id: String,
}

/// `SendPrivateHeartbeatReq` (yuanbao_proto.py:476–478).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendPrivateHeartbeatReq {
    /// The bot's account.
    pub from_account: String,
    /// The other party.
    pub to_account: String,
    /// [`WS_HEARTBEAT_RUNNING`] or [`WS_HEARTBEAT_FINISH`].
    pub heartbeat: i64,
}

/// `SendGroupHeartbeatReq` (yuanbao_proto.py:481–488).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendGroupHeartbeatReq {
    /// The bot's account.
    pub from_account: String,
    /// The group code.
    pub group_code: String,
    /// Send time in milliseconds.
    pub send_time: u64,
    /// [`WS_HEARTBEAT_RUNNING`] or [`WS_HEARTBEAT_FINISH`].
    pub heartbeat: i64,
}

/// Encode a `SendC2CMessage` frame (`encode_send_c2c_message`).
pub fn encode_send_c2c_message(request: &SendC2CMessageReq, seq_no: u64) -> Vec<u8> {
    let mut body = Vec::new();
    if !request.msg_id.is_empty() {
        put_string(1, &request.msg_id, &mut body);
    }
    put_string(2, &request.to_account, &mut body);
    if !request.from_account.is_empty() {
        put_string(3, &request.from_account, &mut body);
    }
    if request.msg_random != 0 {
        put_varint(4, request.msg_random, &mut body);
    }
    for element in &request.msg_body {
        put_bytes(5, &encode_msg_body_element(element), &mut body);
    }
    if !request.group_code.is_empty() {
        put_string(6, &request.group_code, &mut body);
    }
    if let Some(msg_seq) = request.msg_seq {
        put_varint(7, msg_seq, &mut body);
    }
    if !request.trace_id.is_empty() {
        let mut trace = Vec::new();
        put_string(1, &request.trace_id, &mut trace);
        put_bytes(8, &trace, &mut body);
    }
    encode_biz_request(
        BIZ_PACKAGE,
        "send_c2c_message",
        &request.msg_id,
        &body,
        seq_no,
    )
}

/// Encode a `SendGroupMessage` frame (`encode_send_group_message`).
pub fn encode_send_group_message(request: &SendGroupMessageReq, seq_no: u64) -> Vec<u8> {
    let mut body = Vec::new();
    if !request.msg_id.is_empty() {
        put_string(1, &request.msg_id, &mut body);
    }
    put_string(2, &request.group_code, &mut body);
    if !request.from_account.is_empty() {
        put_string(3, &request.from_account, &mut body);
    }
    if !request.to_account.is_empty() {
        put_string(4, &request.to_account, &mut body);
    }
    if !request.random.is_empty() {
        put_string(5, &request.random, &mut body);
    }
    for element in &request.msg_body {
        put_bytes(6, &encode_msg_body_element(element), &mut body);
    }
    if !request.ref_msg_id.is_empty() {
        put_string(7, &request.ref_msg_id, &mut body);
    }
    if let Some(msg_seq) = request.msg_seq {
        put_varint(8, msg_seq, &mut body);
    }
    if !request.trace_id.is_empty() {
        let mut trace = Vec::new();
        put_string(1, &request.trace_id, &mut trace);
        put_bytes(9, &trace, &mut body);
    }
    encode_biz_request(
        BIZ_PACKAGE,
        "send_group_message",
        &request.msg_id,
        &body,
        seq_no,
    )
}

/// Encode a `SendPrivateHeartbeat` frame (`encode_send_private_heartbeat`).
///
/// The reference derives the request id as `hb_priv_<n>` and then draws the connection sequence
/// number, so the two counters differ; both are explicit here.
pub fn encode_send_private_heartbeat(
    request: &SendPrivateHeartbeatReq,
    msg_id: &str,
    seq_no: u64,
) -> Vec<u8> {
    let mut body = Vec::new();
    if !request.from_account.is_empty() {
        put_string(1, &request.from_account, &mut body);
    }
    if !request.to_account.is_empty() {
        put_string(2, &request.to_account, &mut body);
    }
    if request.heartbeat != 0 {
        put_varint(3, request.heartbeat as u64, &mut body);
    }
    encode_biz_request(BIZ_PACKAGE, "send_private_heartbeat", msg_id, &body, seq_no)
}

/// Encode a `SendGroupHeartbeat` frame (`encode_send_group_heartbeat`). `to_account` (field 2) is
/// always written empty, exactly like the reference, and the request id is `hb_grp_<n>`.
pub fn encode_send_group_heartbeat(
    request: &SendGroupHeartbeatReq,
    msg_id: &str,
    seq_no: u64,
) -> Vec<u8> {
    let mut body = Vec::new();
    if !request.from_account.is_empty() {
        put_string(1, &request.from_account, &mut body);
    }
    put_string(2, "", &mut body);
    if !request.group_code.is_empty() {
        put_string(3, &request.group_code, &mut body);
    }
    put_varint(4, request.send_time, &mut body);
    if request.heartbeat != 0 {
        put_varint(5, request.heartbeat as u64, &mut body);
    }
    encode_biz_request(BIZ_PACKAGE, "send_group_heartbeat", msg_id, &body, seq_no)
}

// ---------------------------------------------------------------------------
// Outbound media: COS upload (`yuanbao_media.py`) + TIM media elements
// ---------------------------------------------------------------------------

/// The temporary COS credentials `genUploadInfo` hands back — `get_cos_credentials`'s `data` dict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CosCredentials {
    /// `bucketName` — the bucket the object is PUT into.
    pub bucket_name: String,
    /// `region` — descriptive only; the accelerate host carries no region.
    pub region: String,
    /// `location` — the object key inside the bucket.
    pub location: String,
    /// `encryptTmpSecretId` — the signature's `q-ak`.
    pub secret_id: String,
    /// `encryptTmpSecretKey` — the HMAC-SHA1 key of the signature.
    pub secret_key: String,
    /// `encryptToken` — sent as `x-cos-security-token`.
    pub session_token: String,
    /// `startTime` — the signature's first second; absent or `0` means "now".
    pub start_time: Option<i64>,
    /// `expiredTime` — the signature's last second.
    pub expired_time: Option<i64>,
    /// `resourceUrl` — the URL described back in the message; empty means "the COS URL".
    pub resource_url: String,
    /// `resourceID` — the Yuanbao resource id, when the backend assigned one.
    pub resource_id: String,
}

/// The result of one COS upload — `upload_to_cos`'s return dict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CosUpload {
    /// `url` — `resourceUrl` when the credentials carried one, else the COS object URL.
    pub url: String,
    /// `uuid` — the payload's MD5 (`md5_hex`), used as the media element's `uuid`.
    pub uuid: String,
    /// `size` — the payload length in bytes.
    pub size: u64,
    /// `width` — pixels, images only (`0` otherwise).
    pub width: u64,
    /// `height` — pixels, images only (`0` otherwise).
    pub height: u64,
}

/// One planned COS `PUT`: everything `upload_to_cos` computes before it touches the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CosUploadPlan {
    /// `{bucket}.cos.accelerate.myqcloud.com`.
    pub host: String,
    /// The percent-encoded object key, without its leading slash.
    pub encoded_key: String,
    /// `https://{host}/{encoded_key}`.
    pub url: String,
    /// The resolved MIME type, sent as `Content-Type`.
    pub content_type: String,
    /// The `Authorization` header value ([`cos_sign`]).
    pub authorization: String,
    /// The temporary session token, sent as `x-cos-security-token`.
    pub session_token: String,
}

impl CosUploadPlan {
    /// The `PUT` request: the reference's three headers and the raw bytes as the body.
    pub fn put_request(&self, bytes: &[u8]) -> HttpRequest {
        HttpRequest::put(&self.url)
            .header("Authorization", self.authorization.clone())
            .header("Content-Type", self.content_type.clone())
            .header("x-cos-security-token", self.session_token.clone())
            .body(bytes.to_vec())
    }

    /// What the upload yields once the `PUT` succeeded (`upload_to_cos`'s result dict).
    ///
    /// The MD5, the size and — for `image/*` payloads — the parsed pixel size come from the bytes,
    /// exactly like the reference; a payload whose dimensions do not parse keeps `0`/`0`.
    pub fn upload_result(&self, credentials: &CosCredentials, bytes: &[u8]) -> CosUpload {
        let (width, height) = if self.content_type.starts_with("image/") {
            parse_image_size(bytes).unwrap_or_default()
        } else {
            (0, 0)
        };
        CosUpload {
            url: if credentials.resource_url.is_empty() {
                self.url.clone()
            } else {
                credentials.resource_url.clone()
            },
            uuid: md5_hex(bytes),
            size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            width,
            height,
        }
    }
}

/// The media phase a send reached, so a failure names where it stopped instead of blurring
/// "the upload failed" and "the message failed" into one string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPhase {
    /// `genUploadInfo` — the temporary COS credentials.
    Credentials,
    /// The COS `PUT` of the bytes.
    Upload,
    /// The media message over the socket.
    Message,
}

impl MediaPhase {
    /// The phase's name, as it appears in a failure message.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Credentials => "credentials",
            Self::Upload => "upload",
            Self::Message => "message",
        }
    }
}

/// Parse a `genUploadInfo` response (`get_cos_credentials`): `code` must be absent, `null` or `0`,
/// and the data object must carry both `bucketName` and `location`.
///
/// Python's `code = result.get("code")` / `if code != 0 and code is not None` is reproduced
/// literally — any other value, numeric or not, is a refusal — and so is
/// `data = result.get("data") or result`: a present, non-empty `data` object wins, otherwise the
/// whole envelope is read as the data object.
pub fn parse_cos_credentials(value: &Value) -> Result<CosCredentials, String> {
    if let Some(code) = value.get("code").filter(|code| !code.is_null())
        && code.as_i64() != Some(0)
    {
        return Err(format!(
            "genUploadInfo failed: code={code}, msg={}",
            raw_json_string(value, "msg")
        ));
    }
    let data = value
        .get("data")
        .filter(|data| data.as_object().is_some_and(|map| !map.is_empty()))
        .unwrap_or(value);
    let credentials = CosCredentials {
        bucket_name: raw_json_string(data, "bucketName"),
        region: raw_json_string(data, "region"),
        location: raw_json_string(data, "location"),
        secret_id: raw_json_string(data, "encryptTmpSecretId"),
        secret_key: raw_json_string(data, "encryptTmpSecretKey"),
        session_token: raw_json_string(data, "encryptToken"),
        start_time: json_number_i64(data, "startTime"),
        expired_time: json_number_i64(data, "expiredTime"),
        resource_url: raw_json_string(data, "resourceUrl"),
        resource_id: raw_json_string(data, "resourceID"),
    };
    let mut missing = Vec::new();
    if credentials.bucket_name.is_empty() {
        missing.push("bucketName");
    }
    if credentials.location.is_empty() {
        missing.push("location");
    }
    if !missing.is_empty() {
        return Err(format!(
            "genUploadInfo returned an incomplete data object: missing {}",
            missing.join(", ")
        ));
    }
    Ok(credentials)
}

/// A JSON string field, untrimmed (`json_string` trims, which credential values must not).
fn raw_json_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A JSON numeric field as an integer; `None` when absent or not a number.
fn json_number_i64(value: &Value, key: &str) -> Option<i64> {
    let number = value.get(key)?.as_f64()?;
    // The COS timestamps are whole seconds; a fractional value is rounded down like Python's int().
    #[allow(clippy::cast_possible_truncation)]
    Some(number as i64)
}

/// The `genUploadInfo` request (`get_cos_credentials`).
///
/// `POST {api_domain}/api/resource/genUploadInfo` with the reference's headers — `Content-Type:
/// application/json`, `X-Token: <sign token>`, `X-ID: <bot id, else app key>`, `X-Source: web` and
/// `X-Route-Env` when one is configured — and the body
/// `{"fileName": <filename>, "fileId": <32 hex>, "docFrom": "localDoc", "docOpenId": ""}`.
pub fn cos_credentials_request(
    api_domain: &str,
    token: &str,
    bot_id: &str,
    app_key: &str,
    route_env: &str,
    filename: &str,
    file_id: &str,
) -> HttpRequest {
    let url = format!("{}{UPLOAD_INFO_PATH}", api_domain.trim_end_matches('/'));
    let header_id = if bot_id.trim().is_empty() {
        app_key.trim()
    } else {
        bot_id.trim()
    };
    let mut request = HttpRequest::json(
        &url,
        &json!({
            "fileName": filename,
            "fileId": file_id,
            "docFrom": "localDoc",
            "docOpenId": "",
        }),
    )
    .header("X-Token", token)
    .header("X-ID", header_id)
    .header("X-Source", "web");
    if !route_env.trim().is_empty() {
        request = request.header("X-Route-Env", route_env.trim());
    }
    request
}

/// The `Authorization` header for one COS request (`_cos_sign`, `q-sign-algorithm=sha1`).
///
/// The exact key and strings, in the reference's order:
///
/// ```text
/// q-sign-time    = "<start>;<start+expire>"
/// SignKey        = hex(HMAC-SHA1(secret_key, q-sign-time))
/// http-string    = "put\n<path>\n<params>\n<headers>\n"        (each list "k=v" joined by "&")
/// string-to-sign = "sha1\n<q-sign-time>\nhex(SHA1(http-string))\n"
/// q-signature    = hex(HMAC-SHA1(SignKey, string-to-sign))
/// ```
///
/// `params` and `headers` keys are lower-cased and their values percent-encoded (`safe=""`); both
/// lists are sorted by `(key, value)`. The result is
/// `q-sign-algorithm=sha1&q-ak=…&q-sign-time=…&q-key-time=…&q-header-list=…&q-url-param-list=…&q-signature=…`,
/// with the two lists joined by `;`.
///
/// SHA-1 is used because COS's `q-sign-algorithm=sha1` mandates it — nowhere else in this adapter.
// One parameter per value the reference's `_cos_sign` takes, in its order, so the port can be
// checked against the reference line by line; bundling them into a struct would only hide that.
#[allow(clippy::too_many_arguments)]
pub fn cos_sign(
    method: &str,
    path: &str,
    params: &[(&str, &str)],
    headers: &[(&str, &str)],
    secret_id: &str,
    secret_key: &str,
    start_time: i64,
    expire_seconds: i64,
) -> String {
    let q_sign_time = format!("{start_time};{}", start_time.saturating_add(expire_seconds));
    let sign_key = hmac_sha1_hex(secret_key.as_bytes(), q_sign_time.as_bytes());
    let sorted_params = sorted_signed_kv(params);
    let sorted_headers = sorted_signed_kv(headers);
    let http_string = [
        method.to_lowercase(),
        path.to_string(),
        join_signed_kv(&sorted_params),
        join_signed_kv(&sorted_headers),
        String::new(),
    ]
    .join("\n");
    let string_to_sign = [
        "sha1".to_string(),
        q_sign_time.clone(),
        sha1_hex(http_string.as_bytes()),
        String::new(),
    ]
    .join("\n");
    let signature = hmac_sha1_hex(sign_key.as_bytes(), string_to_sign.as_bytes());
    let header_list = sorted_headers
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let param_list = sorted_params
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(";");
    format!(
        "q-sign-algorithm=sha1&q-ak={secret_id}&q-sign-time={q_sign_time}\
         &q-key-time={q_sign_time}&q-header-list={header_list}&q-url-param-list={param_list}\
         &q-signature={signature}"
    )
}

/// `_sorted_kv`: `(lower-case key, percent-encoded value)` pairs, sorted.
fn sorted_signed_kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut sorted: Vec<(String, String)> = pairs
        .iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), percent_encode(value)))
        .collect();
    sorted.sort();
    sorted
}

/// `_sorted_kv`'s join: `k=v` pairs separated by `&`.
fn join_signed_kv(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Python's `urllib.parse.quote(value, safe="")`: the unreserved characters
/// (`A-Z a-z 0-9 _ . - ~`) pass through, every other UTF-8 byte becomes `%XX` in upper-case hex.
pub fn percent_encode(value: &str) -> String {
    percent_encode_with(value, false)
}

/// Python's `urllib.parse.quote(value, safe="/")` — the object key's encoding in `upload_to_cos`.
pub fn percent_encode_path(value: &str) -> String {
    percent_encode_with(value, true)
}

fn percent_encode_with(value: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'~');
        if unreserved || (keep_slash && byte == b'/') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Lower-case hex SHA-1 (`hashlib.sha1(...).hexdigest()`) for the COS HTTP string.
fn sha1_hex(message: &[u8]) -> String {
    hex_lower(sha1_digest(&SHA1_FOR_LEGACY_USE_ONLY, message).as_ref())
}

/// Lower-case hex HMAC-SHA1 (`_hmac_sha1_hex`), the `q-sign` primitive. SHA-1 appears here only
/// because the COS signature algorithm demands it.
fn hmac_sha1_hex(key: &[u8], message: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
    hex_lower(hmac::sign(&key, message).as_ref())
}

/// Plan the COS `PUT` of one payload (`upload_to_cos`, minus the hypertext transfer):
/// `/^\/`-stripped percent-encoded key, accelerate host, resolved MIME type, signature and expiry.
///
/// `start_time` is used when it is set and non-zero, otherwise `now`; the signature lives until
/// `expiredTime` when that is still in the future, otherwise [`COS_SIGNATURE_TTL_SECONDS`]. Missing
/// `secretId` / `secretKey` / `location` is an error here, before any request is built.
pub fn cos_upload_plan(
    credentials: &CosCredentials,
    filename: &str,
    content_type: &str,
    now: i64,
) -> Result<CosUploadPlan, String> {
    if credentials.secret_id.is_empty()
        || credentials.secret_key.is_empty()
        || credentials.location.is_empty()
    {
        return Err(format!(
            "COS credentials incomplete: secretId={}, secretKey={}, location={}",
            !credentials.secret_id.is_empty(),
            !credentials.secret_key.is_empty(),
            !credentials.location.is_empty()
        ));
    }
    let host = format!("{}{COS_ACCELERATE_HOST_SUFFIX}", credentials.bucket_name);
    let encoded_key = percent_encode_path(&credentials.location)
        .trim_start_matches('/')
        .to_string();
    let url = format!("https://{host}/{encoded_key}");
    let content_type = resolved_content_type(filename, content_type);
    let start_time = credentials
        .start_time
        .filter(|start| *start != 0)
        .unwrap_or(now);
    let expire_seconds = match credentials.expired_time {
        Some(expired) if expired > now => expired.saturating_sub(now),
        _ => COS_SIGNATURE_TTL_SECONDS,
    };
    let authorization = cos_sign(
        "put",
        &format!("/{encoded_key}"),
        &[],
        &[
            ("host", host.as_str()),
            ("content-type", content_type.as_str()),
            ("x-cos-security-token", credentials.session_token.as_str()),
        ],
        &credentials.secret_id,
        &credentials.secret_key,
        start_time,
        expire_seconds,
    );
    Ok(CosUploadPlan {
        host,
        encoded_key,
        url,
        content_type,
        authorization,
        session_token: credentials.session_token.clone(),
    })
}

/// `upload_to_cos`'s MIME decision: a declared type is kept unless it is missing or the
/// octet-stream default, in which case an image filename's extension decides and everything else
/// stays `application/octet-stream`.
fn resolved_content_type(filename: &str, content_type: &str) -> String {
    if content_type.is_empty() || content_type == "application/octet-stream" {
        if is_image_filename(filename) {
            guess_mime_type(filename)
        } else {
            "application/octet-stream".to_string()
        }
    } else {
        content_type.to_string()
    }
}

/// `guess_mime_type`: a filename's MIME type by extension, `application/octet-stream` when unknown.
pub fn guess_mime_type(filename: &str) -> String {
    mime_for_extension(&file_extension(filename))
        .unwrap_or("application/octet-stream")
        .to_string()
}

/// `os.path.splitext(filename)[1]`, lower-cased: the last dot's suffix of the basename, or `""`
/// when the name has no dot or the dot is its first character (`.bashrc` has no extension).
fn file_extension(filename: &str) -> String {
    let basename = filename.rsplit('/').next().unwrap_or_default();
    match basename.rfind('.') {
        Some(index) if index > 0 => basename[index..].to_lowercase(),
        _ => String::new(),
    }
}

/// `_EXT_TO_MIME`, verbatim.
fn mime_for_extension(extension: &str) -> Option<&'static str> {
    Some(match extension {
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".bmp" => "image/bmp",
        ".heic" => "image/heic",
        ".tiff" => "image/tiff",
        ".ico" => "image/x-icon",
        ".pdf" => "application/pdf",
        ".doc" => "application/msword",
        ".docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ".xls" => "application/vnd.ms-excel",
        ".xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ".ppt" => "application/vnd.ms-powerpoint",
        ".pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ".txt" => "text/plain",
        ".zip" => "application/zip",
        ".tar" => "application/x-tar",
        ".gz" => "application/gzip",
        ".mp3" => "audio/mpeg",
        ".mp4" => "video/mp4",
        ".wav" => "audio/wav",
        ".ogg" => "audio/ogg",
        ".webm" => "video/webm",
        _ => return None,
    })
}

/// `is_image(filename, "")` for an extension-only check (`_IMAGE_EXTS`).
pub fn is_image_filename(filename: &str) -> bool {
    matches!(
        file_extension(filename).as_str(),
        ".jpg" | ".jpeg" | ".png" | ".gif" | ".webp" | ".bmp" | ".heic" | ".tiff" | ".ico"
    )
}

/// `get_image_format`: the TIM image format number for a MIME type, `255` when it is unknown.
pub fn image_format_for_mime(mime: &str) -> u64 {
    match mime.to_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => 1,
        "image/gif" => 2,
        "image/png" => 3,
        "image/bmp" => 4,
        "image/webp" | "image/heic" | "image/tiff" => TIM_IMAGE_FORMAT_UNKNOWN,
        _ => TIM_IMAGE_FORMAT_UNKNOWN,
    }
}

/// Pixel size of PNG / JPEG / GIF / WebP bytes (`parse_image_size`), left to right as the
/// reference tries them; `None` when none of the four parses.
pub fn parse_image_size(data: &[u8]) -> Option<(u64, u64)> {
    png_size(data)
        .or_else(|| jpeg_size(data))
        .or_else(|| gif_size(data))
        .or_else(|| webp_size(data))
}

fn be_u16(data: &[u8], at: usize) -> Option<u64> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u64::from(u16::from_be_bytes([bytes[0], bytes[1]])))
}

fn be_u32(data: &[u8], at: usize) -> Option<u64> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u64::from(u32::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
    ])))
}

fn le_u16(data: &[u8], at: usize) -> Option<u64> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u64::from(u16::from_le_bytes([bytes[0], bytes[1]])))
}

fn le_u24(data: &[u8], at: usize) -> Option<u64> {
    let bytes = data.get(at..at.checked_add(3)?)?;
    Some(u64::from(bytes[0]) | (u64::from(bytes[1]) << 8) | (u64::from(bytes[2]) << 16))
}

fn le_u32(data: &[u8], at: usize) -> Option<u64> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u64::from(u32::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3],
    ])))
}

/// `_parse_png_size`: IHDR's width/height at offset 16 (after the 8-byte signature, the 4-byte
/// length and `IHDR`).
fn png_size(data: &[u8]) -> Option<(u64, u64)> {
    if data.len() < 24 || data.get(..4) != Some(b"\x89PNG".as_slice()) {
        return None;
    }
    Some((be_u32(data, 16)?, be_u32(data, 20)?))
}

/// `_parse_jpeg_size`: walk the markers to the first SOF0/SOF2 and read `height` then `width`.
fn jpeg_size(data: &[u8]) -> Option<(u64, u64)> {
    if data.len() < 4 || data.get(..2) != Some([0xFF, 0xD8].as_slice()) {
        return None;
    }
    let mut index = 2usize;
    while index.checked_add(9)? < data.len() {
        if data.get(index) != Some(&0xFF) {
            index += 1;
        } else if matches!(data.get(index + 1), Some(0xC0 | 0xC2)) {
            let height = be_u16(data, index + 5)?;
            let width = be_u16(data, index + 7)?;
            return Some((width, height));
        } else if index + 3 < data.len() {
            let segment = usize::try_from(be_u16(data, index + 2)?).unwrap_or_default();
            index += 2 + segment;
        } else {
            break;
        }
    }
    None
}

/// `_parse_gif_size`: the little-endian logical-screen width/height at offset 6.
fn gif_size(data: &[u8]) -> Option<(u64, u64)> {
    let signature = data.get(..6)?;
    if data.len() < 10 || (signature != b"GIF87a" && signature != b"GIF89a") {
        return None;
    }
    Some((le_u16(data, 6)?, le_u16(data, 8)?))
}

/// `_parse_webp_size`: the `VP8 ` / `VP8L` / `VP8X` chunk's dimensions, masked like the reference.
fn webp_size(data: &[u8]) -> Option<(u64, u64)> {
    if data.len() < 16
        || data.get(..4) != Some(b"RIFF".as_slice())
        || data.get(8..12) != Some(b"WEBP")
    {
        return None;
    }
    let chunk = data.get(12..16)?;
    if chunk == b"VP8 " && data.len() >= 30 && data.get(23..26) == Some(b"\x9d\x01\x2a".as_slice())
    {
        return Some((le_u16(data, 26)? & 0x3fff, le_u16(data, 28)? & 0x3fff));
    }
    if chunk == b"VP8L" && data.len() >= 25 && data.get(20) == Some(&0x2F) {
        let bits = le_u32(data, 21)?;
        return Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1));
    }
    if chunk == b"VP8X" && data.len() >= 30 {
        return Some((le_u24(data, 24)? + 1, le_u24(data, 27)? + 1));
    }
    None
}

/// Lower-case hex MD5 of `data` — the media element's `uuid` (`hashlib.md5(data).hexdigest()`).
fn md5_hex(data: &[u8]) -> String {
    md5::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// MD5 (RFC 1321), implemented here because the crate may not add dependencies and no MD5 is
/// otherwise available; `uuid` in a `TIMImageElem` / `TIMFileElem` is the payload's MD5.
mod md5 {
    /// Per-round left-rotation amounts.
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    /// `floor(abs(sin(i + 1)) * 2^32)`, the RFC's table.
    const TABLE: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];

    /// Raw 16-byte digest, little-endian words as the RFC lays them out.
    pub(super) fn digest(data: &[u8]) -> [u8; 16] {
        let mut message = data.to_vec();
        let bit_len = (data.len() as u64).wrapping_mul(8);
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bit_len.to_le_bytes());

        let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
        for block in message.as_chunks::<64>().0 {
            let mut words = [0u32; 16];
            for (word, chunk) in words.iter_mut().zip(block.as_chunks::<4>().0) {
                *word = u32::from_le_bytes(*chunk);
            }
            let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
            for round in 0..64 {
                let (mixed, index) = match round / 16 {
                    0 => ((b & c) | (!b & d), round),
                    1 => ((d & b) | (!d & c), (5 * round + 1) % 16),
                    2 => (b ^ c ^ d, (3 * round + 5) % 16),
                    _ => (c ^ (b | !d), (7 * round) % 16),
                };
                let sum = a
                    .wrapping_add(mixed)
                    .wrapping_add(TABLE[round])
                    .wrapping_add(words[index]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(sum.rotate_left(SHIFTS[round]));
            }
            state[0] = state[0].wrapping_add(a);
            state[1] = state[1].wrapping_add(b);
            state[2] = state[2].wrapping_add(c);
            state[3] = state[3].wrapping_add(d);
        }

        let mut out = [0u8; 16];
        for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
            *chunk = word.to_le_bytes();
        }
        out
    }
}

/// A `TIMImageElem` body (`build_image_msg_body`).
///
/// `uuid` follows the reference's fallback chain — `uuid or filename or basename(url) or "image"` —
/// the variant is `type 1` (original), and `image_format` is the MIME's TIM number, `255` when the
/// MIME type is absent or unknown.
pub fn image_msg_body(upload: &CosUpload, filename: &str, mime: &str) -> MsgBodyElement {
    let uuid = if !upload.uuid.is_empty() {
        upload.uuid.clone()
    } else if !filename.is_empty() {
        filename.to_string()
    } else {
        let basename = basename_from_url(&upload.url);
        if basename.is_empty() {
            "image".to_string()
        } else {
            basename
        }
    };
    MsgBodyElement {
        msg_type: TIM_IMAGE_ELEM.to_string(),
        msg_content: MsgContent {
            uuid,
            image_format: if mime.is_empty() {
                TIM_IMAGE_FORMAT_UNKNOWN
            } else {
                image_format_for_mime(mime)
            },
            image_info_array: vec![ImageInfo {
                kind: TIM_IMAGE_VARIANT_ORIGINAL,
                size: upload.size,
                width: upload.width,
                height: upload.height,
                url: upload.url.clone(),
            }],
            ..MsgContent::default()
        },
    }
}

/// A `TIMFileElem` body (`build_file_msg_body`): `uuid or filename`, `file_name`, `file_size` and
/// the object `url`.
pub fn file_msg_body(upload: &CosUpload, filename: &str) -> MsgBodyElement {
    MsgBodyElement {
        msg_type: TIM_FILE_ELEM.to_string(),
        msg_content: MsgContent {
            uuid: if upload.uuid.is_empty() {
                filename.to_string()
            } else {
                upload.uuid.clone()
            },
            file_name: filename.to_string(),
            file_size: upload.size,
            url: upload.url.clone(),
            ..MsgContent::default()
        },
    }
}

/// The media element for one [`MediaKind`] — exactly the body the reference's handler builds.
///
/// [`MediaKind::Image`] becomes a `TIMImageElem` (`_ImageHandler`, used by both the URL and the
/// local-file image handlers). Every other kind becomes a `TIMFileElem` (`DocumentHandler`): the
/// reference has no `TIMVideoFileElem` / `TIMSoundElem` body builder, and `send_direct` sends any
/// non-image attachment through `send_document` (adapter.py:2437–2439). Nothing is invented.
pub fn media_msg_body(
    kind: MediaKind,
    upload: &CosUpload,
    filename: &str,
    mime: &str,
) -> MsgBodyElement {
    match kind {
        MediaKind::Image => image_msg_body(upload, filename, mime),
        MediaKind::Document | MediaKind::Video | MediaKind::Audio => {
            file_msg_body(upload, filename)
        }
    }
}

/// The filename a handler uses when the caller gave none: `"image.jpg"` for images,
/// `"document"` otherwise (`_read_local_file`'s `default_name`).
pub fn default_media_filename(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image.jpg",
        MediaKind::Document | MediaKind::Video | MediaKind::Audio => "document",
    }
}

/// The bytes' size half of `MessageSender.validate_media`: `Some(error)` when the payload exceeds
/// [`YUANBAO_MEDIA_MAX_SIZE_MB`], `None` when it fits.
pub fn media_size_error(filename: &str, byte_len: usize) -> Option<String> {
    let measured = u64::try_from(byte_len).unwrap_or(u64::MAX);
    if measured > YUANBAO_MEDIA_MAX_SIZE_MB * 1024 * 1024 {
        #[allow(clippy::cast_precision_loss)]
        let megabytes = measured as f64 / 1024.0 / 1024.0;
        return Some(format!(
            "File too large: {filename} ({megabytes:.1}MB > {YUANBAO_MEDIA_MAX_SIZE_MB}MB)"
        ));
    }
    None
}

/// `os.path.basename(urlparse(url).path)`: the last path segment of a URL, query and fragment
/// dropped, empty when the URL carries no path.
fn basename_from_url(url: &str) -> String {
    let target = &url[..url.find(['?', '#']).unwrap_or(url.len())];
    let after_scheme = match target.split_once("://") {
        Some((_, rest)) => rest,
        None => target,
    };
    let Some((_, path)) = after_scheme.split_once('/') else {
        return String::new();
    };
    path.rsplit('/').next().unwrap_or_default().to_string()
}

/// A decoded frame from the connection layer, as returned by [`YuanbaoAdapter::handle_frame`].
#[derive(Debug, Clone, PartialEq)]
pub enum FrameEvent {
    /// A frame this adapter has no use for.
    Ignored,
    /// The response to a `PING` (or to a reply heartbeat).
    HeartbeatAck {
        /// The `msg_id` of the acknowledged request.
        msg_id: String,
    },
    /// The response to `AUTH_BIND`.
    AuthAnswer {
        /// 0 on success.
        code: u64,
        /// Failure text.
        message: String,
        /// The assigned connection id, when the handshake succeeded.
        connect_id: Option<String>,
    },
    /// A downstream push carrying one decoded message.
    Push {
        /// The push's `msg_id`.
        msg_id: String,
        /// The message it turned into, when it produced one (deduplicated / self / empty pushes do not).
        event: Option<Box<MessageEvent>>,
        /// Whether a push acknowledgement was required and sent.
        acked: bool,
    },
    /// A push acknowledgement from the peer.
    PushAck {
        /// The acknowledged `msg_id`.
        msg_id: String,
    },
    /// A response to a business request.
    Response {
        /// The full response header.
        head: Head,
        /// The response payload.
        data: Vec<u8>,
    },
    /// The gateway kicked this bot off the connection.
    Kickout,
    /// A push frame whose payload did not decode. The frame is dropped, the connection is not.
    MalformedPush {
        /// What went wrong while decoding the payload.
        error: CodecError,
    },
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

/// Compute the sign-token signature (`SignManager.compute_signature`):
/// `HMAC-SHA256(key = app_secret, msg = nonce + timestamp + app_id + app_secret)` as lowercase hex.
pub fn compute_signature(nonce: &str, timestamp: &str, app_key: &str, app_secret: &str) -> String {
    let plain = format!("{nonce}{timestamp}{app_key}{app_secret}");
    let key = hmac::Key::new(hmac::HMAC_SHA256, app_secret.as_bytes());
    let tag = hmac::sign(&key, plain.as_bytes());
    hex_lower(tag.as_ref())
}

/// Lower-case hex, the encoding both `hexdigest()` and the reference's nonces use.
pub fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// A Beijing-time `%Y-%m-%dT%H:%M:%S+08:00` stamp for the given Unix second
/// (`SignManager.build_timestamp`).
pub fn build_timestamp_at(unix_secs: i64) -> String {
    let local = unix_secs.saturating_add(8 * 3600);
    let days = local.div_euclid(86_400);
    let of_day = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+08:00",
        of_day / 3600,
        (of_day % 3600) / 60,
        of_day % 60
    )
}

/// [`build_timestamp_at`] for the current instant.
pub fn build_timestamp() -> String {
    build_timestamp_at(now_unix_secs())
}

/// Howard Hinnant's days-from-civil inverse: (year, month, day) for a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// Sixteen bytes derived from the clock, used only when the CSPRNG is unavailable.
fn entropy_fallback() -> [u8; 16] {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos().to_le_bytes())
        .unwrap_or_default()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Yuanbao configuration, from `platforms.yuanbao.extra` (or the `YUANBAO_*` environment).
///
/// Field names follow `gateway_env.ENV_OVERRIDES` / `plugin.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YuanbaoConfig {
    /// `YUANBAO_APP_ID` (legacy alias `YUANBAO_APP_KEY`) — the HMAC key id.
    pub app_id: String,
    /// `YUANBAO_APP_SECRET` — the HMAC secret.
    pub app_secret: String,
    /// `YUANBAO_BOT_ID`; empty means "take it from the sign-token response".
    pub bot_id: String,
    /// `YUANBAO_WS_URL`.
    pub ws_url: String,
    /// `YUANBAO_API_DOMAIN`, used for the sign-token exchange.
    pub api_domain: String,
    /// `YUANBAO_ROUTE_ENV` — routing header for non-production backends.
    pub route_env: String,
    /// A pre-obtained sign token. Empty means "fetch one during `connect`".
    pub token: String,
    /// `X-AppVersion` / `device_info.app_version`.
    pub app_version: String,
    /// `X-OperationSystem` / `device_info.app_operation_system`.
    pub operation_system: String,
    /// How often a `PING` is due.
    pub heartbeat_interval: Duration,
    /// How long the `AUTH_BIND` response may take.
    pub auth_timeout: Duration,
    /// How long a business request may take for its response.
    pub request_timeout: Duration,
}

impl Default for YuanbaoConfig {
    fn default() -> Self {
        Self {
            app_id: String::new(),
            app_secret: String::new(),
            bot_id: String::new(),
            ws_url: YUANBAO_WS_URL.to_string(),
            api_domain: YUANBAO_API_DOMAIN.to_string(),
            route_env: String::new(),
            token: String::new(),
            app_version: String::from("1.0.0"),
            operation_system: std::env::consts::OS.to_string(),
            heartbeat_interval: HEARTBEAT_INTERVAL,
            auth_timeout: AUTH_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
        }
    }
}

impl YuanbaoConfig {
    /// A config carrying the required credentials over the default endpoints.
    pub fn new(app_id: impl Into<String>, app_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            ..Self::default()
        }
    }

    /// The reference's configured check: an app id **and** an app secret (`register._is_connected`).
    pub fn is_configured(&self) -> bool {
        !self.app_id.trim().is_empty() && !self.app_secret.trim().is_empty()
    }
}

/// A sign token and the identity that came with it (`SignManager` cache entry).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignToken {
    /// The token to present in `AUTH_BIND`.
    pub token: String,
    /// The bot's account id, authoritative over the configured one.
    pub bot_id: String,
    /// Seconds the token stays valid.
    pub duration: u64,
    /// Token source (`web` / `bot`).
    pub source: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Adapter-level failure, distinct from a single frame's [`CodecError`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum YuanbaoError {
    /// A frame did not decode.
    #[error(transparent)]
    Codec(#[from] CodecError),
    /// The socket failed or closed.
    #[error("yuanbao socket error: {0}")]
    Socket(String),
    /// The handshake was refused or incomplete.
    #[error("yuanbao auth failed: {0}")]
    Auth(String),
    /// The sign-token exchange failed.
    #[error("yuanbao sign token failed: {0}")]
    Token(String),
    /// A request got no response in time.
    #[error("yuanbao request {req_id} timed out")]
    Timeout {
        /// The request id that was awaited.
        req_id: String,
    },
    /// Nothing is connected, so nothing can be sent.
    #[error("yuanbao is not connected")]
    NotConnected,
}

/// Why one chunk could not be delivered, in the shared [`SendResult`] taxonomy.
#[derive(Debug, Clone)]
struct SendFailure {
    message: String,
    kind: &'static str,
    retryable: bool,
    retry_after: Option<f64>,
}

impl SendFailure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "transient",
            retryable: true,
            retry_after: None,
        }
    }

    fn permanent(message: impl Into<String>, kind: &'static str) -> Self {
        Self {
            message: message.into(),
            kind,
            retryable: false,
            retry_after: None,
        }
    }

    /// A retryable failure the peer asked us to wait out (`429` / `Retry-After`).
    fn rate_limited(message: impl Into<String>, retry_after: Option<f64>) -> Self {
        Self {
            message: message.into(),
            kind: "rate_limited",
            retryable: true,
            retry_after,
        }
    }

    /// The same failure, naming the media phase it happened in. The taxonomy is untouched, so a
    /// caller still branches on `kind` / `retryable` and only the message grows a prefix.
    fn in_phase(mut self, phase: MediaPhase) -> Self {
        self.message = format!("yuanbao media {}: {}", phase.as_str(), self.message);
        self
    }

    /// Classify a non-2xx HTTP status, the way every other adapter in this crate does.
    fn from_status(status: u16, retry_after: Option<f64>) -> Self {
        let message = format!("HTTP {status}");
        match status {
            401 | 403 => Self::permanent(message, "forbidden"),
            404 => Self::permanent(message, "not_found"),
            429 => Self::rate_limited(message, retry_after),
            500..=599 => Self::rate_limited(message, retry_after),
            _ => Self::permanent(message, "unknown"),
        }
    }

    fn into_result(self) -> SendResult {
        let mut result = SendResult::failed(self.message, Some(self.kind));
        if self.retryable {
            result = result.retryable(self.retry_after);
        }
        result
    }
}

impl From<YuanbaoError> for SendFailure {
    fn from(error: YuanbaoError) -> Self {
        match error {
            YuanbaoError::Codec(error) => {
                // A frame we cannot read is not the peer's fault; a retry may well work.
                SendFailure::transient(format!("yuanbao frame error: {error}"))
            }
            YuanbaoError::Socket(error) => {
                SendFailure::transient(format!("yuanbao socket: {error}"))
            }
            YuanbaoError::Auth(error) => SendFailure::permanent(error, "forbidden"),
            YuanbaoError::Token(error) => SendFailure::permanent(error, "forbidden"),
            YuanbaoError::Timeout { req_id } => {
                SendFailure::transient(format!("yuanbao request {req_id} timed out"))
            }
            YuanbaoError::NotConnected => SendFailure::transient("Not connected"),
        }
    }
}

// ---------------------------------------------------------------------------
// Heartbeat / reconnect / dedup state
// ---------------------------------------------------------------------------

/// The reference's heartbeat liveness policy (`_heartbeat_loop`), as a pure state machine:
/// [`HEARTBEAT_TIMEOUT_THRESHOLD`] consecutive misses mean the connection is dead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeartbeatState {
    misses: u32,
}

impl HeartbeatState {
    /// Consecutive missed pongs since the last answer.
    pub fn misses(&self) -> u32 {
        self.misses
    }

    /// A pong arrived: the counter resets. Returns the previous miss count.
    pub fn on_pong(&mut self) -> u32 {
        std::mem::take(&mut self.misses)
    }

    /// A pong was missed. Returns `true` when the threshold is reached, i.e. reconnect now.
    pub fn on_miss(&mut self) -> bool {
        self.misses = self.misses.saturating_add(1);
        self.misses >= HEARTBEAT_TIMEOUT_THRESHOLD
    }
}

/// Exponential reconnect backoff: `min(2^attempt, 60)` seconds (`_do_reconnect`).
pub fn reconnect_delay(attempt: u32) -> Duration {
    let seconds = 2u64.saturating_pow(attempt.min(6)).min(60);
    Duration::from_secs(seconds)
}

/// Whether a close code is recoverable. The permanent codes never reconnect
/// (`NO_RECONNECT_CLOSE_CODES`); an unknown code (`None`) reconnects.
pub fn should_reconnect(close_code: Option<u16>) -> bool {
    match close_code {
        Some(code) => !NO_RECONNECT_CLOSE_CODES.contains(&code),
        None => true,
    }
}

/// A bounded, TTL-based "already seen" set for inbound message ids (`MessageDeduplicator`).
#[derive(Debug)]
struct Dedup {
    seen: HashMap<String, Instant>,
    order: VecDeque<String>,
    ttl: Duration,
    capacity: usize,
}

impl Dedup {
    fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            seen: HashMap::new(),
            order: VecDeque::new(),
            ttl,
            capacity: capacity.max(1),
        }
    }

    fn is_duplicate(&mut self, id: &str) -> bool {
        let now = Instant::now();
        while let Some(oldest) = self.order.front() {
            let expired = self
                .seen
                .get(oldest)
                .is_none_or(|seen| now.duration_since(*seen) > self.ttl);
            if !expired && self.seen.len() <= self.capacity {
                break;
            }
            if let Some(key) = self.order.pop_front() {
                self.seen.remove(&key);
            }
        }
        if self.seen.contains_key(id) {
            return true;
        }
        self.seen.insert(id.to_string(), now);
        self.order.push_back(id.to_string());
        while self.order.len() > self.capacity {
            if let Some(key) = self.order.pop_front() {
                self.seen.remove(&key);
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Mutable connection state, behind one mutex; no guard is ever held across an `await`.
#[derive(Debug)]
struct Runtime {
    connected: bool,
    connect_id: Option<String>,
    bot_id: String,
    token: String,
    source: String,
    seq: u64,
    heartbeat: HeartbeatState,
    dedup: Dedup,
    pending: VecDeque<MessageEvent>,
    last_error: Option<String>,
}

impl Runtime {
    fn new() -> Self {
        Self {
            connected: false,
            connect_id: None,
            bot_id: String::new(),
            token: String::new(),
            source: String::new(),
            seq: 0,
            heartbeat: HeartbeatState::default(),
            dedup: Dedup::new(DEDUP_TTL, DEDUP_CAPACITY),
            pending: VecDeque::new(),
            last_error: None,
        }
    }
}

/// Tencent Yuanbao bot adapter over an injected [`BinarySocket`].
///
/// The socket and the HTTP transport are injected so the whole handshake, push decoding and send
/// path can be driven by mocks: nothing here opens a connection of its own.
pub struct YuanbaoAdapter {
    socket: Arc<dyn BinarySocket>,
    transport: SharedTransport,
    config: YuanbaoConfig,
    runtime: Mutex<Runtime>,
}

impl YuanbaoAdapter {
    /// Build an adapter over `socket` for the sign-token exchange, using `transport` for HTTP.
    pub fn new(
        config: YuanbaoConfig,
        socket: Arc<dyn BinarySocket>,
        transport: SharedTransport,
    ) -> Self {
        Self {
            socket,
            transport,
            config,
            runtime: Mutex::new(Runtime::new()),
        }
    }

    /// The configuration this adapter was built with.
    pub fn config(&self) -> &YuanbaoConfig {
        &self.config
    }

    /// Whether the `AUTH_BIND` handshake has completed.
    pub fn is_connected(&self) -> bool {
        self.lock().connected
    }

    /// The connection id assigned by the gateway, once bound.
    pub fn connect_id(&self) -> Option<String> {
        self.lock().connect_id.clone()
    }

    /// The bot account id in use (configured, or taken from the sign token).
    pub fn bot_id(&self) -> String {
        self.lock().bot_id.clone()
    }

    /// The most recent handshake/sign failure, for callers that need to explain a `false` from
    /// [`PlatformAdapter::connect`].
    pub fn last_error(&self) -> Option<String> {
        self.lock().last_error.clone()
    }

    /// The current heartbeat liveness state.
    pub fn heartbeat_state(&self) -> HeartbeatState {
        self.lock().heartbeat
    }

    /// Draw the next sequence number, wrapping like the reference's `uint32` counter.
    pub fn next_seq(&self) -> u64 {
        let mut runtime = self.lock();
        let seq = runtime.seq;
        runtime.seq = (runtime.seq + 1) & 0xFFFF_FFFF;
        seq
    }

    /// A fresh UUID-v4-shaped message id (the reference uses `str(uuid.uuid4())`).
    pub fn next_message_id(&self) -> String {
        let mut bytes = [0u8; 16];
        if getrandom::fill(&mut bytes).is_err() {
            // Entropy failure must not panic; a nanosecond stamp still keeps ids distinct.
            bytes = entropy_fallback();
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex = hex_lower(&bytes);
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    }

    /// A 32-hex-character nonce (`secrets.token_hex(16)`).
    pub fn next_nonce(&self) -> String {
        let mut bytes = [0u8; 16];
        if getrandom::fill(&mut bytes).is_err() {
            bytes = entropy_fallback();
        }
        hex_lower(&bytes)
    }

    /// POST the sign-token endpoint (`SignManager.fetch`), retrying the retryable code.
    ///
    /// The request body is `{app_key, nonce, signature, timestamp}` and the headers are the
    /// reference's `X-AppVersion` / `X-OperationSystem` / `X-Instance-Id` / `X-Bot-Version` (plus
    /// `X-Route-Env` when configured).
    pub async fn fetch_sign_token(&self) -> Result<SignToken, YuanbaoError> {
        if !self.config.is_configured() {
            return Err(YuanbaoError::Auth(
                "YUANBAO_APP_ID and YUANBAO_APP_SECRET are required".to_string(),
            ));
        }
        let url = format!(
            "{}{}",
            self.config.api_domain.trim_end_matches('/'),
            SIGN_TOKEN_PATH
        );
        let mut last_error = String::from("sign token failed: max retries exceeded");
        for attempt in 0..=MAX_TOKEN_RETRIES {
            let nonce = self.next_nonce();
            let timestamp = build_timestamp();
            let signature = compute_signature(
                &nonce,
                &timestamp,
                self.config.app_id.trim(),
                self.config.app_secret.trim(),
            );
            let mut request = HttpRequest::json(
                &url,
                &json!({
                    "app_key": self.config.app_id.trim(),
                    "nonce": nonce,
                    "signature": signature,
                    "timestamp": timestamp,
                }),
            )
            .header("X-AppVersion", self.config.app_version.clone())
            .header("X-OperationSystem", self.config.operation_system.clone())
            .header("X-Instance-Id", INSTANCE_ID.to_string())
            .header("X-Bot-Version", self.config.app_version.clone());
            if !self.config.route_env.trim().is_empty() {
                request = request.header("X-Route-Env", self.config.route_env.trim());
            }
            let response = match self.transport.execute(request).await {
                Ok(response) => response,
                Err(error) => {
                    last_error = format!("sign token request failed: {error}");
                    if attempt < MAX_TOKEN_RETRIES {
                        tokio::time::sleep(TOKEN_RETRY_DELAY).await;
                        continue;
                    }
                    return Err(YuanbaoError::Token(last_error));
                }
            };
            let body = response.json().unwrap_or(Value::Null);
            if !response.ok() {
                return Err(YuanbaoError::Token(format!(
                    "sign token API returned {}",
                    response.status
                )));
            }
            match body.get("code").and_then(Value::as_i64) {
                Some(0) => {
                    let Some(data) = body.get("data").filter(|data| data.is_object()) else {
                        return Err(YuanbaoError::Token(
                            "sign token response missing 'data'".to_string(),
                        ));
                    };
                    return Ok(SignToken {
                        token: json_string(data, "token"),
                        bot_id: json_string(data, "bot_id"),
                        duration: data
                            .get("duration")
                            .and_then(Value::as_u64)
                            .unwrap_or_default(),
                        source: json_string(data, "source"),
                    });
                }
                Some(RETRYABLE_TOKEN_CODE) if attempt < MAX_TOKEN_RETRIES => {
                    last_error = format!("sign token retryable code {RETRYABLE_TOKEN_CODE}");
                    tokio::time::sleep(TOKEN_RETRY_DELAY).await;
                }
                Some(code) => {
                    return Err(YuanbaoError::Token(format!(
                        "sign token error: code={code}, msg={}",
                        json_string(&body, "msg")
                    )));
                }
                None => {
                    return Err(YuanbaoError::Token(
                        "sign token response carried no code".to_string(),
                    ));
                }
            }
        }
        Err(YuanbaoError::Token(last_error))
    }

    /// A `PING` frame plus its `msg_id`; calling this also arms the pong tracking.
    pub fn ping_frame(&self) -> (String, Vec<u8>) {
        let msg_id = self.next_message_id();
        let frame = encode_ping(&msg_id, self.next_seq());
        (msg_id, frame)
    }

    /// Record a received pong (`_heartbeat_loop`'s success branch).
    pub fn pong_received(&self) {
        self.lock().heartbeat.on_pong();
    }

    /// Record a missed pong. `true` means the threshold is reached and the host should reconnect.
    pub fn pong_missed(&self) -> bool {
        self.lock().heartbeat.on_miss()
    }

    /// Send a reply heartbeat for `chat_id` (`HeartbeatManager.send_heartbeat_once`): a `group:`
    /// chat gets `SendGroupHeartbeat`, anything else `SendPrivateHeartbeat`.
    pub async fn send_heartbeat(&self, chat_id: &str, heartbeat: i64) -> Result<(), YuanbaoError> {
        let bot_id = {
            let runtime = self.lock();
            if !runtime.connected {
                return Err(YuanbaoError::NotConnected);
            }
            runtime.bot_id.clone()
        };
        if bot_id.is_empty() {
            return Err(YuanbaoError::NotConnected);
        }
        // The reference draws the request id first, then the connection sequence number.
        let frame = if let Some(group_code) = chat_id.strip_prefix("group:") {
            let msg_id = format!("hb_grp_{}", self.next_seq());
            encode_send_group_heartbeat(
                &SendGroupHeartbeatReq {
                    from_account: bot_id,
                    group_code: group_code.to_string(),
                    send_time: now_millis(),
                    heartbeat,
                },
                &msg_id,
                self.next_seq(),
            )
        } else {
            let msg_id = format!("hb_priv_{}", self.next_seq());
            encode_send_private_heartbeat(
                &SendPrivateHeartbeatReq {
                    from_account: bot_id,
                    to_account: chat_id
                        .strip_prefix("direct:")
                        .unwrap_or(chat_id)
                        .to_string(),
                    heartbeat,
                },
                &msg_id,
                self.next_seq(),
            )
        };
        self.socket
            .send_binary(&frame)
            .await
            .map_err(|error| YuanbaoError::Socket(error.to_string()))
    }

    /// Complete the `AUTH_BIND` handshake over the injected socket and return the `connectId`.
    ///
    /// A refusal is reported as an error — a non-zero `AuthBindRsp.code`, a missing `connectId`, a
    /// closed socket or a timeout all fail here rather than pretending the bot is bound.
    pub async fn connect_handshake(&self) -> Result<String, YuanbaoError> {
        let result = self.handshake().await;
        let mut runtime = self.lock();
        match &result {
            Ok(connect_id) => {
                runtime.connected = true;
                runtime.connect_id = Some(connect_id.clone());
                runtime.last_error = None;
            }
            Err(error) => {
                runtime.connected = false;
                runtime.connect_id = None;
                runtime.last_error = Some(error.to_string());
            }
        }
        result
    }

    async fn handshake(&self) -> Result<String, YuanbaoError> {
        if !self.config.is_configured() {
            return Err(YuanbaoError::Auth(
                "YUANBAO_APP_ID and YUANBAO_APP_SECRET are required".to_string(),
            ));
        }
        let token = if self.config.token.trim().is_empty() {
            self.fetch_sign_token().await?
        } else {
            SignToken {
                token: self.config.token.trim().to_string(),
                bot_id: self.config.bot_id.trim().to_string(),
                duration: 3600,
                source: "bot".to_string(),
            }
        };
        let bot_id = if self.config.bot_id.trim().is_empty() {
            token.bot_id.trim().to_string()
        } else {
            self.config.bot_id.trim().to_string()
        };
        if bot_id.is_empty() {
            return Err(YuanbaoError::Auth(
                "no bot id: configure YUANBAO_BOT_ID or return one from sign-token".to_string(),
            ));
        }
        let source = if token.source.trim().is_empty() {
            "bot".to_string()
        } else {
            token.source.trim().to_string()
        };
        {
            let mut runtime = self.lock();
            runtime.bot_id = bot_id.clone();
            runtime.token = token.token.clone();
            runtime.source = source.clone();
        }
        let msg_id = self.next_message_id();
        let request = AuthBindReq {
            biz_id: AUTH_BIZ_ID.to_string(),
            auth_info: AuthInfo {
                uid: bot_id,
                source,
                token: token.token,
            },
            device_info: DeviceInfo {
                app_version: self.config.app_version.clone(),
                operation_system: self.config.operation_system.clone(),
                instance_id: INSTANCE_ID,
                bot_version: self.config.app_version.clone(),
            },
            env_name: self.config.route_env.trim().to_string(),
        };
        let frame = encode_auth_bind(&request, &msg_id, self.next_seq());
        self.socket
            .send_binary(&frame)
            .await
            .map_err(|error| YuanbaoError::Socket(error.to_string()))?;

        let deadline = Instant::now() + self.config.auth_timeout;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(YuanbaoError::Timeout {
                    req_id: msg_id.clone(),
                });
            };
            let frame = match tokio::time::timeout(remaining, self.socket.next_binary()).await {
                Err(_) => {
                    return Err(YuanbaoError::Timeout {
                        req_id: msg_id.clone(),
                    });
                }
                Ok(Err(error)) => return Err(YuanbaoError::Socket(error.to_string())),
                Ok(Ok(None)) => {
                    return Err(YuanbaoError::Auth(
                        "connection closed during the AUTH_BIND handshake".to_string(),
                    ));
                }
                Ok(Ok(Some(frame))) => frame,
            };
            let decoded = match decode_conn_msg(&frame) {
                Ok(decoded) => decoded,
                // Frames that do not decode are skipped, exactly like the reference loop.
                Err(_) => continue,
            };
            if decoded.head.cmd_type != CMD_TYPE_RESPONSE || decoded.head.cmd != CMD_AUTH_BIND {
                if decoded.head.cmd_type == CMD_TYPE_PUSH && decoded.head.need_ack {
                    let ack = encode_push_ack(&decoded.head, self.next_seq());
                    if self.socket.send_binary(&ack).await.is_err() {
                        return Err(YuanbaoError::Socket("push ack failed".to_string()));
                    }
                }
                continue;
            }
            let response = decode_auth_bind_rsp(&decoded.data)?;
            if response.code != 0 {
                return Err(YuanbaoError::Auth(format!(
                    "AuthBindRsp error: code={}, message={:?}",
                    response.code, response.message
                )));
            }
            if response.connect_id.is_empty() {
                return Err(YuanbaoError::Auth("BIND_ACK missing connectId".to_string()));
            }
            return Ok(response.connect_id);
        }
    }

    /// Handle one already-received frame (`_handle_frame`): ACK pushes that ask for it, record
    /// heartbeats, surface auth answers and responses, and decode inbound pushes into events.
    ///
    /// A frame that is not a `ConnMsg` at all is an error; a `ConnMsg` whose push payload is
    /// malformed becomes [`FrameEvent::MalformedPush`] and the connection stays up.
    pub async fn handle_frame(&self, frame: &[u8]) -> Result<FrameEvent, YuanbaoError> {
        let msg = decode_conn_msg(frame)?;
        let head = msg.head;
        // A kickout unbinds the bot whatever its cmd_type: the gateway is telling us to go away.
        if head.cmd == CMD_KICKOUT {
            let mut runtime = self.lock();
            runtime.connected = false;
            runtime.connect_id = None;
            return Ok(FrameEvent::Kickout);
        }
        match head.cmd_type {
            CMD_TYPE_RESPONSE => {
                if head.cmd == CMD_PING {
                    self.lock().heartbeat.on_pong();
                    return Ok(FrameEvent::HeartbeatAck {
                        msg_id: head.msg_id,
                    });
                }
                if head.cmd == CMD_AUTH_BIND {
                    let response = decode_auth_bind_rsp(&msg.data)?;
                    if response.code == 0 && !response.connect_id.is_empty() {
                        let mut runtime = self.lock();
                        runtime.connect_id = Some(response.connect_id.clone());
                        runtime.connected = true;
                    }
                    return Ok(FrameEvent::AuthAnswer {
                        code: response.code,
                        message: response.message,
                        connect_id: (!response.connect_id.is_empty())
                            .then_some(response.connect_id),
                    });
                }
                Ok(FrameEvent::Response {
                    head,
                    data: msg.data,
                })
            }
            CMD_TYPE_PUSH => {
                let mut acked = false;
                if head.need_ack {
                    let ack = encode_push_ack(&head, self.next_seq());
                    self.socket
                        .send_binary(&ack)
                        .await
                        .map_err(|error| YuanbaoError::Socket(error.to_string()))?;
                    acked = true;
                }
                let push = match decode_push_msg(&msg.data) {
                    Ok(push) => push,
                    Err(error) => return Ok(FrameEvent::MalformedPush { error }),
                };
                let event = self.event_from_push(&push);
                if let Some(event) = &event {
                    // The queue is the single source of truth, so an event decoded while a send
                    // is awaiting its response is not lost.
                    self.lock().pending.push_back(event.clone());
                }
                Ok(FrameEvent::Push {
                    msg_id: head.msg_id,
                    event: event.map(Box::new),
                    acked,
                })
            }
            CMD_TYPE_PUSH_ACK => Ok(FrameEvent::PushAck {
                msg_id: head.msg_id,
            }),
            _ => Ok(FrameEvent::Ignored),
        }
    }

    /// Read frames until one produces an event; `Ok(None)` at end of stream.
    ///
    /// Frames that carry no message (acks, responses, malformed pushes, the bot's own messages,
    /// duplicates) are consumed silently, so the caller sees only dispatcheable events.
    pub async fn next_event(&self) -> Result<Option<MessageEvent>, YuanbaoError> {
        loop {
            if let Some(event) = self.lock().pending.pop_front() {
                return Ok(Some(event));
            }
            let frame = match self
                .socket
                .next_binary()
                .await
                .map_err(|error| YuanbaoError::Socket(error.to_string()))?
            {
                Some(frame) => frame,
                None => return Ok(None),
            };
            self.handle_frame(&frame).await?;
        }
    }

    /// Turn a decoded push into an event, or `None` when it is not dispatcheable.
    ///
    /// Mirrors `SkipSelfMiddleware`, `DedupMiddleware`, `ChatRoutingMiddleware`,
    /// `ExtractContentMiddleware`, `ClassifyMessageTypeMiddleware` and `QuoteContextMiddleware`.
    fn event_from_push(&self, push: &PushMsg) -> Option<MessageEvent> {
        let text = push.text();
        if text.trim().is_empty() {
            // A recall callback or an empty frame has nothing to answer.
            return None;
        }
        let mut runtime = self.lock();
        if !runtime.bot_id.is_empty() && push.from_account == runtime.bot_id {
            return None;
        }
        if let Some(id) = push.event_id()
            && runtime.dedup.is_duplicate(id)
        {
            return None;
        }
        let bot_id = runtime.bot_id.clone();
        drop(runtime);

        let is_group = !push.group_code.is_empty();
        let chat_id = if is_group {
            format!("group:{}", push.group_code)
        } else if !push.from_account.is_empty() {
            format!("direct:{}", push.from_account)
        } else {
            return None;
        };
        let chat_type = if is_group { "group" } else { "dm" };
        let chat_name = if is_group {
            if push.group_name.is_empty() {
                push.group_code.clone()
            } else {
                push.group_name.clone()
            }
        } else if push.sender_nickname.is_empty() {
            push.from_account.clone()
        } else {
            push.sender_nickname.clone()
        };

        let platform = Platform::new(platform_names::YUANBAO).unwrap_or_else(Platform::local);
        let mut source = SessionSource::new(platform, chat_id);
        source.chat_type = chat_type.to_string();
        source.chat_name = (!chat_name.is_empty()).then_some(chat_name);
        source.user_id = (!push.from_account.is_empty()).then(|| push.from_account.clone());
        source.user_name = (!push.sender_nickname.is_empty()).then(|| push.sender_nickname.clone());
        source.message_id = push.event_id().map(str::to_string);
        if is_group {
            source.thread_id = Some("main".to_string());
        }

        let mut event = MessageEvent::new(text.clone(), source);
        event.message_type = classify_message_type(&text, &push.msg_body);
        event.user_id = (!push.from_account.is_empty()).then(|| push.from_account.clone());
        event.user_name = (!push.sender_nickname.is_empty()).then(|| push.sender_nickname.clone());
        event.message_id = push.event_id().map(str::to_string);
        let (quote_id, quote_text) = push.quote();
        event.reply_to_message_id = quote_id;
        event.reply_to_text = quote_text;
        event.raw_message = serde_json::to_value(push).ok();
        event
            .metadata
            .insert("chat_type".to_string(), json!(chat_type));
        event
            .metadata
            .insert("msg_seq".to_string(), json!(push.msg_seq));
        if !push.trace_id.is_empty() {
            event
                .metadata
                .insert("trace_id".to_string(), json!(push.trace_id));
        }
        let mentions = push.mentions();
        if !mentions.is_empty() {
            event
                .metadata
                .insert("mentions".to_string(), json!(mentions));
        }
        event.metadata.insert(
            "addressed_to_bot".to_string(),
            json!(push.addressed_to(&bot_id)),
        );
        Some(event)
    }

    /// Wait for the response frame whose `msg_id` is `req_id`, decoding (and queueing) any push
    /// that arrives in between (`send_biz_request`).
    async fn await_response(&self, req_id: &str, timeout: Duration) -> Result<Head, YuanbaoError> {
        let deadline = Instant::now() + timeout;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(YuanbaoError::Timeout {
                    req_id: req_id.to_string(),
                });
            };
            let frame = match tokio::time::timeout(remaining, self.socket.next_binary()).await {
                Err(_) => {
                    return Err(YuanbaoError::Timeout {
                        req_id: req_id.to_string(),
                    });
                }
                Ok(Err(error)) => return Err(YuanbaoError::Socket(error.to_string())),
                Ok(Ok(None)) => {
                    return Err(YuanbaoError::Socket("connection closed".to_string()));
                }
                Ok(Ok(Some(frame))) => frame,
            };
            // An undecodable frame is skipped rather than failing the request.
            if let Ok(FrameEvent::Response { head, .. }) = self.handle_frame(&frame).await
                && head.msg_id == req_id
            {
                return Ok(head);
            }
        }
    }

    /// Build the frame for one `msg_body`, by chat-id prefix (`_send_msg_body`,
    /// `send_c2c_msg_body` / `send_group_msg_body`). `reply_to` becomes the group `ref_msg_id`; a
    /// C2C request has no such field, exactly like the reference.
    fn build_text_frame(
        &self,
        chat_id: &str,
        msg_body: Vec<MsgBodyElement>,
        reply_to: Option<&str>,
        group_code: &str,
    ) -> (String, Vec<u8>) {
        let bot_id = self.lock().bot_id.clone();
        if let Some(group) = chat_id.strip_prefix("group:") {
            let req_id = format!("grp_{}", self.next_seq());
            let frame = encode_send_group_message(
                &SendGroupMessageReq {
                    msg_id: req_id.clone(),
                    group_code: group.to_string(),
                    from_account: bot_id,
                    to_account: String::new(),
                    random: String::new(),
                    msg_body,
                    ref_msg_id: reply_to.unwrap_or_default().to_string(),
                    msg_seq: None,
                    trace_id: String::new(),
                },
                self.next_seq(),
            );
            (req_id, frame)
        } else {
            let to_account = chat_id.strip_prefix("direct:").unwrap_or(chat_id);
            let req_id = format!("c2c_{}", self.next_seq());
            let frame = encode_send_c2c_message(
                &SendC2CMessageReq {
                    msg_id: req_id.clone(),
                    to_account: to_account.to_string(),
                    from_account: bot_id,
                    msg_random: 0,
                    msg_body,
                    group_code: group_code.to_string(),
                    msg_seq: None,
                    trace_id: String::new(),
                },
                self.next_seq(),
            );
            (req_id, frame)
        }
    }

    /// Send one text chunk and wait for its response (`send_text_chunk`).
    async fn send_text_chunk(
        &self,
        chat_id: &str,
        text: &str,
        reply_to: Option<&str>,
        group_code: &str,
    ) -> Result<String, SendFailure> {
        self.send_msg_body(
            chat_id,
            vec![MsgBodyElement::text(text)],
            reply_to,
            group_code,
        )
        .await
    }

    /// Send one already-built `msg_body` and wait for its response (`_send_msg_body`,
    /// `dispatch_msg_body`): the text path and the media path share this, so both route
    /// `group:<code>` to `SendGroupMessage` and everything else to `SendC2CMessage`.
    async fn send_msg_body(
        &self,
        chat_id: &str,
        msg_body: Vec<MsgBodyElement>,
        reply_to: Option<&str>,
        group_code: &str,
    ) -> Result<String, SendFailure> {
        let (req_id, frame) = self.build_text_frame(chat_id, msg_body, reply_to, group_code);
        self.socket
            .send_binary(&frame)
            .await
            .map_err(|error| SendFailure::transient(format!("yuanbao socket: {error}")))?;
        let head = self
            .await_response(&req_id, self.config.request_timeout)
            .await
            .map_err(SendFailure::from)?;
        if head.status != 0 {
            return Err(SendFailure::permanent(
                format!("yuanbao rejected the message: status={}", head.status),
                "bad_format",
            ));
        }
        Ok(if head.msg_id.is_empty() {
            req_id
        } else {
            head.msg_id
        })
    }

    /// The sign token and bot id `genUploadInfo` must present: the handshake's token and bot id,
    /// falling back to the configured ones (`_auth_headers`' inputs).
    fn media_identity(&self) -> (String, String) {
        let (token, bot_id) = {
            let runtime = self.lock();
            (runtime.token.clone(), runtime.bot_id.clone())
        };
        let token = if token.is_empty() {
            self.config.token.trim().to_string()
        } else {
            token
        };
        let bot_id = if bot_id.is_empty() {
            self.config.bot_id.trim().to_string()
        } else {
            bot_id
        };
        (token, bot_id)
    }

    /// `get_cos_credentials`: temporary COS credentials for one upload.
    ///
    /// A non-2xx response, a non-zero `code` and an incomplete data object are all failures here;
    /// none of them can produce a URL, so no upload is attempted after them.
    async fn fetch_cos_credentials(&self, filename: &str) -> Result<CosCredentials, SendFailure> {
        let (token, bot_id) = self.media_identity();
        if token.is_empty() {
            return Err(SendFailure::permanent(
                "no sign token: connect (or set YUANBAO_TOKEN) before sending media",
                "forbidden",
            )
            .in_phase(MediaPhase::Credentials));
        }
        let request = cos_credentials_request(
            &self.config.api_domain,
            &token,
            &bot_id,
            &self.config.app_id,
            &self.config.route_env,
            filename,
            // `generate_file_id()`: `secrets.token_hex(16)`, i.e. 32 hex characters.
            &self.next_nonce(),
        );
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(error) => {
                return Err(SendFailure::transient(format!(
                    "genUploadInfo request failed: {error}"
                ))
                .in_phase(MediaPhase::Credentials));
            }
        };
        if !response.ok() {
            return Err(
                SendFailure::from_status(response.status, response.retry_after)
                    .in_phase(MediaPhase::Credentials),
            );
        }
        let body = response.json().unwrap_or(Value::Null);
        parse_cos_credentials(&body).map_err(|error| {
            SendFailure::permanent(error, "unknown").in_phase(MediaPhase::Credentials)
        })
    }

    /// `upload_to_cos`: sign the plan's `PUT`, upload the bytes, describe the stored object.
    async fn upload_bytes_to_cos(
        &self,
        credentials: &CosCredentials,
        filename: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<CosUpload, SendFailure> {
        let plan = cos_upload_plan(credentials, filename, content_type, now_unix_secs()).map_err(
            |error| SendFailure::permanent(error, "unknown").in_phase(MediaPhase::Upload),
        )?;
        let response = match self.transport.execute(plan.put_request(bytes)).await {
            Ok(response) => response,
            Err(error) => {
                return Err(SendFailure::transient(format!("COS PUT failed: {error}"))
                    .in_phase(MediaPhase::Upload));
            }
        };
        if !response.ok() {
            return Err(
                SendFailure::from_status(response.status, response.retry_after)
                    .in_phase(MediaPhase::Upload),
            );
        }
        Ok(plan.upload_result(credentials, bytes))
    }

    /// One media send in the reference's order (`MediaSendHandler.handle`): validate without I/O,
    /// then credentials, then the COS `PUT`, then the message. A failure stops the chain, so a
    /// rejected upload can never be followed by a media message.
    async fn send_media_inner(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> Result<SendResult, SendFailure> {
        if !self.is_connected() {
            return Err(SendFailure::transient("Not connected"));
        }
        let bytes = match &media.source {
            MediaSource::Bytes(bytes) => bytes,
            MediaSource::Url(url) => {
                return Err(SendFailure::permanent(
                    format!(
                        "yuanbao media sends need bytes: the reference downloads {url:?} through its \
                         SSRF-guarded fetch and re-uploads it, which this adapter does not implement"
                    ),
                    "bad_format",
                ));
            }
        };
        let filename = match media.filename.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => default_media_filename(media.kind).to_string(),
        };
        // `validate_media`: an empty payload and the size cap are refused before any request.
        if bytes.is_empty() {
            return Err(SendFailure::permanent(
                format!("Empty file: {filename}"),
                "bad_format",
            ));
        }
        if let Some(error) = media_size_error(&filename, bytes.len()) {
            return Err(SendFailure::permanent(error, "too_long"));
        }
        let declared = media.mime.as_deref().unwrap_or_default().trim();
        let content_type = if declared.is_empty() {
            guess_mime_type(&filename)
        } else {
            declared.to_string()
        };
        let credentials = self.fetch_cos_credentials(&filename).await?;
        let upload = self
            .upload_bytes_to_cos(&credentials, &filename, &content_type, bytes)
            .await?;
        let mut msg_body = vec![media_msg_body(
            media.kind,
            &upload,
            &filename,
            &content_type,
        )];
        // `if caption: msg_body.append(_text_elem(caption))` — an empty caption adds nothing.
        if let Some(caption) = media
            .caption
            .as_deref()
            .filter(|caption| !caption.is_empty())
        {
            msg_body.push(MsgBodyElement::text(caption));
        }
        // `dispatch_msg_body(..., group_code="")`: `send_media` carries no group context.
        let message_id = self
            .send_msg_body(chat_id, msg_body, reply_to, "")
            .await
            .map_err(|failure| failure.in_phase(MediaPhase::Message))?;
        Ok(SendResult::ok(message_id))
    }

    /// Never-panicking runtime guard: a poisoned mutex still yields the state.
    fn lock(&self) -> MutexGuard<'_, Runtime> {
        match self.runtime.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// The gateway's message-type classification (`ClassifyMessageTypeMiddleware._classify`).
fn classify_message_type(text: &str, body: &[MsgBodyElement]) -> MessageType {
    if text.starts_with('/') {
        return MessageType::Command;
    }
    for element in body {
        match element.msg_type.as_str() {
            TIM_IMAGE_ELEM => return MessageType::Photo,
            TIM_SOUND_ELEM => return MessageType::Voice,
            TIM_VIDEO_FILE_ELEM => return MessageType::Video,
            TIM_FILE_ELEM => return MessageType::Document,
            _ => {}
        }
    }
    MessageType::Text
}

#[async_trait]
impl PlatformAdapter for YuanbaoAdapter {
    /// Run the sign-token → `AUTH_BIND` → wait-for-`BIND_ACK` handshake. `false` means the bot is
    /// not bound; [`YuanbaoAdapter::last_error`] says why.
    ///
    /// `is_reconnect` changes nothing: the reference re-authenticates with a freshly signed token on
    /// every reconnect (`_do_reconnect` → `force_refresh` → `_dial`), and
    /// [`YuanbaoAdapter::connect_handshake`] always fetches the token it needs.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        self.connect_handshake().await.is_ok()
    }

    /// Mark the bot unbound and drop queued events. The socket is injected and shared, so closing it
    /// is the host's call; nothing here pretends to have closed it.
    async fn disconnect(&mut self) {
        let mut runtime = self.lock();
        runtime.connected = false;
        runtime.connect_id = None;
        runtime.heartbeat = HeartbeatState::default();
        runtime.dedup = Dedup::new(DEDUP_TTL, DEDUP_CAPACITY);
        runtime.pending.clear();
    }

    /// Trim, chunk to [`YUANBAO_MAX_MESSAGE_LENGTH`] code points and deliver the chunks in order.
    ///
    /// `group:<code>` chats go out as `SendGroupMessage` with the quoted message as `ref_msg_id`
    /// (first chunk only, like the reference); everything else goes out as `SendC2CMessage` with
    /// `metadata["group_code"]` set when a DM originated in a group. A non-zero response `status` is
    /// a failure. After delivery a `FINISH` reply heartbeat is emitted, best effort.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        metadata: Option<Value>,
    ) -> SendResult {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return SendResult::failed(
                "yuanbao cannot send an empty text message",
                Some("bad_format"),
            );
        }
        if !self.is_connected() {
            return SendFailure::transient("Not connected").into_result();
        }
        let chunks = truncate_message(trimmed, YUANBAO_MAX_MESSAGE_LENGTH, codepoint_len);
        let group_code = metadata
            .as_ref()
            .and_then(|metadata| metadata.get("group_code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut delivered: Vec<String> = Vec::with_capacity(chunks.len());
        for (index, chunk) in chunks.iter().enumerate() {
            let anchor = if index == 0 { reply_to } else { None };
            match self
                .send_text_chunk(chat_id, chunk, anchor, &group_code)
                .await
            {
                Ok(message_id) => delivered.push(message_id),
                Err(failure) => {
                    let mut result = failure.into_result();
                    result.continuation_message_ids = delivered;
                    return result;
                }
            }
        }
        // Delivery done: FINISH so the client's "working" bubble closes.
        let _ = self.send_heartbeat(chat_id, WS_HEARTBEAT_FINISH).await;
        match delivered.last() {
            Some(last) => SendResult::ok_split(last.clone(), delivered),
            None => SendResult::failed("yuanbao delivered no chunks", Some("unknown")),
        }
    }

    /// No lookup exists, so this reports the reference's fallback pair (hermes
    /// `YuanbaoAdapter.get_chat_info`).
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: if chat_id.starts_with("group:") {
                "group".to_string()
            } else {
                "dm".to_string()
            },
        }
    }

    /// True: [`PlatformAdapter::send_media`] really uploads to COS and posts a media element
    /// (`YuanbaoAdapter.send_image` / `send_image_file` / `send_document` →
    /// `MediaSendHandler.handle`).
    fn supports_media(&self) -> bool {
        true
    }

    /// Send one attachment through the reference's COS flow (`MediaSendHandler.handle`,
    /// yuanbao_media.py):
    ///
    /// 1. **validate** — connected, bytes present, at most [`YUANBAO_MEDIA_MAX_SIZE_MB`]
    ///    (`MessageSender.validate_media`). Nothing is requested before these hold.
    /// 2. **credentials** — `POST {api_domain}/api/resource/genUploadInfo` for temporary COS
    ///    credentials ([`cos_credentials_request`]).
    /// 3. **upload** — a `q-sign-algorithm=sha1` `PUT` of the bytes to
    ///    `https://{bucket}.cos.accelerate.myqcloud.com/{key}` ([`cos_upload_plan`]).
    /// 4. **message** — the media element ([`media_msg_body`]: `TIMImageElem` for an image,
    ///    `TIMFileElem` for everything else) plus the caption as a trailing `TIMTextElem`, sent over
    ///    the existing binary socket. `reply_to` rides along as the group `ref_msg_id`.
    ///
    /// A [`MediaSource::Url`] is refused with zero requests: the reference downloads remote URLs
    /// itself (`download_url`, SSRF-guarded) and re-uploads the bytes, and this adapter has no such
    /// fetch, so it will not pretend a URL it never read was delivered.
    ///
    /// Every failure is reported as a failure, and the message names the phase it happened in
    /// (`yuanbao media credentials:` / `upload:` / `message:`). A failed upload cannot be followed
    /// by a message: the phases are sequential. Unlike [`PlatformAdapter::send`], no `FINISH`
    /// reply heartbeat is emitted — the reference's media path does not send one either.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        match self.send_media_inner(chat_id, media, reply_to).await {
            Ok(result) => result,
            Err(failure) => failure.into_result(),
        }
    }

    /// Yuanbao renders markdown (code blocks, tables, bold) — see `register._PLATFORM_HINT`.
    fn supports_code_blocks(&self) -> bool {
        true
    }

    /// `send` chunks natively, so the router must not truncate first.
    fn splits_long_messages(&self) -> bool {
        true
    }

    /// The per-message cap `send` chunks against.
    fn max_message_length(&self) -> usize {
        YUANBAO_MAX_MESSAGE_LENGTH
    }

    /// Yuanbao counts code points.
    fn message_length(&self, text: &str) -> usize {
        codepoint_len(text)
    }

    /// Emit one `RUNNING` reply heartbeat so the client shows a working indicator
    /// (`HeartbeatManager.send_heartbeat_once`).
    ///
    /// The reference additionally renews `RUNNING` every 2 s and auto-`FINISH`es after 30 s idle;
    /// that is a background timer, so it is the host's cadence to drive here.
    async fn send_typing(&self, chat_id: &str, _metadata: Option<Value>) {
        let _ = self.send_heartbeat(chat_id, WS_HEARTBEAT_RUNNING).await;
    }

    /// No `FINISH` on stop: `send` emits it after delivery so the order stays
    /// RUNNING… → message → FINISH (hermes `stop_typing(send_finish=False)`).
    async fn stop_typing(&self, _chat_id: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::SocketError;

    // ---- Golden vectors, produced by running the reference codec in this workspace ----
    //
    // `python3` against `research/hermes-platforms/yuanbao/yuanbao_proto.py`, with the module's
    // sequence counter reset before each frame so `seq_no` is deterministic:
    //
    //   P._seq_counter = 0
    //   P.encode_auth_bind(biz_id="ybBot", uid="bot-uid-1", source="bot", token="tok-1",
    //                      msg_id="auth-1", app_version="1.2.3", operation_system="linux",
    //                      bot_version="1.2.3", route_env="")
    //
    // Every constant below is an exact byte-for-byte capture.

    /// `encode_conn_msg(2, 7, b"hello")` — the simplified interface.
    const GOLDEN_CONN_MSG_SIMPLE: &str = "0a0408021807120568656c6c6f";

    /// `encode_auth_bind(…)` with `seq_no = 0`.
    const GOLDEN_AUTH_BIND: &str = concat!(
        "0a201209617574682d62696e642206617574682d312a0b636f6e6e5f616363657373123c0a057962426f7412170a",
        "09626f742d7569642d311203626f741a05746f6b2d311a1a0a05312e322e3312056c696e757852023137c20105",
        "312e322e33"
    );

    /// `encode_ping("ping-1")` with `seq_no = 0`.
    const GOLDEN_PING: &str = "0a1b120470696e67220670696e672d312a0b636f6e6e5f616363657373";

    /// `encode_push_ack({cmd: "inbound_message_push", msg_id: "push-9", module:
    /// "yuanbao_openclaw_proxy"})` with `seq_no = 0`.
    const GOLDEN_PUSH_ACK: &str = concat!(
        "0a3808031214696e626f756e645f6d6573736167655f707573682206707573682d392a167975616e62616f5f6f",
        "70656e636c61775f70726f7879"
    );

    /// `encode_conn_msg_full(1, "auth-bind", 0, "auth-1", "conn_access", AuthBindRsp{code: 0,
    /// connectId: "connect-abc"})`.
    const GOLDEN_AUTH_BIND_RSP: &str = concat!(
        "0a2208011209617574682d62696e642206617574682d312a0b636f6e6e5f616363657373120d1a0b636f6e6e65",
        "63742d616263"
    );

    /// The same with `code = 10001`, `message = "bad token"`.
    const GOLDEN_AUTH_BIND_RSP_ERR: &str = concat!(
        "0a2208011209617574682d62696e642206617574682d322a0b636f6e6e5f616363657373120e08914e1209626164",
        "20746f6b656e"
    );

    /// An `InboundMessagePush` payload: mention + text + image elements, a quote in
    /// `cloud_custom_data`, a bot owner and a `LogInfoExt` trace id.
    const GOLDEN_PUSH_PAYLOAD: &str = concat!(
        "0a001206757365722d321a09626f742d7569642d312203416e6e32056772702d313a045465616d402a480750",
        "fbd095ffbc315a056b65792d3962056d73672d396a510a0d54494d437573746f6d456c656d1240223e7b22656c",
        "656d5f74797065223a20313030322c202274657874223a20224076696b72616d222c2022757365725f6964223a",
        "2022626f742d7569642d31227d6a190a0b54494d54657874456c656d120a0a0868692074686572656a560a0c54",
        "494d496d616765456c656d12461208696d672d7575696418034238080110b96018800520e0032a2b6874747073",
        "3a2f2f6578616d706c652e746573742f692e706e673f7265736f7572636549643d7269642d37724a7b2271756f",
        "7465223a207b226964223a20226d73672d38222c202264657363223a202271756f7465642074657874222c2022",
        "73656e6465725f6e69636b6e616d65223a2022426f227d7d820106757365722d32a201090a0774726163652d31"
    );

    /// The same payload with two unknown fields appended (`_v(77, 42)`, `_s(78, "zzz")`); the
    /// reference decodes it to the identical dict.
    const GOLDEN_PUSH_PAYLOAD_WITH_UNKNOWN: &str = concat!(
        "0a001206757365722d321a09626f742d7569642d312203416e6e32056772702d313a045465616d402a480750",
        "fbd095ffbc315a056b65792d3962056d73672d396a510a0d54494d437573746f6d456c656d1240223e7b22656c",
        "656d5f74797065223a20313030322c202274657874223a20224076696b72616d222c2022757365725f6964223a",
        "2022626f742d7569642d31227d6a190a0b54494d54657874456c656d120a0a0868692074686572656a560a0c54",
        "494d496d616765456c656d12461208696d672d7575696418034238080110b96018800520e0032a2b6874747073",
        "3a2f2f6578616d706c652e746573742f692e706e673f7265736f7572636549643d7269642d37724a7b2271756f",
        "7465223a207b226964223a20226d73672d38222c202264657363223a202271756f7465642074657874222c2022",
        "73656e6465725f6e69636b6e616d65223a2022426f227d7d820106757365722d32a201090a0774726163652d31",
        "e8042af204037a7a7a"
    );

    /// `encode_conn_msg_full(2, "inbound_message_push", 5, "push-9", "yuanbao_openclaw_proxy",
    /// GOLDEN_PUSH_PAYLOAD, need_ack=True)`.
    const GOLDEN_DIRECTED_PUSH: &str = concat!(
        "0a3c08021214696e626f756e645f6d6573736167655f7075736818052206707573682d392a167975616e6261",
        "6f5f6f70656e636c61775f70726f7879300112e7020a001206757365722d321a09626f742d7569642d31220341",
        "6e6e32056772702d313a045465616d402a480750fbd095ffbc315a056b65792d3962056d73672d396a510a0d5449",
        "4d437573746f6d456c656d1240223e7b22656c656d5f74797065223a20313030322c202274657874223a20224076",
        "696b72616d222c2022757365725f6964223a2022626f742d7569642d31227d6a190a0b54494d54657874456c65",
        "6d120a0a0868692074686572656a560a0c54494d496d616765456c656d12461208696d672d7575696418034238",
        "080110b96018800520e0032a2b68747470733a2f2f6578616d706c652e746573742f692e706e673f7265736f7572",
        "636549643d7269642d37724a7b2271756f7465223a207b226964223a20226d73672d38222c202264657363223a",
        "202271756f7465642074657874222c202273656e6465725f6e69636b6e616d65223a2022426f227d7d82010675",
        "7365722d32a201090a0774726163652d31"
    );

    /// `encode_send_c2c_message(to_account="user-2", msg_body=[text "hello"], from_account=
    /// "bot-uid-1", msg_id="c2c_0")` with `seq_no = 0`.
    const GOLDEN_SEND_C2C: &str = concat!(
        "0a31121073656e645f6332635f6d65737361676522056332635f302a167975616e62616f5f6f70656e636c61",
        "775f70726f787912320a056332635f301206757365722d321a09626f742d7569642d312a160a0b54494d546578",
        "74456c656d12070a0568656c6c6f"
    );

    /// `encode_send_group_message(group_code="grp-1", msg_body=[text "pong"], from_account=
    /// "bot-uid-1", msg_id="grp_0", ref_msg_id="push-9")` with `seq_no = 0`.
    const GOLDEN_SEND_GROUP: &str = concat!(
        "0a33121273656e645f67726f75705f6d65737361676522056772705f302a167975616e62616f5f6f70656e63",
        "6c61775f70726f787912380a056772705f3012056772702d311a09626f742d7569642d3132150a0b54494d54",
        "657874456c656d12060a04706f6e673a06707573682d39"
    );

    /// `encode_send_private_heartbeat("bot-uid-1", "user-2", RUNNING)` with `seq_no = 0`.
    const GOLDEN_SEND_PRIVATE_HEARTBEAT: &str = concat!(
        "0a3d121673656e645f707269766174655f6865617274626561741801220968625f707269765f302a16797561",
        "6e62616f5f6f70656e636c61775f70726f787912150a09626f742d7569642d311206757365722d321801"
    );

    /// `encode_send_group_heartbeat("bot-uid-1", "grp-1", FINISH, send_time=1700000000000)` with
    /// `seq_no = 0`.
    const GOLDEN_SEND_GROUP_HEARTBEAT: &str = concat!(
        "0a3a121473656e645f67726f75705f6865617274626561741801220868625f6772705f302a167975616e6261",
        "6f5f6f70656e636c61775f70726f7879121d0a09626f742d7569642d3112001a056772702d312080d095ffbc",
        "312802"
    );

    /// `encode_conn_msg_full(1, "ping", 3, "ping-1", "conn_access", b"")`.
    const GOLDEN_PING_RSP: &str =
        "0a1f0801120470696e671803220670696e672d312a0b636f6e6e5f616363657373";

    /// `_v(11, -1)` — `-1` as a 64-bit two's-complement varint.
    const GOLDEN_NEGATIVE_INT: &str = "58ffffffffffffffffff01";

    /// `SignManager.compute_signature(nonce, timestamp, "app-id-1", "secret-1")` computed with
    /// Python's `hmac.new(secret, nonce+timestamp+key+secret, sha256).hexdigest()`.
    const GOLDEN_SIGNATURE: &str =
        "180ef55285415f1ad4adea160881eb8c7019ae56f5ac5f04576e3f2edd41b0d9";

    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        fn nibble(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            }
        }
        let bytes = hex.as_bytes();
        bytes
            .chunks(2)
            .map(|pair| {
                let high = nibble(pair[0]).unwrap_or_default();
                let low = pair
                    .get(1)
                    .and_then(|byte| nibble(*byte))
                    .unwrap_or_default();
                high << 4 | low
            })
            .collect()
    }

    fn describe_bytes(bytes: &[u8]) -> String {
        hex_lower(bytes)
    }

    /// The push payload fixture: mention + text + image, quote, owner and trace id.
    fn push_fixture() -> PushMsg {
        PushMsg {
            callback_command: String::new(),
            from_account: "user-2".to_string(),
            to_account: "bot-uid-1".to_string(),
            sender_nickname: "Ann".to_string(),
            group_code: "grp-1".to_string(),
            group_name: "Team".to_string(),
            msg_seq: 42,
            msg_random: 7,
            msg_time: 1_700_000_000_123,
            msg_key: "key-9".to_string(),
            msg_id: "msg-9".to_string(),
            msg_body: vec![
                MsgBodyElement {
                    msg_type: TIM_CUSTOM_ELEM.to_string(),
                    msg_content: MsgContent {
                        data: r#"{"elem_type": 1002, "text": "@vikram", "user_id": "bot-uid-1"}"#
                            .to_string(),
                        ..MsgContent::default()
                    },
                },
                MsgBodyElement::text("hi there"),
                MsgBodyElement {
                    msg_type: TIM_IMAGE_ELEM.to_string(),
                    msg_content: MsgContent {
                        uuid: "img-uuid".to_string(),
                        image_format: 3,
                        image_info_array: vec![ImageInfo {
                            kind: 1,
                            size: 12_345,
                            width: 640,
                            height: 480,
                            url: "https://example.test/i.png?resourceId=rid-7".to_string(),
                        }],
                        ..MsgContent::default()
                    },
                },
            ],
            cloud_custom_data:
                r#"{"quote": {"id": "msg-8", "desc": "quoted text", "sender_nickname": "Bo"}}"#
                    .to_string(),
            bot_owner_id: "user-2".to_string(),
            trace_id: "trace-1".to_string(),
            ..PushMsg::default()
        }
    }

    // ---- codec: golden vectors ----

    #[test]
    fn golden_conn_msg_simple_round_trips() {
        let golden = hex_to_bytes(GOLDEN_CONN_MSG_SIMPLE);
        let decoded = decode_conn_msg(&golden).expect("golden frame decodes");
        assert_eq!(decoded.head.cmd_type, CMD_TYPE_PUSH);
        assert_eq!(decoded.head.seq_no, 7);
        assert_eq!(decoded.data, b"hello");
        // Re-encoding the decoded frame reproduces the reference bytes exactly.
        assert_eq!(
            describe_bytes(&encode_conn_msg(&decoded)),
            GOLDEN_CONN_MSG_SIMPLE
        );
    }

    #[test]
    fn golden_auth_bind_matches_the_reference_bytes() {
        let request = AuthBindReq {
            biz_id: AUTH_BIZ_ID.to_string(),
            auth_info: AuthInfo {
                uid: "bot-uid-1".to_string(),
                source: "bot".to_string(),
                token: "tok-1".to_string(),
            },
            device_info: DeviceInfo {
                app_version: "1.2.3".to_string(),
                operation_system: "linux".to_string(),
                instance_id: INSTANCE_ID,
                bot_version: "1.2.3".to_string(),
            },
            env_name: String::new(),
        };
        let encoded = encode_auth_bind(&request, "auth-1", 0);
        assert_eq!(describe_bytes(&encoded), GOLDEN_AUTH_BIND);

        let frame = decode_conn_msg(&hex_to_bytes(GOLDEN_AUTH_BIND)).expect("frame decodes");
        assert_eq!(frame.head.cmd_type, CMD_TYPE_REQUEST);
        assert_eq!(frame.head.cmd, CMD_AUTH_BIND);
        assert_eq!(frame.head.module, MODULE_CONN_ACCESS);
        assert_eq!(
            decode_auth_bind(&frame.data).expect("payload decodes"),
            request
        );
    }

    #[test]
    fn golden_ping_and_ping_response() {
        assert_eq!(
            describe_bytes(&encode_ping("ping-1", 0)),
            GOLDEN_PING,
            "PING frame"
        );
        let ping = decode_conn_msg(&hex_to_bytes(GOLDEN_PING)).expect("frame decodes");
        assert_eq!(ping.head.cmd_type, CMD_TYPE_REQUEST);
        assert_eq!(ping.head.cmd, CMD_PING);
        assert_eq!(ping.head.msg_id, "ping-1");
        assert!(ping.data.is_empty());
        assert_eq!(decode_ping_req(&ping.data).expect("PingReq"), PingReq);

        let pong = decode_conn_msg(&hex_to_bytes(GOLDEN_PING_RSP)).expect("frame decodes");
        assert_eq!(pong.head.cmd_type, CMD_TYPE_RESPONSE);
        assert_eq!(pong.head.cmd, CMD_PING);
        assert_eq!(pong.head.seq_no, 3);
        assert_eq!(decode_ping_rsp(&pong.data).expect("PingRsp"), PingRsp);
    }

    #[test]
    fn golden_push_ack_echoes_the_push_head() {
        let head = Head {
            cmd_type: CMD_TYPE_PUSH,
            cmd: "inbound_message_push".to_string(),
            seq_no: 0,
            msg_id: "push-9".to_string(),
            module: BIZ_PACKAGE.to_string(),
            need_ack: true,
            status: 0,
        };
        assert_eq!(describe_bytes(&encode_push_ack(&head, 0)), GOLDEN_PUSH_ACK);
        let decoded = decode_conn_msg(&hex_to_bytes(GOLDEN_PUSH_ACK)).expect("frame decodes");
        assert_eq!(decoded.head.cmd_type, CMD_TYPE_PUSH_ACK);
        assert_eq!(decoded.head.cmd, head.cmd);
        assert_eq!(decoded.head.msg_id, head.msg_id);
        assert_eq!(decoded.head.module, head.module);
        assert!(decoded.data.is_empty());
    }

    #[test]
    fn golden_auth_bind_responses_decode_both_ways() {
        let ok = decode_conn_msg(&hex_to_bytes(GOLDEN_AUTH_BIND_RSP)).expect("frame decodes");
        let response = decode_auth_bind_rsp(&ok.data).expect("payload decodes");
        assert_eq!(response.code, 0);
        assert_eq!(response.connect_id, "connect-abc");
        assert_eq!(
            describe_bytes(&encode_auth_bind_response(&response, "auth-1", 0)),
            GOLDEN_AUTH_BIND_RSP
        );

        let refused = decode_conn_msg(&hex_to_bytes(GOLDEN_AUTH_BIND_RSP_ERR)).expect("decodes");
        let response = decode_auth_bind_rsp(&refused.data).expect("payload decodes");
        assert_eq!(response.code, 10_001);
        assert_eq!(response.message, "bad token");
        assert!(response.connect_id.is_empty());
    }

    #[test]
    fn golden_push_payload_decodes_and_re_encodes() {
        let payload = hex_to_bytes(GOLDEN_PUSH_PAYLOAD);
        let decoded = decode_push_msg(&payload).expect("payload decodes");
        assert_eq!(decoded, push_fixture());
        assert_eq!(decoded.msg_seq, 42);
        assert_eq!(
            decoded
                .msg_body
                .iter()
                .map(|e| e.msg_type.as_str())
                .collect::<Vec<_>>(),
            vec![TIM_CUSTOM_ELEM, TIM_TEXT_ELEM, TIM_IMAGE_ELEM]
        );
        let image = &decoded.msg_body[2].msg_content.image_info_array[0];
        assert_eq!(image.size, 12_345);
        assert_eq!(image.width, 640);
        assert_eq!(image.height, 480);
        assert_eq!(resource_id_from_url(&image.url), "rid-7");
        // A lossless round trip: the decoded message re-encodes to the same bytes.
        assert_eq!(
            describe_bytes(&encode_push_msg(&decoded)),
            GOLDEN_PUSH_PAYLOAD
        );
    }

    #[test]
    fn golden_directed_push_decodes_head_and_payload() {
        let frame = decode_conn_msg(&hex_to_bytes(GOLDEN_DIRECTED_PUSH)).expect("frame decodes");
        assert_eq!(frame.head.cmd_type, CMD_TYPE_PUSH);
        assert_eq!(frame.head.cmd, "inbound_message_push");
        assert_eq!(frame.head.seq_no, 5);
        assert_eq!(frame.head.msg_id, "push-9");
        assert_eq!(frame.head.module, BIZ_PACKAGE);
        assert!(frame.head.need_ack);
        let push = decode_push_msg(&frame.data).expect("payload decodes");
        assert_eq!(push, push_fixture());
        let rebuilt = ConnMsg {
            head: frame.head,
            data: encode_push_msg(&push),
        };
        assert_eq!(
            describe_bytes(&encode_conn_msg(&rebuilt)),
            GOLDEN_DIRECTED_PUSH
        );
    }

    #[test]
    fn golden_outbound_frames_match_the_reference_bytes() {
        let c2c = SendC2CMessageReq {
            msg_id: "c2c_0".to_string(),
            to_account: "user-2".to_string(),
            from_account: "bot-uid-1".to_string(),
            msg_body: vec![MsgBodyElement::text("hello")],
            ..SendC2CMessageReq::default()
        };
        assert_eq!(
            describe_bytes(&encode_send_c2c_message(&c2c, 0)),
            GOLDEN_SEND_C2C
        );

        let group = SendGroupMessageReq {
            msg_id: "grp_0".to_string(),
            group_code: "grp-1".to_string(),
            from_account: "bot-uid-1".to_string(),
            msg_body: vec![MsgBodyElement::text("pong")],
            ref_msg_id: "push-9".to_string(),
            ..SendGroupMessageReq::default()
        };
        assert_eq!(
            describe_bytes(&encode_send_group_message(&group, 0)),
            GOLDEN_SEND_GROUP
        );

        let private_heartbeat = SendPrivateHeartbeatReq {
            from_account: "bot-uid-1".to_string(),
            to_account: "user-2".to_string(),
            heartbeat: WS_HEARTBEAT_RUNNING,
        };
        assert_eq!(
            describe_bytes(&encode_send_private_heartbeat(
                &private_heartbeat,
                "hb_priv_0",
                1
            )),
            GOLDEN_SEND_PRIVATE_HEARTBEAT
        );

        let group_heartbeat = SendGroupHeartbeatReq {
            from_account: "bot-uid-1".to_string(),
            group_code: "grp-1".to_string(),
            send_time: 1_700_000_000_000,
            heartbeat: WS_HEARTBEAT_FINISH,
        };
        assert_eq!(
            describe_bytes(&encode_send_group_heartbeat(
                &group_heartbeat,
                "hb_grp_0",
                1
            )),
            GOLDEN_SEND_GROUP_HEARTBEAT
        );

        // Every outbound frame really is a request to the biz service.
        for golden in [GOLDEN_SEND_C2C, GOLDEN_SEND_GROUP] {
            let frame = decode_conn_msg(&hex_to_bytes(golden)).expect("frame decodes");
            assert_eq!(frame.head.cmd_type, CMD_TYPE_REQUEST);
            assert_eq!(frame.head.module, BIZ_PACKAGE);
        }
    }

    // ---- codec: robustness ----

    #[test]
    fn unknown_fields_are_skipped() {
        let base = decode_push_msg(&hex_to_bytes(GOLDEN_PUSH_PAYLOAD)).expect("payload decodes");
        let with_unknown =
            decode_push_msg(&hex_to_bytes(GOLDEN_PUSH_PAYLOAD_WITH_UNKNOWN)).expect("decodes");
        assert_eq!(
            base, with_unknown,
            "unknown fields must not change the message"
        );

        // Unknown fields inside an embedded message are skipped too.
        let mut content = encode_msg_content(&MsgContent {
            text: "a".to_string(),
            ..MsgContent::default()
        });
        put_varint(4242, 7, &mut content);
        put_bytes(4243, b"ignored", &mut content);
        let decoded = decode_msg_content(&content).expect("content decodes");
        assert_eq!(decoded.text, "a");

        // Units, floats and fixed fields are storable-but-unread: parse, never panic.
        let mut frame = encode_head(&Head {
            cmd_type: CMD_TYPE_RESPONSE,
            cmd: CMD_PING.to_string(),
            ..Head::default()
        });
        push_tag(9, WIRE_FIXED64, &mut frame);
        frame.extend_from_slice(&[0u8; 8]);
        push_tag(11, WIRE_FIXED32, &mut frame);
        frame.extend_from_slice(&[0u8; 4]);
        let head = decode_head(&frame).expect("fixed-width fields are skipped");
        assert_eq!(head.cmd, CMD_PING);
    }

    #[test]
    fn truncated_frames_are_typed_errors() {
        let payload = hex_to_bytes(GOLDEN_PUSH_PAYLOAD);
        // Every prefix must be panic-free; whether it decodes depends on where the cut lands.
        for cut in 1..payload.len() {
            let _ = decode_push_msg(&payload[..cut]);
        }
        // A cut inside a length-delimited field is always a typed truncation error: the prefix ends
        // mid-payload (`0a 00` | `12 06` + 1 of 6 bytes), mid-string, and right before the last
        // body element's content.
        for cut in [4usize, 5, 11, payload.len() / 2, payload.len() - 1] {
            let error = decode_push_msg(&payload[..cut]).expect_err("truncation must be an error");
            assert!(
                matches!(
                    error,
                    CodecError::Truncated { .. }
                        | CodecError::VarintTooLong { .. }
                        | CodecError::UnsupportedWireType { .. }
                ),
                "cut at {cut} produced {error:?}"
            );
        }
        // A frame that declares a payload it does not carry is truncated, not silently short.
        let frame = hex_to_bytes(GOLDEN_DIRECTED_PUSH);
        let error = decode_conn_msg(&frame[..frame.len() - 4]).expect_err("truncated frame");
        assert!(matches!(error, CodecError::Truncated { .. }), "{error:?}");

        // An empty frame is a valid empty message: the reference tolerates it everywhere.
        assert_eq!(
            decode_conn_msg(&[]).expect("empty frame").head,
            Head::default()
        );
    }

    #[test]
    fn bad_varints_and_wire_types_are_typed_errors() {
        // Eleven bytes, all continuation bits set: longer than protobuf allows.
        let mut too_long = vec![0x80u8; 11];
        too_long.push(0x01);
        let mut pos = 0usize;
        assert!(matches!(
            decode_varint(&too_long, &mut pos),
            Err(CodecError::VarintTooLong { .. })
        ));
        // A tag whose wire type is 3 (deprecated group start).
        let mut group = Vec::new();
        push_tag(5, 3, &mut group);
        assert!(matches!(
            parse_fields(&group),
            Err(CodecError::UnsupportedWireType { wire_type: 3, .. })
        ));
        // Wire type 7 is not a wire type at all.
        let mut bogus = Vec::new();
        push_tag(5, 7, &mut bogus);
        assert!(matches!(
            parse_fields(&bogus),
            Err(CodecError::UnsupportedWireType { wire_type: 7, .. })
        ));
        // A length prefix larger than the frame.
        let mut oversize = Vec::new();
        push_tag(3, WIRE_LEN, &mut oversize);
        push_varint(4096, &mut oversize);
        assert!(matches!(
            parse_fields(&oversize),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn negative_ints_use_64_bit_twos_complement() {
        // The bare varint is ten bytes; the golden vector is the same value at field 11.
        assert_eq!(
            describe_bytes(&encode_i64_varint(-1)),
            "ffffffffffffffffff01"
        );
        let mut out = Vec::new();
        put_varint(11, -1i64 as u64, &mut out);
        assert_eq!(describe_bytes(&out), GOLDEN_NEGATIVE_INT);

        let fields = Fields::parse(&hex_to_bytes(GOLDEN_NEGATIVE_INT)).expect("field decodes");
        assert_eq!(fields.varint(11), u64::MAX);
        assert_eq!(fields.varint(11) as i64, -1);
        // A negative `int32`-style value takes the ten-byte form too, and round trips.
        let mut buffer = Vec::new();
        put_varint(9, -2i64 as u64, &mut buffer);
        let fields = Fields::parse(&buffer).expect("field decodes");
        assert_eq!(fields.varint(9) as i64, -2);
        assert_eq!(buffer.len(), 11, "-2 is a ten-byte varint plus its tag");
    }

    #[test]
    fn repeated_fields_keep_wire_order() {
        let push = push_fixture();
        let encoded = encode_push_msg(&push);
        let fields = Fields::parse(&encoded).expect("payload parses");
        let bodies = fields.repeated_bytes(13);
        assert_eq!(bodies.len(), 3);
        assert_eq!(
            decode_msg_body_element(&Fields::parse(bodies[1]).expect("elem"))
                .expect("elem")
                .msg_content
                .text,
            "hi there"
        );

        // An empty repeated field decodes as an empty list, not as one empty entry.
        let fields = Fields::parse(&[]).expect("empty parses");
        assert!(fields.repeated_bytes(13).is_empty());

        // Recall callbacks carry a repeated sub-message; both entries survive in order.
        let recall = PushMsg {
            callback_command: "Group.CallbackAfterRecallMsg".to_string(),
            group_code: "grp-1".to_string(),
            recall_msg_seq_list: vec![
                RecallSeq {
                    msg_seq: 1,
                    msg_id: "m-1".to_string(),
                },
                RecallSeq {
                    msg_seq: 2,
                    msg_id: "m-2".to_string(),
                },
            ],
            ..PushMsg::default()
        };
        let decoded = decode_push_msg(&encode_push_msg(&recall)).expect("payload decodes");
        assert_eq!(decoded, recall);
        assert!(decoded.is_recall());
    }

    #[test]
    fn large_messages_round_trip() {
        let text: String = (0..8000)
            .map(|idx| char::from(b'a' + (idx % 26) as u8))
            .collect();
        let push = PushMsg {
            from_account: "user-2".to_string(),
            msg_id: "big-1".to_string(),
            msg_body: vec![MsgBodyElement::text(text.clone())],
            ..PushMsg::default()
        };
        let encoded = encode_push_msg(&push);
        assert!(encoded.len() > text.len());
        let decoded = decode_push_msg(&encoded).expect("large payload decodes");
        assert_eq!(decoded, push);
        assert_eq!(decoded.text(), text);

        // Multi-byte UTF-8 survives the length-prefixed encoding unchanged.
        let cjk = "元宝 ".repeat(1000);
        let push = PushMsg {
            from_account: "user-2".to_string(),
            msg_body: vec![MsgBodyElement::text(cjk.clone())],
            ..PushMsg::default()
        };
        let decoded = decode_push_msg(&encode_push_msg(&push)).expect("payload decodes");
        assert_eq!(decoded.text(), cjk.trim());
    }

    #[test]
    fn text_extraction_matches_the_reference_shapes() {
        let push = push_fixture();
        assert_eq!(push.text(), "@vikram hi there [image|ybres:rid-7]");
        assert_eq!(
            push.mentions(),
            vec![Mention {
                user_id: "bot-uid-1".to_string(),
                text: "@vikram".to_string(),
            }]
        );
        assert!(push.addressed_to("bot-uid-1"));
        assert!(!push.addressed_to("someone-else"));
        assert_eq!(
            push.quote(),
            (
                Some("msg-8".to_string()),
                Some("Bo: quoted text".to_string())
            )
        );

        // Media placeholders without a resourceId, a file with a name, an emoji and a card.
        let elements = vec![
            MsgBodyElement {
                msg_type: TIM_FILE_ELEM.to_string(),
                msg_content: MsgContent {
                    file_name: "report.pdf".to_string(),
                    url: "https://example.test/f.pdf?resourceId=rid-9".to_string(),
                    ..MsgContent::default()
                },
            },
            MsgBodyElement {
                msg_type: TIM_FACE_ELEM.to_string(),
                msg_content: MsgContent {
                    data: r#"{"name": "微笑"}"#.to_string(),
                    ..MsgContent::default()
                },
            },
            MsgBodyElement {
                msg_type: TIM_CUSTOM_ELEM.to_string(),
                msg_content: MsgContent {
                    data: r#"{"elem_type": 1010, "title": "card"}"#.to_string(),
                    ..MsgContent::default()
                },
            },
            MsgBodyElement {
                msg_type: TIM_SOUND_ELEM.to_string(),
                msg_content: MsgContent::default(),
            },
        ];
        let push = PushMsg {
            from_account: "user-2".to_string(),
            msg_id: "m-1".to_string(),
            msg_body: elements,
            ..PushMsg::default()
        };
        assert_eq!(
            push.text(),
            "[file:report.pdf|ybres:rid-9] [emoji: 微笑] [unsupported message type] [voice]"
        );

        // A leading full-width slash is rewritten so commands match.
        let command = PushMsg {
            from_account: "user-2".to_string(),
            msg_body: vec![MsgBodyElement::text("\u{ff0f}reset now")],
            ..PushMsg::default()
        };
        assert_eq!(command.text(), "/reset now");
    }

    #[test]
    fn signatures_and_timestamps_match_the_reference() {
        assert_eq!(
            compute_signature(
                "00112233445566778899aabbccddeeff",
                "2023-11-15T06:13:20+08:00",
                "app-id-1",
                "secret-1"
            ),
            GOLDEN_SIGNATURE
        );
        // Beijing time, second precision, no milliseconds.
        assert_eq!(
            build_timestamp_at(1_700_000_000),
            "2023-11-15T06:13:20+08:00"
        );
        assert_eq!(build_timestamp_at(0), "1970-01-01T08:00:00+08:00");
        // A pre-epoch instant still formats instead of panicking.
        assert_eq!(build_timestamp_at(-1), "1970-01-01T07:59:59+08:00");
        assert!(build_timestamp().ends_with("+08:00"));
        // Nonces are 32 hex characters, like `secrets.token_hex(16)`.
        let adapter = adapter_offline(YuanbaoConfig::new("app", "secret"));
        assert_eq!(adapter.next_nonce().len(), 32);
        let id = adapter.next_message_id();
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|ch| *ch == '-').count(), 4);
    }

    #[test]
    fn reconnect_policy_matches_the_reference() {
        assert_eq!(reconnect_delay(0), Duration::from_secs(1));
        assert_eq!(reconnect_delay(1), Duration::from_secs(2));
        assert_eq!(reconnect_delay(5), Duration::from_secs(32));
        assert_eq!(reconnect_delay(6), Duration::from_secs(60));
        assert_eq!(reconnect_delay(99), Duration::from_secs(60));
        for code in NO_RECONNECT_CLOSE_CODES {
            assert!(!should_reconnect(Some(code)), "{code} is permanent");
        }
        assert!(should_reconnect(Some(1000)));
        assert!(should_reconnect(None));
    }

    #[test]
    fn heartbeat_state_trips_at_the_threshold() {
        let mut state = HeartbeatState::default();
        assert!(!state.on_miss(), "one miss is tolerated");
        assert_eq!(state.misses(), 1);
        assert!(state.on_miss(), "the second consecutive miss reconnects");
        assert_eq!(state.on_pong(), 2, "a pong reports the misses it cleared");
        assert_eq!(state.misses(), 0);
        assert!(!state.on_miss());
    }

    // ---- adapter harness ----

    /// Scripted binary socket: records every frame written, replays queued frames on read.
    ///
    /// With `auto_reply` on, every business send request is answered immediately with a `status: 0`
    /// response carrying the request's own `msg_id`, the way the live gateway answers.
    struct MockSocket {
        outbound: Mutex<Vec<Vec<u8>>>,
        inbound: Mutex<VecDeque<Result<Option<Vec<u8>>, SocketError>>>,
        auto_reply: Mutex<bool>,
    }

    impl MockSocket {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                outbound: Mutex::new(Vec::new()),
                inbound: Mutex::new(VecDeque::new()),
                auto_reply: Mutex::new(false),
            })
        }

        fn auto_reply(&self) {
            *lock(&self.auto_reply) = true;
        }

        fn script(&self, frame: Vec<u8>) {
            lock(&self.inbound).push_back(Ok(Some(frame)));
        }

        fn script_error(&self, error: SocketError) {
            lock(&self.inbound).push_back(Err(error));
        }

        fn script_eof(&self) {
            lock(&self.inbound).push_back(Ok(None));
        }

        fn sent(&self) -> Vec<Vec<u8>> {
            lock(&self.outbound).clone()
        }

        fn clear_sent(&self) {
            lock(&self.outbound).clear();
        }
    }

    #[async_trait]
    impl BinarySocket for MockSocket {
        async fn send_binary(&self, data: &[u8]) -> Result<(), SocketError> {
            lock(&self.outbound).push(data.to_vec());
            if *lock(&self.auto_reply)
                && let Ok(request) = decode_conn_msg(data)
                && matches!(
                    request.head.cmd.as_str(),
                    "send_c2c_message" | "send_group_message"
                )
            {
                let response = encode_conn_msg(&ConnMsg {
                    head: Head {
                        cmd_type: CMD_TYPE_RESPONSE,
                        cmd: request.head.cmd.clone(),
                        seq_no: request.head.seq_no,
                        msg_id: request.head.msg_id.clone(),
                        module: BIZ_PACKAGE.to_string(),
                        need_ack: false,
                        status: 0,
                    },
                    data: Vec::new(),
                });
                lock(&self.inbound).push_back(Ok(Some(response)));
            }
            Ok(())
        }

        async fn next_binary(&self) -> Result<Option<Vec<u8>>, SocketError> {
            match lock(&self.inbound).pop_front() {
                Some(frame) => frame,
                None => Ok(None),
            }
        }
    }

    fn lock<T>(guard: &Mutex<T>) -> MutexGuard<'_, T> {
        guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records every request and replays scripted responses in order.
    struct MockTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<
            VecDeque<Result<crate::transport::HttpResponse, crate::transport::TransportError>>,
        >,
    }

    impl MockTransport {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::new()),
            })
        }

        fn script(&self, response: Value) {
            lock(&self.responses).push_back(Ok(crate::transport::HttpResponse {
                status: 200,
                body: response.to_string(),
                retry_after: None,
            }));
        }

        /// A response with an explicit status — the media path must not treat a non-2xx as success.
        fn script_status(&self, status: u16, response: Value) {
            lock(&self.responses).push_back(Ok(crate::transport::HttpResponse {
                status,
                body: response.to_string(),
                retry_after: None,
            }));
        }

        /// A transport-level failure (no response at all).
        fn script_error(&self, error: crate::transport::TransportError) {
            lock(&self.responses).push_back(Err(error));
        }

        fn requests(&self) -> Vec<HttpRequest> {
            lock(&self.requests).clone()
        }
    }

    #[async_trait]
    impl crate::transport::Transport for MockTransport {
        async fn execute(
            &self,
            request: HttpRequest,
        ) -> Result<crate::transport::HttpResponse, crate::transport::TransportError> {
            lock(&self.requests).push(request);
            match lock(&self.responses).pop_front() {
                Some(response) => response,
                None => Err(crate::transport::TransportError::Request(
                    "mock transport ran out of scripted responses".to_string(),
                )),
            }
        }
    }

    fn adapter_offline(config: YuanbaoConfig) -> YuanbaoAdapter {
        YuanbaoAdapter::new(config, MockSocket::new(), MockTransport::new())
    }

    fn adapter_with(
        config: YuanbaoConfig,
        socket: &Arc<MockSocket>,
        transport: &Arc<MockTransport>,
    ) -> YuanbaoAdapter {
        YuanbaoAdapter::new(config, socket.clone(), transport.clone())
    }

    fn token_config() -> YuanbaoConfig {
        YuanbaoConfig {
            app_id: "app-id-1".to_string(),
            app_secret: "secret-1".to_string(),
            bot_id: "bot-uid-1".to_string(),
            token: "tok-1".to_string(),
            ..YuanbaoConfig::default()
        }
    }

    fn auth_rsp_frame(response: &AuthBindRsp) -> Vec<u8> {
        encode_auth_bind_response(response, "auth-1", 0)
    }

    fn send_response_frame(req_id: &str) -> Vec<u8> {
        encode_conn_msg(&ConnMsg {
            head: Head {
                cmd_type: CMD_TYPE_RESPONSE,
                cmd: "send_c2c_message".to_string(),
                seq_no: 2,
                msg_id: req_id.to_string(),
                module: BIZ_PACKAGE.to_string(),
                need_ack: false,
                status: 0,
            },
            data: Vec::new(),
        })
    }

    /// A connected adapter whose socket has nothing queued.
    async fn connected_adapter(
        socket: &Arc<MockSocket>,
        transport: &Arc<MockTransport>,
    ) -> YuanbaoAdapter {
        socket.script(auth_rsp_frame(&AuthBindRsp {
            code: 0,
            message: String::new(),
            connect_id: "connect-abc".to_string(),
        }));
        let mut adapter = adapter_with(token_config(), socket, transport);
        assert!(adapter.connect(false).await, "handshake must succeed");
        socket.clear_sent();
        adapter
    }

    #[tokio::test]
    async fn auth_handshake_transcript_is_the_reference_frame() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        socket.script(auth_rsp_frame(&AuthBindRsp {
            code: 0,
            message: String::new(),
            connect_id: "connect-abc".to_string(),
        }));
        let config = token_config();
        let mut adapter = adapter_with(config.clone(), &socket, &transport);

        assert!(adapter.connect(false).await);
        assert!(adapter.is_connected());
        assert_eq!(adapter.connect_id().as_deref(), Some("connect-abc"));
        assert_eq!(adapter.bot_id(), "bot-uid-1");
        assert_eq!(adapter.last_error(), None);

        // The AUTH_BIND frame is byte-for-byte the reference's, apart from the random msg id.
        let sent = socket.sent();
        assert_eq!(sent.len(), 1);
        let frame = decode_conn_msg(&sent[0]).expect("frame decodes");
        assert_eq!(frame.head.cmd_type, CMD_TYPE_REQUEST);
        assert_eq!(frame.head.cmd, CMD_AUTH_BIND);
        assert_eq!(frame.head.module, MODULE_CONN_ACCESS);
        assert_eq!(frame.head.seq_no, 0);
        assert_eq!(frame.head.msg_id.len(), 36);
        let request = decode_auth_bind(&frame.data).expect("payload decodes");
        assert_eq!(request.biz_id, AUTH_BIZ_ID);
        assert_eq!(request.auth_info.uid, "bot-uid-1");
        assert_eq!(request.auth_info.source, "bot");
        assert_eq!(request.auth_info.token, "tok-1");
        assert_eq!(request.device_info.instance_id, INSTANCE_ID);
        assert_eq!(
            request.device_info.operation_system,
            config.operation_system
        );
        // No token was fetched: the config already carried one.
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn auth_handshake_fetches_a_sign_token_when_needed() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        transport.script(json!({
            "code": 0,
            "data": {"token": "tok-from-api", "bot_id": "bot-from-api", "duration": 7200, "source": "web"},
        }));
        socket.script(auth_rsp_frame(&AuthBindRsp {
            code: 0,
            message: String::new(),
            connect_id: "connect-xyz".to_string(),
        }));
        let config = YuanbaoConfig {
            bot_id: String::new(),
            token: String::new(),
            route_env: "test".to_string(),
            ..YuanbaoConfig::new("app-id-1", "secret-1")
        };
        let mut adapter = adapter_with(config, &socket, &transport);

        assert!(adapter.connect(false).await);
        // The token endpoint, headers and signed body are the reference's.
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            format!("{}{}", YUANBAO_API_DOMAIN, SIGN_TOKEN_PATH)
        );
        let body: Value = serde_json::from_slice(requests[0].body.as_deref().unwrap_or_default())
            .expect("body is json");
        assert_eq!(body["app_key"], json!("app-id-1"));
        let nonce = body["nonce"].as_str().unwrap_or_default().to_string();
        let timestamp = body["timestamp"].as_str().unwrap_or_default().to_string();
        assert_eq!(
            body["signature"].as_str().unwrap_or_default(),
            compute_signature(&nonce, &timestamp, "app-id-1", "secret-1")
        );
        assert!(timestamp.ends_with("+08:00"));
        for header in [
            "X-AppVersion",
            "X-OperationSystem",
            "X-Instance-Id",
            "X-Bot-Version",
            "X-Route-Env",
        ] {
            assert!(
                requests[0].headers.iter().any(|(key, _)| key == header),
                "missing header {header}"
            );
        }

        // The bot id came from the token, and the AUTH_BIND carried the API's source.
        assert_eq!(adapter.bot_id(), "bot-from-api");
        let frame = decode_conn_msg(&socket.sent()[0]).expect("frame decodes");
        let request = decode_auth_bind(&frame.data).expect("payload decodes");
        assert_eq!(request.auth_info.uid, "bot-from-api");
        assert_eq!(request.auth_info.token, "tok-from-api");
        assert_eq!(request.auth_info.source, "web");
        assert_eq!(request.env_name, "test");
    }

    #[tokio::test]
    async fn auth_failures_are_reported_truthfully() {
        // A refused bind: non-zero code.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        socket.script(auth_rsp_frame(&AuthBindRsp {
            code: 10_001,
            message: "bad token".to_string(),
            connect_id: String::new(),
        }));
        let mut adapter = adapter_with(token_config(), &socket, &transport);
        assert!(!adapter.connect(false).await);
        assert!(!adapter.is_connected());
        assert_eq!(adapter.connect_id(), None);
        assert!(
            adapter
                .last_error()
                .unwrap_or_default()
                .contains("code=10001"),
            "error was {:?}",
            adapter.last_error()
        );

        // A 0 code with no connectId is not a successful bind either.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        socket.script(auth_rsp_frame(&AuthBindRsp::default()));
        let mut adapter = adapter_with(token_config(), &socket, &transport);
        assert!(!adapter.connect(false).await);
        assert!(!adapter.is_connected());
        assert!(
            adapter
                .last_error()
                .unwrap_or_default()
                .contains("missing connectId")
        );

        // A socket that closes mid-handshake.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        socket.script_eof();
        let mut adapter = adapter_with(token_config(), &socket, &transport);
        assert!(!adapter.connect(false).await);
        assert!(!adapter.is_connected());

        // A socket error.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        socket.script_error(SocketError::Io("reset".to_string()));
        let mut adapter = adapter_with(token_config(), &socket, &transport);
        assert!(!adapter.connect(false).await);
        assert!(!adapter.is_connected());

        // Missing credentials never reach the socket.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let mut adapter = adapter_with(YuanbaoConfig::new("", "secret-1"), &socket, &transport);
        assert!(!adapter.connect(false).await);
        assert!(socket.sent().is_empty());
        assert!(transport.requests().is_empty());

        // A refused sign-token exchange.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        transport.script(json!({"code": 40001, "msg": "invalid signature"}));
        let mut adapter = adapter_with(
            YuanbaoConfig {
                token: String::new(),
                ..YuanbaoConfig::new("app-id-1", "secret-1")
            },
            &socket,
            &transport,
        );
        assert!(!adapter.connect(false).await);
        assert!(
            adapter
                .last_error()
                .unwrap_or_default()
                .contains("invalid signature")
        );

        // A token response with no bot id anywhere cannot bind.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        transport.script(json!({"code": 0, "data": {"token": "t", "bot_id": ""}}));
        let mut adapter = adapter_with(
            YuanbaoConfig {
                token: String::new(),
                ..YuanbaoConfig::new("app-id-1", "secret-1")
            },
            &socket,
            &transport,
        );
        assert!(!adapter.connect(false).await);
        assert!(
            adapter
                .last_error()
                .unwrap_or_default()
                .contains("no bot id")
        );
    }

    #[tokio::test]
    async fn heartbeat_frames_and_liveness() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;

        // A PING is a request frame whose payload is empty; the msg id is fresh each time.
        let (first_id, first) = adapter.ping_frame();
        let (second_id, _second) = adapter.ping_frame();
        assert_ne!(first_id, second_id);
        let frame = decode_conn_msg(&first).expect("ping decodes");
        assert_eq!(frame.head.cmd_type, CMD_TYPE_REQUEST);
        assert_eq!(frame.head.cmd, CMD_PING);
        assert_eq!(frame.head.module, MODULE_CONN_ACCESS);
        assert_eq!(frame.head.msg_id, first_id);
        assert!(frame.data.is_empty());
        // Sequence numbers advance across frames, and start where the handshake left off.
        assert_eq!(frame.head.seq_no, 1);

        // A pong response clears the miss counter.
        let pong = encode_conn_msg(&ConnMsg {
            head: Head {
                cmd_type: CMD_TYPE_RESPONSE,
                cmd: CMD_PING.to_string(),
                msg_id: first_id.clone(),
                module: MODULE_CONN_ACCESS.to_string(),
                ..Head::default()
            },
            data: Vec::new(),
        });
        assert!(!adapter.pong_missed(), "one miss is tolerated");
        assert_eq!(adapter.heartbeat_state().misses(), 1);
        let outcome = adapter.handle_frame(&pong).await.expect("pong handled");
        assert_eq!(
            outcome,
            FrameEvent::HeartbeatAck {
                msg_id: first_id.clone()
            }
        );
        assert_eq!(adapter.heartbeat_state().misses(), 0);

        // Reply heartbeats go to the right encoder per chat kind and are FINISH-able.
        adapter
            .send_heartbeat("direct:user-2", WS_HEARTBEAT_RUNNING)
            .await
            .expect("private heartbeat");
        adapter
            .send_heartbeat("group:grp-1", WS_HEARTBEAT_FINISH)
            .await
            .expect("group heartbeat");
        let sent = socket.sent();
        assert_eq!(sent.len(), 2);
        let private = decode_conn_msg(&sent[0]).expect("frame decodes");
        assert_eq!(private.head.cmd, "send_private_heartbeat");
        let group = decode_conn_msg(&sent[1]).expect("frame decodes");
        assert_eq!(group.head.cmd, "send_group_heartbeat");
        assert!(group.head.msg_id.starts_with("hb_grp_"));

        // Typing emits one RUNNING heartbeat.
        socket.clear_sent();
        adapter.send_typing("direct:user-2", None).await;
        let typing = socket.sent();
        assert_eq!(typing.len(), 1);
        let frame = decode_conn_msg(&typing[0]).expect("frame decodes");
        assert_eq!(frame.head.cmd, "send_private_heartbeat");
        // …and stopping emits nothing (send() owns FINISH).
        adapter.stop_typing("direct:user-2").await;
        assert_eq!(socket.sent().len(), 1);
    }

    #[tokio::test]
    async fn directed_push_becomes_an_event_and_is_acked() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;
        socket.script(hex_to_bytes(GOLDEN_DIRECTED_PUSH));

        let event = adapter
            .next_event()
            .await
            .expect("push decodes")
            .expect("push produced an event");
        assert_eq!(event.text, "@vikram hi there [image|ybres:rid-7]");
        // A media element wins the classification, exactly like `ClassifyMessageTypeMiddleware`.
        assert_eq!(event.message_type, MessageType::Photo);
        assert_eq!(event.source.platform.as_str(), "yuanbao");
        assert_eq!(event.source.chat_id, "group:grp-1");
        assert_eq!(event.source.chat_type, "group");
        assert_eq!(event.source.chat_name.as_deref(), Some("Team"));
        assert_eq!(event.source.thread_id.as_deref(), Some("main"));
        assert_eq!(event.user_id.as_deref(), Some("user-2"));
        assert_eq!(event.user_name.as_deref(), Some("Ann"));
        assert_eq!(event.message_id.as_deref(), Some("msg-9"));
        assert_eq!(event.reply_to_message_id.as_deref(), Some("msg-8"));
        assert_eq!(event.reply_to_text.as_deref(), Some("Bo: quoted text"));
        assert_eq!(
            event.metadata.get("chat_type").and_then(Value::as_str),
            Some("group")
        );
        assert_eq!(
            event.metadata.get("msg_seq").and_then(Value::as_u64),
            Some(42)
        );
        assert_eq!(
            event.metadata.get("trace_id").and_then(Value::as_str),
            Some("trace-1")
        );
        assert_eq!(
            event.metadata.get("mentions"),
            Some(&json!([{"user_id": "bot-uid-1", "text": "@vikram"}]))
        );
        assert_eq!(
            event
                .metadata
                .get("addressed_to_bot")
                .and_then(Value::as_bool),
            Some(true)
        );
        // The raw push is kept for adapter-specific handling.
        assert_eq!(
            event
                .raw_message
                .as_ref()
                .and_then(|raw| raw.get("group_code"))
                .and_then(Value::as_str),
            Some("grp-1")
        );

        // `need_ack` was set, so the push acknowledgement went out echoing the head.
        let sent = socket.sent();
        assert_eq!(sent.len(), 1);
        let ack = decode_conn_msg(&sent[0]).expect("ack decodes");
        assert_eq!(ack.head.cmd_type, CMD_TYPE_PUSH_ACK);
        assert_eq!(ack.head.cmd, "inbound_message_push");
        assert_eq!(ack.head.msg_id, "push-9");
        assert_eq!(ack.head.module, BIZ_PACKAGE);

        // Nothing else is queued.
        assert!(adapter.next_event().await.expect("no error").is_none());
    }

    #[tokio::test]
    async fn duplicate_and_self_pushes_are_dropped() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;

        socket.script(hex_to_bytes(GOLDEN_DIRECTED_PUSH));
        assert!(adapter.next_event().await.expect("decodes").is_some());

        // The same message id again: deduplicated.
        socket.script(hex_to_bytes(GOLDEN_DIRECTED_PUSH));
        assert!(
            adapter.next_event().await.expect("decodes").is_none(),
            "the second delivery is a duplicate"
        );

        // The bot's own message is ignored.
        let own = PushMsg {
            from_account: "bot-uid-1".to_string(),
            msg_id: "msg-own".to_string(),
            msg_body: vec![MsgBodyElement::text("echo")],
            ..PushMsg::default()
        };
        socket.script(encode_conn_msg(&ConnMsg {
            head: Head {
                cmd_type: CMD_TYPE_PUSH,
                cmd: "inbound_message_push".to_string(),
                msg_id: "push-own".to_string(),
                module: BIZ_PACKAGE.to_string(),
                ..Head::default()
            },
            data: encode_push_msg(&own),
        }));
        assert!(adapter.next_event().await.expect("decodes").is_none());

        // A push with no text at all produces no event either.
        let empty = PushMsg {
            from_account: "user-2".to_string(),
            msg_id: "msg-empty".to_string(),
            ..PushMsg::default()
        };
        socket.script(encode_conn_msg(&ConnMsg {
            head: Head {
                cmd_type: CMD_TYPE_PUSH,
                cmd: "inbound_message_push".to_string(),
                msg_id: "push-empty".to_string(),
                module: BIZ_PACKAGE.to_string(),
                ..Head::default()
            },
            data: encode_push_msg(&empty),
        }));
        assert!(adapter.next_event().await.expect("decodes").is_none());
    }

    #[tokio::test]
    async fn malformed_pushes_are_ignored_but_bad_frames_error() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;

        // A push whose payload is truncated: the frame is dropped, the connection survives.
        let truncated = encode_conn_msg(&ConnMsg {
            head: Head {
                cmd_type: CMD_TYPE_PUSH,
                cmd: "inbound_message_push".to_string(),
                msg_id: "push-bad".to_string(),
                module: BIZ_PACKAGE.to_string(),
                ..Head::default()
            },
            data: hex_to_bytes(GOLDEN_PUSH_PAYLOAD)[..12].to_vec(),
        });
        match adapter
            .handle_frame(&truncated)
            .await
            .expect("frame decodes")
        {
            FrameEvent::MalformedPush { error } => {
                assert!(matches!(error, CodecError::Truncated { .. }), "{error:?}");
            }
            other => panic!("expected a malformed push, got {other:?}"),
        }

        // Garbage that is not a ConnMsg at all is an error for the caller to count.
        assert!(adapter.handle_frame(b"\xff\xff\xff").await.is_err());

        // A well-formed push frame carrying unknown fields still yields its event: unknown fields
        // are skipped, not treated as an error.
        socket.script(encode_conn_msg(&ConnMsg {
            head: Head {
                cmd_type: CMD_TYPE_PUSH,
                cmd: "inbound_message_push".to_string(),
                msg_id: "push-10".to_string(),
                module: BIZ_PACKAGE.to_string(),
                ..Head::default()
            },
            data: hex_to_bytes(GOLDEN_PUSH_PAYLOAD_WITH_UNKNOWN),
        }));
        let event = adapter.next_event().await.expect("decodes").expect("event");
        assert_eq!(event.message_id.as_deref(), Some("msg-9"));
    }

    #[tokio::test]
    async fn send_writes_the_reference_frame_and_reports_the_response() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;
        socket.auto_reply();

        let result = adapter.send("direct:user-2", "hello", None, None).await;
        assert!(result.success, "{result:?}");
        assert!(
            result
                .message_id
                .as_deref()
                .unwrap_or_default()
                .starts_with("c2c_")
        );
        assert_eq!(
            result.continuation_message_ids,
            vec![result.message_id.clone().unwrap_or_default()]
        );

        // The first frame is the text C2C request; the second is the FINISH heartbeat.
        let sent = socket.sent();
        assert_eq!(sent.len(), 2);
        let frame = decode_conn_msg(&sent[0]).expect("frame decodes");
        assert_eq!(frame.head.cmd_type, CMD_TYPE_REQUEST);
        assert_eq!(frame.head.cmd, "send_c2c_message");
        assert_eq!(frame.head.module, BIZ_PACKAGE);
        assert_eq!(
            frame.head.msg_id,
            result.message_id.clone().unwrap_or_default()
        );
        let body = Fields::parse(&frame.data).expect("payload parses");
        assert_eq!(body.string(1), frame.head.msg_id);
        assert_eq!(body.string(2), "user-2");
        assert_eq!(body.string(3), "bot-uid-1");
        let element =
            decode_msg_body_element(&Fields::parse(body.repeated_bytes(5)[0]).expect("elem"))
                .expect("element decodes");
        assert_eq!(element, MsgBodyElement::text("hello"));
        let finish = decode_conn_msg(&sent[1]).expect("frame decodes");
        assert_eq!(finish.head.cmd, "send_private_heartbeat");

        // A group chat sends SendGroupMessage with the quoted message.
        let result = adapter
            .send("group:grp-1", "pong", Some("push-9"), None)
            .await;
        assert!(result.success, "{result:?}");
        assert!(
            result
                .message_id
                .as_deref()
                .unwrap_or_default()
                .starts_with("grp_")
        );
        let sent = socket.sent();
        let frame = decode_conn_msg(&sent[2]).expect("frame decodes");
        assert_eq!(frame.head.cmd, "send_group_message");
        assert_eq!(
            frame.head.msg_id,
            result.message_id.clone().unwrap_or_default()
        );
        let body = Fields::parse(&frame.data).expect("payload parses");
        assert_eq!(body.string(2), "grp-1");
        assert_eq!(body.string(7), "push-9", "the quoted message is referenced");
    }

    #[tokio::test]
    async fn send_classifies_failures_and_never_lies() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;

        // Blank content never reaches the socket.
        socket.clear_sent();
        let blank = adapter.send("direct:user-2", "   \n", None, None).await;
        assert!(!blank.success);
        assert_eq!(blank.error_kind.as_deref(), Some("bad_format"));
        assert!(socket.sent().is_empty());

        // A non-zero response status is a failure (the reference ignores it; see the module docs).
        socket.script(encode_conn_msg(&ConnMsg {
            head: Head {
                cmd_type: CMD_TYPE_RESPONSE,
                cmd: "send_c2c_message".to_string(),
                msg_id: "c2c_1".to_string(),
                module: BIZ_PACKAGE.to_string(),
                status: 500,
                ..Head::default()
            },
            data: Vec::new(),
        }));
        let rejected = adapter.send("direct:user-2", "hello", None, None).await;
        assert!(!rejected.success);
        assert_eq!(rejected.error_kind.as_deref(), Some("bad_format"));
        assert!(
            rejected
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("status=500")
        );

        // A closed socket is transient, so the ledger may retry.
        socket.script_eof();
        let closed = adapter.send("direct:user-2", "hello", None, None).await;
        assert!(!closed.success);
        assert_eq!(closed.error_kind.as_deref(), Some("transient"));
        assert!(closed.retryable);

        // Not connected at all.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = adapter_with(token_config(), &socket, &transport);
        let offline = adapter.send("direct:user-2", "hello", None, None).await;
        assert!(!offline.success);
        assert_eq!(offline.error_kind.as_deref(), Some("transient"));
        assert!(socket.sent().is_empty());
    }

    #[tokio::test]
    async fn long_messages_are_chunked_and_ordered() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;

        let content: String = (0..140)
            .map(|idx| format!("{idx:03} a sentence that exists mostly to occupy space\n"))
            .collect();
        assert!(content.chars().count() > YUANBAO_MAX_MESSAGE_LENGTH);
        socket.auto_reply();
        let result = adapter
            .send("direct:user-2", &content, Some("push-9"), None)
            .await;
        assert!(result.success, "{result:?}");
        assert!(result.continuation_message_ids.len() > 1);

        // Every chunk fits the cap, only the first carries the quote, and order is preserved.
        let mut texts = Vec::new();
        let mut refs = Vec::new();
        for frame in socket.sent() {
            let decoded = decode_conn_msg(&frame).expect("frame decodes");
            if decoded.head.cmd != "send_c2c_message" {
                continue;
            }
            let body = Fields::parse(&decoded.data).expect("payload parses");
            refs.push(body.string(7));
            let element =
                decode_msg_body_element(&Fields::parse(body.repeated_bytes(5)[0]).expect("elem"))
                    .expect("element decodes");
            texts.push(element.msg_content.text);
        }
        assert_eq!(result.continuation_message_ids.len(), texts.len());
        assert_eq!(
            result.message_id.as_deref(),
            result.continuation_message_ids.last().map(String::as_str)
        );
        for text in &texts {
            assert!(text.chars().count() <= YUANBAO_MAX_MESSAGE_LENGTH);
        }
        let joined: String = texts.concat();
        let strip = |value: &str| {
            value
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
        };
        assert_eq!(strip(&joined), strip(content.trim()));
    }

    #[tokio::test]
    async fn pushes_during_a_send_are_queued_not_lost() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;

        // A push arrives before the response to the send: it must be decoded, not swallowed.
        socket.script(hex_to_bytes(GOLDEN_DIRECTED_PUSH));
        socket.script(send_response_frame("c2c_1"));
        let result = adapter.send("direct:user-2", "hello", None, None).await;
        assert!(result.success, "{result:?}");
        let event = adapter.next_event().await.expect("decodes").expect("event");
        assert_eq!(event.message_id.as_deref(), Some("msg-9"));
    }

    #[tokio::test]
    async fn auth_answer_and_kickout_frames_are_surfaced() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = adapter_with(token_config(), &socket, &transport);

        let answer = auth_rsp_frame(&AuthBindRsp {
            code: 0,
            message: String::new(),
            connect_id: "connect-abc".to_string(),
        });
        assert_eq!(
            adapter.handle_frame(&answer).await.expect("handled"),
            FrameEvent::AuthAnswer {
                code: 0,
                message: String::new(),
                connect_id: Some("connect-abc".to_string()),
            }
        );
        assert!(adapter.is_connected());
        assert_eq!(adapter.connect_id().as_deref(), Some("connect-abc"));

        // A business response is surfaced with its head and payload.
        let response = send_response_frame("c2c_9");
        match adapter.handle_frame(&response).await.expect("handled") {
            FrameEvent::Response { head, data } => {
                assert_eq!(head.cmd, "send_c2c_message");
                assert_eq!(head.msg_id, "c2c_9");
                assert!(data.is_empty());
            }
            other => panic!("expected a response, got {other:?}"),
        }

        // An ignored frame type, then a kickout that unbinds the adapter.
        let ignored = encode_conn_msg(&ConnMsg {
            head: Head {
                cmd_type: 9,
                cmd: "update-meta".to_string(),
                ..Head::default()
            },
            data: Vec::new(),
        });
        assert_eq!(
            adapter.handle_frame(&ignored).await.expect("handled"),
            FrameEvent::Ignored
        );
        let kickout = encode_conn_msg(&ConnMsg {
            head: Head {
                cmd_type: CMD_TYPE_PUSH,
                cmd: CMD_KICKOUT.to_string(),
                ..Head::default()
            },
            data: Vec::new(),
        });
        assert_eq!(
            adapter.handle_frame(&kickout).await.expect("handled"),
            FrameEvent::Kickout
        );
        assert!(!adapter.is_connected());
        assert_eq!(adapter.connect_id(), None);
    }

    #[tokio::test]
    async fn adapter_capabilities_and_chat_info() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = adapter_with(token_config(), &socket, &transport);
        assert_eq!(adapter.max_message_length(), YUANBAO_MAX_MESSAGE_LENGTH);
        assert!(adapter.splits_long_messages());
        assert!(adapter.supports_code_blocks());
        assert_eq!(adapter.message_length("héllo"), 5);
        assert_eq!(
            adapter.get_chat_info("group:grp-1").await,
            ChatInfo {
                name: "group:grp-1".to_string(),
                chat_type: "group".to_string(),
            }
        );
        assert_eq!(
            adapter.get_chat_info("direct:user-2").await,
            ChatInfo {
                name: "direct:user-2".to_string(),
                chat_type: "dm".to_string(),
            }
        );
        // Configuration defaults are the reference's endpoints.
        assert_eq!(adapter.config().ws_url, YUANBAO_WS_URL);
        assert_eq!(adapter.config().api_domain, YUANBAO_API_DOMAIN);
        assert!(adapter.config().is_configured());
        assert!(!YuanbaoConfig::default().is_configured());
    }

    #[tokio::test]
    async fn disconnect_unbinds_and_clears_queued_events() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let mut adapter = connected_adapter(&socket, &transport).await;

        // Queue an event by handling a push frame directly.
        let frame = hex_to_bytes(GOLDEN_DIRECTED_PUSH);
        assert!(matches!(
            adapter.handle_frame(&frame).await.expect("handled"),
            FrameEvent::Push { event: Some(_), .. }
        ));

        socket.script_eof();
        adapter.disconnect().await;
        assert!(!adapter.is_connected());
        assert_eq!(adapter.connect_id(), None);
        // The queued event did not survive the disconnect.
        assert!(adapter.next_event().await.expect("no error").is_none());
    }

    #[test]
    fn dedup_expires_and_stays_bounded() {
        let mut dedup = Dedup::new(DEDUP_TTL, 2);
        assert!(!dedup.is_duplicate("a"));
        assert!(dedup.is_duplicate("a"));
        assert!(!dedup.is_duplicate("b"));
        // Capacity is enforced by evicting the oldest entry.
        assert!(!dedup.is_duplicate("c"));
        assert!(!dedup.is_duplicate("a"), "the oldest entry was evicted");
        // A zero TTL never remembers: every delivery is fresh.
        let mut short = Dedup::new(Duration::ZERO, 2);
        assert!(!short.is_duplicate("a"));
        assert!(!short.is_duplicate("a"));
    }

    #[test]
    fn sequence_numbers_wrap_like_a_uint32() {
        let adapter = adapter_offline(YuanbaoConfig::new("app", "secret"));
        assert_eq!(adapter.next_seq(), 0);
        assert_eq!(adapter.next_seq(), 1);
        {
            let mut runtime = adapter.lock();
            runtime.seq = 0xFFFF_FFFF;
        }
        assert_eq!(adapter.next_seq(), 0xFFFF_FFFF);
        assert_eq!(
            adapter.next_seq(),
            0,
            "the counter wraps instead of overflowing"
        );
    }

    #[test]
    fn signatures_vary_with_every_input() {
        let base = compute_signature("nonce", "stamp", "key", "secret");
        assert_ne!(base, compute_signature("nonce2", "stamp", "key", "secret"));
        assert_ne!(base, compute_signature("nonce", "stamp2", "key", "secret"));
        assert_ne!(base, compute_signature("nonce", "stamp", "key2", "secret"));
        assert_ne!(base, compute_signature("nonce", "stamp", "key", "secret2"));
        // Lower-case hex, 64 characters (SHA-256).
        assert_eq!(base.len(), 64);
        assert!(
            base.chars()
                .all(|ch| ch.is_ascii_hexdigit() && !ch.is_uppercase())
        );
    }

    // ---- outbound media: golden vectors ----
    //
    // Every constant below came from running the reference in this workspace:
    //
    //   python3 -c 'import sys, types; sys.modules["httpx"] = types.ModuleType("httpx")
    //               import yuanbao_media as m; print(m._cos_sign(...))'
    //
    // (`httpx` is stubbed because only the module's pure helpers are exercised offline.)

    /// `_cos_sign(method="put", path="/test-bucket-1250000000/reports/report%20one.pdf",
    /// params={}, headers={"host": "test-bucket-1250000000.cos.accelerate.myqcloud.com",
    /// "content-type": "application/pdf", "x-cos-security-token": "session-token-1"},
    /// secret_id="AKIDEXAMPLE1234567890", secret_key="secret-key-example",
    /// start_time=1700000000, expire_seconds=3600)`.
    ///
    /// The intermediates, so the signature can be recomputed by hand:
    ///
    /// ```text
    /// key             = "secret-key-example"
    /// q-sign-time     = "1700000000;1700003600"
    /// SignKey         = hex(HMAC-SHA1(key, q-sign-time))            = "9c8d6bd65792a2f720f7f6019c40645b7ffbb025"
    /// http-string     = "put\n/test-bucket-1250000000/reports/report%20one.pdf\n\ncontent-type=application%2Fpdf&host=test-bucket-1250000000.cos.accelerate.myqcloud.com&x-cos-security-token=session-token-1\n"
    /// SHA1(http-string)                                             = "7bfde07d4d3f7f21ebaa0195417d11efd86fbd0d"
    /// string-to-sign  = "sha1\n1700000000;1700003600\n7bfde07d4d3f7f21ebaa0195417d11efd86fbd0d\n"
    /// q-signature     = hex(HMAC-SHA1(SignKey, string-to-sign))
    /// ```
    const GOLDEN_COS_AUTHORIZATION: &str = concat!(
        "q-sign-algorithm=sha1&q-ak=AKIDEXAMPLE1234567890&q-sign-time=1700000000;1700003600",
        "&q-key-time=1700000000;1700003600",
        "&q-header-list=content-type;host;x-cos-security-token&q-url-param-list=",
        "&q-signature=fbd954322bf85e47dbac3c0602bc3dedd496b6ba"
    );

    /// `_cos_sign(method="get", path="/b/k.png", params={"a": "x y", "Z": "1"},
    /// headers={"host": "b.cos.accelerate.myqcloud.com", "content-type": "image/png"},
    /// secret_id="ak", secret_key="sk", start_time=1000, expire_seconds=100)` — the query-parameter
    /// branch, showing the lower-cased, percent-encoded, sorted `q-url-param-list`.
    const GOLDEN_COS_AUTHORIZATION_WITH_PARAMS: &str = concat!(
        "q-sign-algorithm=sha1&q-ak=ak&q-sign-time=1000;1100&q-key-time=1000;1100",
        "&q-header-list=content-type;host&q-url-param-list=a;z",
        "&q-signature=e6f514fcd6d58fefc3e4b5ee7a5a0f1e737772f6"
    );

    /// `_cos_sign` for the [`cos_credentials_fixture`] upload: the same secret and start time, key
    /// `/uploads/a3f1-img.png`, `content-type: image/png`.
    const GOLDEN_COS_UPLOAD_AUTHORIZATION: &str = concat!(
        "q-sign-algorithm=sha1&q-ak=AKIDEXAMPLE1234567890&q-sign-time=1700000000;1700003600",
        "&q-key-time=1700000000;1700003600",
        "&q-header-list=content-type;host;x-cos-security-token&q-url-param-list=",
        "&q-signature=1da2bed985abeb13bd6b91e2b0c1052e038f412a"
    );

    /// `hashlib.md5(png_fixture()).hexdigest()` — the element `uuid` of the fixture image.
    const GOLDEN_PNG_MD5: &str = "3ec9d84978a9cb6708f13767c7ec5226";

    /// The bucket name the COS fixture advertises (`genUploadInfo.data.bucketName`).
    const COS_BUCKET: &str = "test-bucket-1250000000";
    /// `genUploadInfo.data.encryptTmpSecretId`.
    const COS_SECRET_ID: &str = "AKIDEXAMPLE1234567890";
    /// `genUploadInfo.data.encryptTmpSecretKey`.
    const COS_SECRET_KEY: &str = "secret-key-example";
    /// `genUploadInfo.data.encryptToken`.
    const COS_SESSION_TOKEN: &str = "session-token-1";
    /// `genUploadInfo.data.resourceUrl` — what the media element points at.
    const COS_RESOURCE_URL: &str = "https://yuanbao.example.test/res/a3f1-img.png";

    /// A 29-byte PNG whose IHDR says 640×480 (`parse_image_size` reads it).
    fn png_fixture() -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        png.extend_from_slice(&[0x08, 0x02, 0x00, 0x00, 0x00]);
        png
    }

    /// A `genUploadInfo` success body shaped like the live API's `data` object.
    ///
    /// `expiredTime` is deliberately in the past, so the signature falls back to
    /// [`COS_SIGNATURE_TTL_SECONDS`] and `startTime` pins the q-sign time: the golden authorization
    /// is then deterministic whatever the wall clock says.
    fn cos_credentials_fixture() -> Value {
        json!({
            "code": 0,
            "msg": "",
            "data": {
                "bucketName": COS_BUCKET,
                "region": "ap-shanghai",
                "location": "/uploads/a3f1-img.png",
                "encryptTmpSecretId": COS_SECRET_ID,
                "encryptTmpSecretKey": COS_SECRET_KEY,
                "encryptToken": COS_SESSION_TOKEN,
                "startTime": 1_700_000_000,
                "expiredTime": 1_700_003_600,
                "resourceUrl": COS_RESOURCE_URL,
                "resourceID": "rid-1",
            }
        })
    }

    /// One outbound image carrying the fixture bytes.
    fn image_media() -> OutboundMedia {
        OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(png_fixture()))
            .with_filename("a3f1-img.png")
            .with_mime("image/png")
    }

    /// Case-insensitive header lookup on a recorded request.
    fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    // ---- outbound media: credentials ----

    #[test]
    fn cos_credentials_request_is_the_reference_gen_upload_info_call() {
        let file_id = "f".repeat(32);
        let request = cos_credentials_request(
            "https://bot.yuanbao.tencent.com/",
            "tok-1",
            "bot-uid-1",
            "app-id-1",
            "",
            "report.pdf",
            &file_id,
        );
        assert_eq!(request.method, crate::transport::HttpMethod::Post);
        assert_eq!(
            request.url,
            format!("{YUANBAO_API_DOMAIN}{UPLOAD_INFO_PATH}")
        );
        assert_eq!(header(&request, "Content-Type"), Some("application/json"));
        assert_eq!(header(&request, "X-Token"), Some("tok-1"));
        assert_eq!(header(&request, "X-ID"), Some("bot-uid-1"));
        assert_eq!(header(&request, "X-Source"), Some("web"));
        assert_eq!(
            header(&request, "X-Route-Env"),
            None,
            "no routing header unless one is configured"
        );
        let body: Value =
            serde_json::from_slice(request.body.as_deref().unwrap_or_default()).expect("json body");
        assert_eq!(body["fileName"], json!("report.pdf"));
        assert_eq!(body["fileId"], json!(file_id));
        assert_eq!(body["docFrom"], json!("localDoc"));
        assert_eq!(body["docOpenId"], json!(""));

        // `X-ID` is the bot id when there is one, else the app key; the route env is forwarded.
        let fallback = cos_credentials_request(
            YUANBAO_API_DOMAIN,
            "tok-1",
            "  ",
            "app-id-1",
            "test",
            "a.png",
            &file_id,
        );
        assert_eq!(header(&fallback, "X-ID"), Some("app-id-1"));
        assert_eq!(header(&fallback, "X-Route-Env"), Some("test"));
    }

    #[test]
    fn cos_credentials_response_parses_and_rejects_incomplete_data() {
        let credentials =
            parse_cos_credentials(&cos_credentials_fixture()).expect("the fixture is complete");
        assert_eq!(credentials.bucket_name, COS_BUCKET);
        assert_eq!(credentials.region, "ap-shanghai");
        assert_eq!(credentials.location, "/uploads/a3f1-img.png");
        assert_eq!(credentials.secret_id, COS_SECRET_ID);
        assert_eq!(credentials.secret_key, COS_SECRET_KEY);
        assert_eq!(credentials.session_token, COS_SESSION_TOKEN);
        assert_eq!(credentials.start_time, Some(1_700_000_000));
        assert_eq!(credentials.expired_time, Some(1_700_003_600));
        assert_eq!(credentials.resource_url, COS_RESOURCE_URL);
        assert_eq!(credentials.resource_id, "rid-1");

        // A non-zero code is a refusal, message included.
        let refused = parse_cos_credentials(&json!({"code": 40001, "msg": "invalid token"}))
            .expect_err("a non-zero code cannot yield credentials");
        assert!(refused.contains("code=40001"), "{refused}");
        assert!(refused.contains("invalid token"), "{refused}");
        // …whatever the code's JSON type is: only the number `0` (or no code at all) passes.
        assert!(
            parse_cos_credentials(&json!({"code": "40001"})).is_err(),
            "a string code is not the number 0"
        );
        // A `null` code is "no code", exactly like Python's `code is not None` guard.
        assert_eq!(
            parse_cos_credentials(&json!({
                "code": null, "bucketName": "b-3", "location": "k/3.png",
            }))
            .expect("a null code is treated as absent")
            .bucket_name,
            "b-3"
        );

        // `data = result.get("data") or result`: an absent or empty `data` falls back to the whole
        // envelope, which is then read as the data object.
        let flat = json!({"bucketName": "b-1", "location": "k/1.png"});
        assert_eq!(
            parse_cos_credentials(&flat)
                .expect("flat envelope")
                .bucket_name,
            "b-1"
        );
        let empty_data = json!({"code": 0, "data": {}, "bucketName": "b-2", "location": "k/2.png"});
        assert_eq!(
            parse_cos_credentials(&empty_data)
                .expect("empty data falls back")
                .location,
            "k/2.png"
        );

        // `bucketName` and `location` are both mandatory, and both are named when missing.
        let incomplete = parse_cos_credentials(&json!({"code": 0, "data": {"region": "x"}}))
            .expect_err("incomplete data is an error");
        assert!(incomplete.contains("bucketName"), "{incomplete}");
        assert!(incomplete.contains("location"), "{incomplete}");
    }

    // ---- outbound media: q-sign ----

    #[test]
    fn cos_signature_matches_the_reference_vector() {
        assert_eq!(
            cos_sign(
                "put",
                "/test-bucket-1250000000/reports/report%20one.pdf",
                &[],
                &[
                    ("host", "test-bucket-1250000000.cos.accelerate.myqcloud.com"),
                    ("content-type", "application/pdf"),
                    ("x-cos-security-token", "session-token-1"),
                ],
                "AKIDEXAMPLE1234567890",
                "secret-key-example",
                1_700_000_000,
                3600,
            ),
            GOLDEN_COS_AUTHORIZATION
        );
        // The method is lower-cased, the lists are lower-cased, percent-encoded and sorted.
        assert_eq!(
            cos_sign(
                "GET",
                "/b/k.png",
                &[("a", "x y"), ("Z", "1")],
                &[
                    ("Host", "b.cos.accelerate.myqcloud.com"),
                    ("Content-Type", "image/png"),
                ],
                "ak",
                "sk",
                1000,
                100,
            ),
            GOLDEN_COS_AUTHORIZATION_WITH_PARAMS
        );
        // Every input moves the signature.
        let base = cos_sign("put", "/k", &[], &[("host", "h")], "ak", "sk", 1000, 100);
        assert_ne!(
            base,
            cos_sign("get", "/k", &[], &[("host", "h")], "ak", "sk", 1000, 100)
        );
        assert_ne!(
            base,
            cos_sign("put", "/k2", &[], &[("host", "h")], "ak", "sk", 1000, 100)
        );
        assert_ne!(
            base,
            cos_sign("put", "/k", &[], &[("host", "h2")], "ak", "sk", 1000, 100)
        );
        assert_ne!(
            base,
            cos_sign("put", "/k", &[], &[("host", "h")], "ak2", "sk", 1000, 100)
        );
        assert_ne!(
            base,
            cos_sign("put", "/k", &[], &[("host", "h")], "ak", "sk2", 1000, 100)
        );
        assert_ne!(
            base,
            cos_sign("put", "/k", &[], &[("host", "h")], "ak", "sk", 1001, 100)
        );
        assert_ne!(
            base,
            cos_sign("put", "/k", &[], &[("host", "h")], "ak", "sk", 1000, 200)
        );
        // SHA-1 HMAC is 20 bytes → 40 hex characters, lower-cased.
        let signature = base.rsplit("q-signature=").next().unwrap_or_default();
        assert_eq!(signature.len(), 40);
        assert!(
            signature
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() && !ch.is_uppercase())
        );
    }

    // ---- outbound media: the PUT ----

    #[test]
    fn cos_put_request_carries_the_reference_method_url_headers_and_body() {
        let credentials =
            parse_cos_credentials(&cos_credentials_fixture()).expect("the fixture is complete");
        let png = png_fixture();
        // `now` is after the fixture's `expiredTime`, so the signature takes the 3600 s fallback
        // and `startTime` pins the q-sign time.
        let plan = cos_upload_plan(&credentials, "a3f1-img.png", "image/png", 1_800_000_000)
            .expect("a complete credential set plans");
        assert_eq!(
            plan.host,
            format!("{COS_BUCKET}{COS_ACCELERATE_HOST_SUFFIX}")
        );
        assert_eq!(plan.encoded_key, "uploads/a3f1-img.png");
        assert_eq!(
            plan.url,
            "https://test-bucket-1250000000.cos.accelerate.myqcloud.com/uploads/a3f1-img.png"
        );
        assert_eq!(plan.content_type, "image/png");
        assert_eq!(plan.authorization, GOLDEN_COS_UPLOAD_AUTHORIZATION);

        let request = plan.put_request(&png);
        assert_eq!(request.method, crate::transport::HttpMethod::Put);
        assert_eq!(request.url, plan.url);
        assert_eq!(
            header(&request, "Authorization"),
            Some(GOLDEN_COS_UPLOAD_AUTHORIZATION)
        );
        assert_eq!(header(&request, "Content-Type"), Some("image/png"));
        assert_eq!(
            header(&request, "x-cos-security-token"),
            Some(COS_SESSION_TOKEN)
        );
        assert_eq!(request.body.as_deref(), Some(png.as_slice()));

        // The object key is percent-encoded (`safe="/"`), its leading slashes dropped.
        let spaced = CosCredentials {
            location: "//uploads/a b/报告.png".to_string(),
            ..credentials.clone()
        };
        let spaced_plan =
            cos_upload_plan(&spaced, "a.png", "image/png", 1_800_000_000).expect("plan");
        assert_eq!(
            spaced_plan.encoded_key,
            "uploads/a%20b/%E6%8A%A5%E5%91%8A.png"
        );
        assert_eq!(
            spaced_plan.url,
            format!("https://{}/{}", spaced_plan.host, spaced_plan.encoded_key)
        );

        // MIME resolution is the reference's: a declared type wins; the octet-stream default only
        // re-derives for an image filename, and a non-image keeps octet-stream even with an
        // extension that would guess otherwise.
        let derive = |filename: &str, declared: &str| {
            cos_upload_plan(&credentials, filename, declared, 1_800_000_000)
                .expect("plan")
                .content_type
        };
        assert_eq!(derive("photo.PNG", "application/octet-stream"), "image/png");
        assert_eq!(
            derive("report.pdf", "application/octet-stream"),
            "application/octet-stream"
        );
        assert_eq!(derive("clip.mp4", ""), "application/octet-stream");
        assert_eq!(
            derive("notes.txt", "text/plain; charset=utf-8"),
            "text/plain; charset=utf-8"
        );

        // An incomplete credential set is refused before a request exists.
        let incomplete = CosCredentials {
            secret_key: String::new(),
            ..credentials
        };
        let error = cos_upload_plan(&incomplete, "a.png", "image/png", 1_800_000_000)
            .expect_err("a missing secret key cannot be signed with");
        assert!(error.contains("secretKey=false"), "{error}");
    }

    // ---- outbound media: message elements ----

    #[test]
    fn media_elements_match_the_reference_body_builders() {
        let upload = CosUpload {
            url: COS_RESOURCE_URL.to_string(),
            uuid: GOLDEN_PNG_MD5.to_string(),
            size: 29,
            width: 640,
            height: 480,
        };
        let image = image_msg_body(&upload, "a3f1-img.png", "image/png");
        assert_eq!(image.msg_type, TIM_IMAGE_ELEM);
        assert_eq!(image.msg_content.uuid, GOLDEN_PNG_MD5);
        assert_eq!(image.msg_content.image_format, 3, "image/png is format 3");
        assert_eq!(image.msg_content.image_info_array.len(), 1);
        let info = &image.msg_content.image_info_array[0];
        assert_eq!(info.kind, TIM_IMAGE_VARIANT_ORIGINAL);
        assert_eq!(info.size, 29);
        assert_eq!((info.width, info.height), (640, 480));
        assert_eq!(info.url, COS_RESOURCE_URL);

        // The `uuid` fallback chain: md5 → filename → URL basename → "image"; an unknown or absent
        // MIME type is format 255.
        let bare = CosUpload {
            url: "https://host/objects/fallback.png".to_string(),
            ..CosUpload::default()
        };
        assert_eq!(
            image_msg_body(&bare, "given.png", "").msg_content.uuid,
            "given.png"
        );
        assert_eq!(
            image_msg_body(&bare, "", "").msg_content.uuid,
            "fallback.png"
        );
        assert_eq!(
            image_msg_body(&CosUpload::default(), "", "")
                .msg_content
                .uuid,
            "image"
        );
        assert_eq!(
            image_msg_body(&bare, "a.png", "").msg_content.image_format,
            TIM_IMAGE_FORMAT_UNKNOWN
        );
        assert_eq!(
            image_msg_body(&bare, "a.bin", "application/zip")
                .msg_content
                .image_format,
            TIM_IMAGE_FORMAT_UNKNOWN
        );

        let file = file_msg_body(&upload, "report.pdf");
        assert_eq!(file.msg_type, TIM_FILE_ELEM);
        assert_eq!(file.msg_content.uuid, GOLDEN_PNG_MD5);
        assert_eq!(file.msg_content.file_name, "report.pdf");
        assert_eq!(file.msg_content.file_size, 29);
        assert_eq!(file.msg_content.url, COS_RESOURCE_URL);
        // Without a uuid the filename stands in, exactly like `uuid or filename`.
        let no_uuid = file_msg_body(&CosUpload::default(), "notes.txt");
        assert_eq!(no_uuid.msg_content.uuid, "notes.txt");
    }

    #[test]
    fn media_kind_maps_to_the_reference_element_type() {
        let upload = CosUpload {
            url: COS_RESOURCE_URL.to_string(),
            uuid: GOLDEN_PNG_MD5.to_string(),
            size: 29,
            ..CosUpload::default()
        };
        // Only images get a `TIMImageElem`; the reference has no video/voice body builder and
        // routes every other attachment through `DocumentHandler` (`send_direct`, adapter.py:2438).
        for (kind, expected) in [
            (MediaKind::Image, TIM_IMAGE_ELEM),
            (MediaKind::Document, TIM_FILE_ELEM),
            (MediaKind::Video, TIM_FILE_ELEM),
            (MediaKind::Audio, TIM_FILE_ELEM),
        ] {
            let element = media_msg_body(kind, &upload, "payload.bin", "image/png");
            assert_eq!(element.msg_type, expected, "{kind:?}");
        }
        assert_eq!(default_media_filename(MediaKind::Image), "image.jpg");
        for kind in [MediaKind::Document, MediaKind::Video, MediaKind::Audio] {
            assert_eq!(default_media_filename(kind), "document");
        }
    }

    // ---- outbound media: the whole send ----

    #[tokio::test]
    async fn send_media_uploads_the_bytes_then_posts_the_media_message() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;
        socket.auto_reply();
        transport.script(cos_credentials_fixture());
        transport.script(json!({"ETag": "\"etag-1\""}));

        let png = png_fixture();
        let media = image_media().with_caption("here you go");
        let result = adapter
            .send_media("direct:user-2", &media, Some("msg-8"))
            .await;
        assert!(result.success, "{result:?}");
        assert!(
            result
                .message_id
                .as_deref()
                .unwrap_or_default()
                .starts_with("c2c_")
        );

        // Two HTTP requests: genUploadInfo, then the COS PUT.
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(header(&requests[0], "X-Token"), Some("tok-1"));
        assert_eq!(header(&requests[0], "X-ID"), Some("bot-uid-1"));
        let credentials_body: Value =
            serde_json::from_slice(requests[0].body.as_deref().unwrap_or_default()).expect("json");
        assert_eq!(credentials_body["fileName"], json!("a3f1-img.png"));
        let file_id = credentials_body["fileId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert_eq!(file_id.len(), 32, "`generate_file_id()` is 16 bytes of hex");
        assert!(file_id.chars().all(|ch| ch.is_ascii_hexdigit()));

        assert_eq!(requests[1].method, crate::transport::HttpMethod::Put);
        assert_eq!(
            requests[1].url,
            "https://test-bucket-1250000000.cos.accelerate.myqcloud.com/uploads/a3f1-img.png"
        );
        assert_eq!(
            header(&requests[1], "Authorization"),
            Some(GOLDEN_COS_UPLOAD_AUTHORIZATION)
        );
        assert_eq!(header(&requests[1], "Content-Type"), Some("image/png"));
        assert_eq!(
            header(&requests[1], "x-cos-security-token"),
            Some(COS_SESSION_TOKEN)
        );
        assert_eq!(requests[1].body.as_deref(), Some(png.as_slice()));

        // Exactly one socket frame: the media message (no FINISH heartbeat on the media path, and
        // a C2C request has no `ref_msg_id` to carry the reply anchor).
        let sent = socket.sent();
        assert_eq!(sent.len(), 1);
        let frame = decode_conn_msg(&sent[0]).expect("frame decodes");
        assert_eq!(frame.head.cmd, "send_c2c_message");
        assert_eq!(frame.head.module, BIZ_PACKAGE);
        let body = Fields::parse(&frame.data).expect("payload parses");
        assert_eq!(body.string(2), "user-2");
        assert_eq!(body.string(3), "bot-uid-1");
        let elements = body.repeated_messages(5).expect("elements parse");
        assert_eq!(elements.len(), 2, "media element then caption");
        let media_element = decode_msg_body_element(&elements[0]).expect("media element");
        assert_eq!(media_element.msg_type, TIM_IMAGE_ELEM);
        assert_eq!(media_element.msg_content.uuid, GOLDEN_PNG_MD5);
        assert_eq!(media_element.msg_content.image_format, 3);
        let info = &media_element.msg_content.image_info_array[0];
        assert_eq!(info.kind, TIM_IMAGE_VARIANT_ORIGINAL);
        assert_eq!(info.size, 29);
        assert_eq!((info.width, info.height), (640, 480));
        assert_eq!(info.url, COS_RESOURCE_URL);
        assert_eq!(
            decode_msg_body_element(&elements[1]).expect("caption element"),
            MsgBodyElement::text("here you go")
        );
    }

    #[tokio::test]
    async fn send_media_group_message_carries_the_reply_anchor() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;
        socket.auto_reply();
        transport.script(cos_credentials_fixture());
        transport.script(json!({}));

        // A document: no image parsing, `TIMFileElem`, filename and size from the payload.
        let media = OutboundMedia::new(
            MediaKind::Document,
            MediaSource::Bytes(b"%PDF-1.4 fixture".to_vec()),
        )
        .with_filename("report.pdf");
        let result = adapter
            .send_media("group:grp-1", &media, Some("push-9"))
            .await;
        assert!(result.success, "{result:?}");
        assert!(
            result
                .message_id
                .as_deref()
                .unwrap_or_default()
                .starts_with("grp_")
        );

        let sent = socket.sent();
        assert_eq!(sent.len(), 1);
        let frame = decode_conn_msg(&sent[0]).expect("frame decodes");
        assert_eq!(frame.head.cmd, "send_group_message");
        let body = Fields::parse(&frame.data).expect("payload parses");
        assert_eq!(body.string(2), "grp-1");
        assert_eq!(
            body.string(7),
            "push-9",
            "the reply anchor is the ref_msg_id"
        );
        let elements = body.repeated_messages(6).expect("elements parse");
        assert_eq!(elements.len(), 1, "no caption, so one element");
        let element = decode_msg_body_element(&elements[0]).expect("file element");
        assert_eq!(element.msg_type, TIM_FILE_ELEM);
        assert_eq!(element.msg_content.file_name, "report.pdf");
        assert_eq!(element.msg_content.file_size, 16);
        assert_eq!(element.msg_content.url, COS_RESOURCE_URL);
        assert_eq!(element.msg_content.uuid, md5_hex(b"%PDF-1.4 fixture"));
        // The PUT's content type came from the filename's extension.
        assert_eq!(
            header(&transport.requests()[1], "Content-Type"),
            Some("application/pdf")
        );
    }

    // ---- outbound media: refusals and phase ordering ----

    #[tokio::test]
    async fn send_media_refuses_a_remote_url_without_any_request() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;

        let media = OutboundMedia::image_url("https://example.test/cat.png");
        let result = adapter.send_media("direct:user-2", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("need bytes"), "{error}");
        assert!(
            error.contains("cat.png"),
            "the refused URL is named: {error}"
        );
        assert!(
            transport.requests().is_empty(),
            "a URL is never uploaded unseen"
        );
        assert!(socket.sent().is_empty(), "and never announced");
    }

    #[tokio::test]
    async fn send_media_refuses_empty_bytes_and_oversized_payloads() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;

        // Empty bytes: `Empty file: <name>`, `bad_format`, no request at all.
        let empty = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(Vec::new()))
            .with_filename("empty.bin");
        let result = adapter.send_media("direct:user-2", &empty, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(result.error.as_deref(), Some("Empty file: empty.bin"));
        assert!(transport.requests().is_empty());
        assert!(socket.sent().is_empty());

        // The cap is `MEDIA_MAX_SIZE_MB = 50`: exactly 50 MiB fits, one byte more does not. The
        // predicate is length-based, so the boundary is exercised without allocating 50 MiB.
        let cap = usize::try_from(YUANBAO_MEDIA_MAX_SIZE_MB * 1024 * 1024).expect("fits");
        assert_eq!(media_size_error("ok.bin", cap), None);
        let error = media_size_error("big.bin", cap + 1).expect("over the cap");
        assert_eq!(error, "File too large: big.bin (50.0MB > 50MB)");

        // A one-byte payload is under the cap, so it does reach the network — the guard above is
        // the only thing between it and the upload, and the message phase still fails honestly.
        let tiny = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(vec![0u8]));
        transport.script(cos_credentials_fixture());
        transport.script(json!({}));
        socket.script_eof();
        let result = adapter.send_media("direct:user-2", &tiny, None).await;
        assert!(!result.success);
        assert_eq!(
            result.error_kind.as_deref(),
            Some("transient"),
            "a closed socket after a successful upload is transient"
        );
        assert_eq!(
            result.error.as_deref().unwrap_or_default(),
            "yuanbao media message: yuanbao socket: connection closed"
        );
        assert_eq!(transport.requests().len(), 2, "the upload itself happened");
    }

    #[tokio::test]
    async fn send_media_stops_at_the_phase_that_failed() {
        // (a) genUploadInfo refuses with a business code: no PUT, no message.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;
        socket.auto_reply();
        transport.script(json!({"code": 40001, "msg": "invalid token"}));
        let result = adapter
            .send_media("direct:user-2", &image_media(), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.starts_with("yuanbao media credentials:"), "{error}");
        assert!(error.contains("code=40001"), "{error}");
        assert_eq!(
            transport.requests().len(),
            1,
            "no PUT after a refused exchange"
        );
        assert!(
            socket.sent().is_empty(),
            "no message after a refused exchange"
        );

        // (b) genUploadInfo returns 500: retryable, and still nothing after it.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;
        socket.auto_reply();
        transport.script_status(500, json!({"msg": "boom"}));
        let result = adapter
            .send_media("direct:user-2", &image_media(), None)
            .await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .starts_with("yuanbao media credentials: HTTP 500")
        );
        assert_eq!(transport.requests().len(), 1);
        assert!(socket.sent().is_empty());

        // (c) the COS PUT is refused: the credentials request happened, the message did not.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;
        socket.auto_reply();
        transport.script(cos_credentials_fixture());
        transport.script_status(403, json!({}));
        let result = adapter
            .send_media("direct:user-2", &image_media(), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .starts_with("yuanbao media upload: HTTP 403"),
            "{:?}",
            result.error
        );
        assert_eq!(
            transport.requests().len(),
            2,
            "credentials and the PUT, nothing else"
        );
        assert!(
            socket.sent().is_empty(),
            "a rejected upload must never be followed by a media message"
        );

        // (d) the PUT never gets a response at all: transient, still no message.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;
        socket.auto_reply();
        transport.script(cos_credentials_fixture());
        transport.script_error(crate::transport::TransportError::Request(
            "connection reset".to_string(),
        ));
        let result = adapter
            .send_media("direct:user-2", &image_media(), None)
            .await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(socket.sent().is_empty());

        // (e) the message is rejected by the gateway: the failure names the message phase.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = connected_adapter(&socket, &transport).await;
        transport.script(cos_credentials_fixture());
        transport.script(json!({}));
        socket.script(encode_conn_msg(&ConnMsg {
            head: Head {
                cmd_type: CMD_TYPE_RESPONSE,
                cmd: "send_c2c_message".to_string(),
                msg_id: "c2c_1".to_string(),
                module: BIZ_PACKAGE.to_string(),
                status: 500,
                ..Head::default()
            },
            data: Vec::new(),
        }));
        let result = adapter
            .send_media("direct:user-2", &image_media(), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.starts_with("yuanbao media message:"), "{error}");
        assert!(error.contains("status=500"), "{error}");
    }

    #[tokio::test]
    async fn send_media_needs_a_connection_and_a_sign_token() {
        // Not connected: the reference's first check, retryable, no I/O.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = adapter_with(token_config(), &socket, &transport);
        let result = adapter
            .send_media("direct:user-2", &image_media(), None)
            .await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.error.as_deref(), Some("Not connected"));
        assert!(socket.sent().is_empty());
        assert!(transport.requests().is_empty());

        // Connected, but the sign token the upload must present is missing: refused locally rather
        // than POSTing an empty `X-Token` that can only 401.
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        transport.script(
            json!({"code": 0, "data": {"token": "", "bot_id": "bot-from-api", "source": "web"}}),
        );
        socket.script(auth_rsp_frame(&AuthBindRsp {
            code: 0,
            message: String::new(),
            connect_id: "connect-abc".to_string(),
        }));
        let config = YuanbaoConfig {
            token: String::new(),
            bot_id: String::new(),
            ..YuanbaoConfig::new("app-id-1", "secret-1")
        };
        let mut adapter = adapter_with(config, &socket, &transport);
        assert!(adapter.connect(false).await);
        socket.clear_sent();
        let result = adapter
            .send_media("direct:user-2", &image_media(), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("no sign token")
        );
        assert_eq!(
            transport.requests().len(),
            1,
            "only the handshake's own token request"
        );
        assert!(socket.sent().is_empty());
    }

    #[tokio::test]
    async fn supports_media_is_true_and_the_send_path_does_not_come_from_the_default() {
        let socket = MockSocket::new();
        let transport = MockTransport::new();
        let adapter = adapter_with(token_config(), &socket, &transport);
        assert!(adapter.supports_media());
        // The trait default would refuse with "this platform does not send media"; this adapter's
        // refusal for an offline send is its own ("Not connected"), which proves the override runs.
        let result = adapter
            .send_media("direct:user-2", &image_media(), None)
            .await;
        assert_ne!(
            result.error.as_deref(),
            Some("this platform does not send media")
        );
    }

    // ---- outbound media: the pure helpers behind it ----

    #[test]
    fn image_sizes_mime_tables_and_encoding_match_the_reference() {
        // PNG: IHDR's width/height.
        assert_eq!(parse_image_size(&png_fixture()), Some((640, 480)));
        // GIF: the little-endian logical screen.
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&240u16.to_le_bytes());
        assert_eq!(parse_image_size(&gif), Some((320, 240)));
        // JPEG: SOF0 carries height then width.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        jpeg.extend_from_slice(&480u16.to_be_bytes());
        jpeg.extend_from_slice(&640u16.to_be_bytes());
        jpeg.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(parse_image_size(&jpeg), Some((640, 480)));
        // WebP: the VP8X chunk's 24-bit little-endian dimensions, minus one.
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(b"VP8X");
        webp.extend_from_slice(&[0u8; 8]);
        webp.extend_from_slice(&639u32.to_le_bytes()[..3]);
        webp.extend_from_slice(&479u32.to_le_bytes()[..3]);
        assert_eq!(parse_image_size(&webp), Some((640, 480)));
        // Anything else is `None`, never a panic — including every prefix of a real PNG.
        assert_eq!(parse_image_size(b"not an image at all"), None);
        assert_eq!(parse_image_size(&[]), None);
        for cut in 0..png_fixture().len() {
            let _ = parse_image_size(&png_fixture()[..cut]);
        }

        assert_eq!(guess_mime_type("a.PNG"), "image/png");
        assert_eq!(guess_mime_type("archive.tar"), "application/x-tar");
        assert_eq!(guess_mime_type("noext"), "application/octet-stream");
        assert_eq!(guess_mime_type(".bashrc"), "application/octet-stream");
        assert_eq!(guess_mime_type("clip.mp4"), "video/mp4");
        assert!(is_image_filename("x.JPEG"));
        assert!(!is_image_filename("x.pdf"));

        assert_eq!(image_format_for_mime("image/png"), 3);
        assert_eq!(image_format_for_mime("IMAGE/JPEG"), 1);
        assert_eq!(image_format_for_mime("image/gif"), 2);
        assert_eq!(image_format_for_mime("image/bmp"), 4);
        assert_eq!(
            image_format_for_mime("image/webp"),
            TIM_IMAGE_FORMAT_UNKNOWN
        );
        assert_eq!(
            image_format_for_mime("application/pdf"),
            TIM_IMAGE_FORMAT_UNKNOWN
        );

        // `urllib.parse.quote` semantics: unreserved pass through, everything else %XX upper-case.
        assert_eq!(
            percent_encode("a b/报告~_.-"),
            "a%20b%2F%E6%8A%A5%E5%91%8A~_.-"
        );
        assert_eq!(percent_encode_path("/a b/c?.pdf"), "/a%20b/c%3F.pdf");
    }

    #[test]
    fn md5_matches_the_rfc_1321_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        assert_eq!(md5_hex(&png_fixture()), GOLDEN_PNG_MD5);
    }
}
