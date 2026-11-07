# Gateway platform port — A-to-Z inventory

Source of truth: `~/.hermes/hermes-agent/plugins/platforms/` (27 adapters). This file records what
each platform actually is, measured from that source, and the port state in
`crates/vikram-gateway/src/platforms/`.

Measurement: file counts and Python LOC from the source tree; inbound transport classified by the
libraries and loop shapes the adapter uses; limits from the adapter's own
`MAX_MESSAGE_LENGTH`/`max_message_length`; auth keys from each `plugin.yaml`.

## Shared contract

Every adapter implements `PlatformAdapter` (`crates/vikram-gateway/src/platform.rs`) against the
frozen `Transport` seam (`src/transport.rs`): build an `HttpRequest`, receive an `HttpResponse`.
That is what makes every adapter testable offline with a mock transport. Chunking is
`helpers::truncate_message` (fence-aware); retries belong to `delivery::DeliveryLedger`; access
decisions belong to `access::authorize_source`; chat/thread → session keys belong to `routing`.

## Inventory

| # | Platform | Hermes LOC | Inbound | Auth env (prefix) | Outbound base | Limit | Port state |
|---|---|---|---|---|---|---|---|
| 1 | telegram | 8,726 | long poll / webhook | `TELEGRAM_BOT_TOKEN` | `api.telegram.org` | 4096 | **ported** (27 tests) |
| 2 | discord | 9,206 | gateway WS / REST | `DISCORD_BOT_TOKEN` | `discord.com/api/v10` | 2000 | **ported** (20 tests) |
| 3 | slack | 7,847 | socket mode / webhook | `SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN` | `slack.com/api` | 4000 per chunk | **ported** (19 tests) |
| 4 | feishu | 5,721 | WS / webhook | `FEISHU_APP_ID/SECRET` | `open.feishu.cn`, `accounts.larksuite.com` | 8000 | **ported** (18 tests) |
| 5 | yuanbao | 4,043 | WS / webhook | `YUANBAO_APP_ID`, `YUANBAO_APP_SECRET`, `YUANBAO_BOT_ID` | `bot.yuanbao.tencent.com` | — | **ported + wired** (35 tests, hand-written protobuf) |
| 6 | matrix | 3,537 | client sync loop | `MATRIX_HOMESERVER`, `MATRIX_ACCESS_TOKEN` | homeserver `/_matrix/client/v3` | 8000 | **ported** (22 tests) |
| 7 | photon | 3,059 | webhook / sidecar | `PHOTON_SIDECAR_URL`, `PHOTON_SIDECAR_TOKEN` | sidecar (`/inbound` NDJSON) | 8000 | **ported + wired** (12 tests) |
| 8 | qqbot | 2,838 | WS / webhook | `QQ_BOT_APP_ID`, `QQ_BOT_SECRET` | `api.sgroup.qq.com` | 4000 | **ported + wired** (31 tests, Ed25519 webhook) |
| 9 | google_chat | 2,585 | webhook | `GOOGLE_CHAT_SERVICE_ACCOUNT_JSON` | `chat.googleapis.com/v1` | 4000 | **ported** (9 tests, webhook mode) |
| 10 | buzz | 2,244 | relay WS / HTTP | `BUZZ_RELAY_URL`, `BUZZ_PRIVATE_KEY` | relay | — | **ported + wired** (46 tests, BIP-340 via k256) |
| 11 | wecom | 2,171 | WS / callback | `WECOM_BOT_ID`, `WECOM_SECRET` | `wss://openws.work.weixin.qq.com` | 4000 | **ported** (7 tests) |
| 12 | a2a | 2,115 | HTTP server / webhook | none (bearer optional, not enforced yet) | own host/port | — | **ported** (20 tests, engine + `/a2a`) |
| 13 | signal | 1,658 | `signal-cli` REST | `SIGNAL_HTTP_URL`, `SIGNAL_ACCOUNT` | local signal-cli HTTP | 8000 | **ported** (18 tests) |
| 14 | whatsapp | 1,545 | Baileys bridge (HTTP) | `WHATSAPP_ENABLED` | `127.0.0.1:3000` bridge | 4096 | **ported** (9 tests) |
| 15 | whatsapp_cloud | 1,538 | webhook | `WHATSAPP_CLOUD_*` | `graph.facebook.com` | 4096 | **ported** (10 tests) |
| 16 | weixin | 1,465 | poll / callback | `WEIXIN_ACCOUNT_ID`, `WEIXIN_TOKEN` | `ilinkai.weixin.qq.com` | 2000 | **ported** (8 tests) |
| 17 | dingtalk | 1,061 | stream WS / webhook | `DINGTALK_CLIENT_ID/SECRET` | `open.dingtalk.com` | 20000 | **ported + wired** (18 + 39 tests, stream mode) |
| 18 | email | 1,055 | IMAP poll + SMTP | `EMAIL_ADDRESS`, `EMAIL_SMTP_HOST` | SMTP/IMAP | 50 (subject preview) | **ported + wired** (15 tests; TLS + CLI) |
| 19 | teams | 1,033 | Bot Framework webhook | `TEAMS_CLIENT_ID/SECRET` | `login.microsoftonline.com`, `api.botframework.com` | 28000 | **ported** (24 tests) |
| 20 | line | 1,001 | webhook | `LINE_CHANNEL_ACCESS_TOKEN/SECRET` | `api.line.me/v2/bot` | 5000/bubble | **ported** (26 tests) |
| 21 | mattermost | 776 | WS / REST | `MATTERMOST_URL`, `MATTERMOST_TOKEN` | `MATTERMOST_URL/api/v4` | 4000 | **ported** (25 tests) |
| 22 | bluebubbles | 758 | webhook | `BLUEBUBBLES_SERVER_URL`, `BLUEBUBBLES_PASSWORD` | iMessage bridge | — | **ported** (21 tests) |
| 23 | simplex | 722 | WS | `SIMPLEX_WS_URL` | local simplex-chat WS | 8000 | **ported** (6 tests) |
| 24 | irc | 622 | IRC socket | `IRC_SERVER`, `IRC_CHANNEL`, `IRC_NICKNAME` | IRC server | 450 | **ported** (7 tests, plain TCP) |
| 25 | raft | 590 | local process | `RAFT_BRIDGE_TOKEN`, `RAFT_PROFILE` | local HTTP wake bridge | — | **ported + wired** (36 tests, wake-only) |
| 26 | ntfy | 384 | HTTP publish/poll | `NTFY_TOPIC`, `NTFY_SERVER_URL` | `ntfy.sh` | 4096 | **ported** (21 tests) |
| 27 | sms | 385 | Twilio webhook | `TWILIO_ACCOUNT_SID/AUTH_TOKEN` | `api.twilio.com/2010-04-01` | 1600 (~10 segments) | **ported** (27 tests) |

## Port order (by value and by shared machinery)

1. **Batch 1 (running):** telegram, discord, slack. These three cover the three common shapes —
   long-poll JSON, REST-with-bot-token, and Web-API-with-`ok:false` bodies.
2. **Batch 2:** whatsapp_cloud (Graph API), matrix (client sync), mattermost (REST + WS).
3. **Batch 3:** signal (signal-cli REST), bluebubbles (REST webhook), ntfy (trivial HTTP).
4. **Batch 4:** feishu, dingtalk, wecom, weixin (China-region suites, shared token refresh).
5. **Batch 5:** google_chat, teams, line, qqbot, yuanbao, a2a, buzz, simplex, irc, email, sms, raft,
   photon.

## Functionality every port must carry

For each platform, the Rust adapter must provide, or explicitly report as unsupported with the
platform's reason:

- connect / disconnect lifecycle, including reconnect semantics;
- text send with the platform's own length budget and chunking;
- reply/thread anchoring where the platform has it;
- typing indicator where available;
- media send (image/document) where available;
- edit / delete where available;
- chat metadata (`get_chat_info`) with a real `chat_type` for routing;
- error taxonomy mapped onto `SendResult::error_kind` (`too_long`, `bad_format`, `forbidden`,
  `not_found`, `rate_limited`, `transient`, `unknown`) and `retryable`/`retry_after`;
- inbound parsing into `MessageEvent` (text, captions, media, reply context, update ids);
- the platform's auth/signature verification for webhook-shaped platforms.

## Deliberate differences from hermes

- No Python plugin loader: bundled adapters are Rust modules registered in `platforms/mod.rs`;
  third-party platforms remain a plugin-host concern, not an adapter concern.
- No hidden global state: tokens are constructor arguments, so one process can run two accounts of
  the same platform without environment collisions.
- No inline sleeps for rate limits: adapters return `retryable` + `retry_after` and the delivery
  ledger owns the wait.

## Transport verbs (fixed after the first ports)

The first adapter pass exposed only `GET`/`POST`, which forced Matrix to send writes as
`POST + X-HTTP-Method-Override: PUT` and forced Discord/Mattermost to refuse edits. `transport.rs`
now exposes `Get`, `Post`, `Put`, `Patch`, `Delete` plus `put_json`/`patch_json`, and all three
adapters use the real verb. Any future adapter must use the real verb rather than a workaround.

## Remaining platforms and the seam each needs

24 of 27 are ported. The three left are not waiting on adapter logic — they are waiting on a
transport capability:

| Platform | Needs | Why the current seam cannot carry it |
|---|---|---|
| yuanbao | `wss://` | Tencent bot transport is a WebSocket with heartbeat/ack frames |
| buzz | `wss://` + secp256k1 | relay is WebSocket and events are Nostr-signed |
| raft | inbound server adapter | same shape as a2a: the channel is an HTTP listener the adapter serves |

Email is ported and wired: `TlsLineSocket` (rustls + webpki roots) supplies the encrypted stream, and
`vikram gateway` configures SMTP/IMAP from the environment.

The `Socket` seam supports both `ws://` and `wss://` (rustls) and plain TCP. Yuanbao additionally
needs binary frames plus a protobuf codec; buzz needs secp256k1/Nostr signing.
bridge with no outbound message API, so it cannot be a normal adapter.

## Host loop and the `vikram gateway` command

`host.rs` turns an adapter into a running pipeline:

```
next_events() -> handler(event) -> send(chat_id, reply_text)
```

`host::serve(platform, handler, stop)` owns the loop; a read error is counted and retried after a
short pause, and empty polls pause briefly so a non-blocking transport cannot spin. Wrappers own
each adapter's cursor: `TelegramPlatform` (update offset), `NtfyPlatform` / `WeixinPlatform` (sync
cursor), `SignalPlatform`, `SimplexPlatform`, `IrcPlatform`.

`vikram gateway` builds whatever the environment configures, starts one turn per event (session
per chat, headless/unattended posture), and sends the turn's final text back:

| Variable | Platform |
|---|---|
| `TELEGRAM_BOT_TOKEN` (+ optional `TELEGRAM_API_BASE` for a mirror/local Bot API) | Telegram |
| `DISCORD_BOT_TOKEN` (+ `DISCORD_API_BASE`, `DISCORD_GATEWAY_URL`) | Discord (REST send + gateway socket) |
| `SIGNAL_HTTP_URL` + `SIGNAL_ACCOUNT` | Signal (`signal-cli-rest-api`) |
| `NTFY_TOPIC` (+ `NTFY_SERVER_URL`, `NTFY_TOKEN`) | ntfy |
| `WEIXIN_ACCOUNT_ID` + `WEIXIN_TOKEN` (+ `WEIXIN_BASE_URL`) | Weixin |
| `SIMPLEX_WS_URL` | SimpleX (`ws://` or `wss://`) |
| `IRC_SERVER` + `IRC_NICKNAME` (+ `IRC_PORT`, `IRC_CHANNEL`) | IRC |
| `MATRIX_HOMESERVER` + `MATRIX_ACCESS_TOKEN` | Matrix |
| `WHATSAPP_BRIDGE_URL` | WhatsApp (local Baileys bridge) |
| `WECOM_BOT_ID` + `WECOM_SECRET` (+ `WECOM_WEBSOCKET_URL`, `WECOM_DEVICE_ID`) | WeCom |
| `MATTERMOST_URL` + `MATTERMOST_TOKEN` + `MATTERMOST_CHANNEL_ID` | Mattermost |
| `EMAIL_FROM`, `EMAIL_SMTP_HOST`, `EMAIL_IMAP_HOST` (+ `EMAIL_SMTP_PORT`, `EMAIL_IMAP_PORT`, `EMAIL_TLS`, `EMAIL_USERNAME`, `EMAIL_PASSWORD`, `EMAIL_MAILBOX`) | Email |
| `PHOTON_SIDECAR_URL`, `PHOTON_SIDECAR_TOKEN` | Photon (NDJSON `/inbound`) |
| `RAFT_BRIDGE_TOKEN` (+ `RAFT_RUNTIME_SESSION`, `RAFT_PATH`, `RAFT_HOST`, `RAFT_PORT`) | Raft (wake-only bridge) |
| `YUANBAO_APP_ID`, `YUANBAO_APP_SECRET` (+ `YUANBAO_BOT_ID`, `YUANBAO_WS_URL`, `YUANBAO_API_DOMAIN`) | Yuanbao (protobuf over WS) |
| `BUZZ_RELAY_URL`, `BUZZ_PRIVATE_KEY` (hex or `nsec1…`) | Buzz (Nostr relay) |
| `DINGTALK_CLIENT_ID`, `DINGTALK_CLIENT_SECRET` | DingTalk Stream Mode |
| `BLUEBUBBLES_SERVER_URL`, `BLUEBUBBLES_PASSWORD` | BlueBubbles (iMessage server) |
| `WHATSAPP_CLOUD_ACCESS_TOKEN`, `WHATSAPP_CLOUD_PHONE_NUMBER_ID`, `WHATSAPP_CLOUD_APP_SECRET`, `WHATSAPP_CLOUD_VERIFY_TOKEN` | WhatsApp Cloud API |
| `TWILIO_ACCOUNT_SID`, `TWILIO_AUTH_TOKEN`, `TWILIO_FROM_NUMBER`, `TWILIO_WEBHOOK_URL` | Twilio SMS |

Verified end-to-end against a local Bot API double: `getUpdates` → runtime turn → `sendMessage`
with the model's reply (MarkdownV2-escaped). Platforms without credentials are reported by name and
skipped; when none are configured the command says exactly which variables are missing.

## Webhook ingress

`ingress.rs` is a small HTTP/1.1 listener for platforms that push instead of poll: exact-path
routing, `Content-Length` bodies, 405 for non-POST, 413 for oversized bodies. A route handler does
its own verification before parsing. `WebhookPlatform` turns the routed events into the same
`GatewayPlatform` stream the polled adapters use, so the host loop and session mapping are shared.

Run it with `GATEWAY_WEBHOOK_BIND=host:port`:

| Route | Variables | Verification |
|---|---|---|
| `/feishu` | `FEISHU_APP_ID`, `FEISHU_APP_SECRET`, `FEISHU_VERIFICATION_TOKEN` (+ `FEISHU_API_BASE`) | `header.token` must match; `url_verification` challenge is answered |
| `/line` | `LINE_CHANNEL_ACCESS_TOKEN`, `LINE_CHANNEL_SECRET` | `X-Line-Signature` HMAC-SHA256 over the raw body |
| `/slack` | `SLACK_BOT_TOKEN`, `SLACK_SIGNING_SECRET` (+ `SLACK_API_BASE`) | `X-Slack-Signature` `v0=` HMAC-SHA256 over `v0:{timestamp}:{body}`; `url_verification` challenge answered |
| `/a2a` | none | JSON-RPC 2.0; `message/send` returns a submitted task, the reply is visible via `tasks/get` |
| `/.well-known/agent.json` (GET) | none | serves the A2A agent card |
| `/google_chat` | `GOOGLE_CHAT_WEBHOOK_URL`, `GOOGLE_CHAT_PROJECT_NUMBER` | Google-signed JWT (`Authorization`), aud = project number |
| `/teams` | `TEAMS_TENANT_ID`, `TEAMS_CLIENT_ID`, `TEAMS_CLIENT_SECRET` | Bot Framework JWT (`Authorization`), aud = client id |
| `/qqbot` | `QQ_BOT_APP_ID`, `QQ_BOT_SECRET` | Ed25519 (`X-Signature-Ed25519` over `X-Signature-Timestamp` + body); the secret must be the 32-byte seed |
| `/wake`, `/activity`, `/activity/drain`, `/health` | `RAFT_BRIDGE_TOKEN` | Raft bridge token (`x-raft-bridge-token`); `/health` unauthenticated |
| `/bluebubbles` | `BLUEBUBBLES_PASSWORD` | server password via `?password=` or `x-password` |
| `/whatsapp_cloud` (GET/POST) | `WHATSAPP_CLOUD_APP_SECRET`, `WHATSAPP_CLOUD_VERIFY_TOKEN` | Meta hub challenge + `X-Hub-Signature-256` |
| `/sms` (POST) | `TWILIO_AUTH_TOKEN`, `TWILIO_WEBHOOK_URL` | `X-Twilio-Signature` over the public URL + form params |

Verified end-to-end for Feishu, Slack and A2A: webhook POST (signed for Slack) → runtime turn → the
platform send API containing the model's reply; for A2A, `message/send` → `tasks/get` shows the
completed task with the reply as an artifact. Routes whose signatures are not implemented (Google
Chat, Teams, QQ Bot) are deliberately not exposed rather than accepting unverified traffic.


## Outbound media (18th pass)

`PlatformAdapter::send_media(chat_id, &OutboundMedia, reply_to)` carries images, video, audio and
documents. Implemented end-to-end for:

- **Telegram** — `sendPhoto`/`sendVideo`/`sendAudio`/`sendDocument`, multipart bytes or URL passthrough, caption cap 1024.
- **Discord** — multipart `payload_json` + `files[0]`, reply reference, 25 MiB cap.
- **Matrix** — `POST /_matrix/media/v3/upload` then `PUT …/send/m.room.message/{txn}`, reply relation, 100 MiB cap.
- **Slack** — `files.getUploadURLExternal` → upload → `files.completeUploadExternal`, comment/thread, 20 MiB cap.
- **Feishu** — `im/v1/images` or `im/v1/files` upload, then `im/v1/messages` with the returned key.
- **WhatsApp Cloud** — `/{phone}/media` upload → `messages` with `id` or `link`, per-kind captions and Meta size caps.
- **Mattermost** — `/api/v4/files` upload → `/api/v4/posts` with `file_ids`, `root_id`.
- **Email** — MIME `multipart/mixed` with base64 attachments and 76-char wrapping.
- **Signal** — inline `base64_attachments` (or staged file paths), caption, quote, 100 MB cap.
- **BlueBubbles** — `/api/v1/message/attachment` multipart, voice flag, caption follow-up.
- **Photon** — `/send-attachment` with a staged temp path, `voice`/`attachment` kind.
- **WhatsApp bridge** — `/send-media` with a staged temp path, per-kind caps.
- **LINE** — URL-only image/video/audio messages, preview URL, caption follow-up, HTTPS/length checks.
- **WeCom** — `aibot_upload_media_init` → 512 KiB chunks → `finish`, then the media message; 2/10/20 MB caps and file downgrades.
- **DingTalk** — URL images as markdown; local uploads refused (webhook has no media API).
- **QQ Bot** — `POST .../files` with `url`, then a media message (`msg_type 7`); bytes need the chunked uploader and are refused.
- **Weixin** — `ilink/bot/getuploadurl` → AES-128-ECB CDN upload → media item with `encrypt_query_param` and base64 AES key.
- **Teams** — message activity with `attachments[{contentType, contentUrl}]`; bytes become `data:` URIs, 28 MB cap.
- **Yuanbao** — `genUploadInfo` credentials → q-sign HMAC-SHA1 PUT to COS → `TIMImageElem`/`TIMFileElem` message body.

The remaining adapters inherit the trait default, which returns a typed `bad_format` failure — it never
pretends media was delivered. `GatewayPlatform::send_media` delegates for the three host wrappers above.
Media that needs a fetch (Discord/Matrix `MediaSource::Url`) is refused with a reason, because the
reference downloads bytes and this transport does not carry binary responses.


## Browser & TUI (19th pass)

- `crates/vikram-runtime/tests/browser_live.rs` drives the installed Chromium over CDP: local-page
  load + evaluate, trusted click mutating the DOM, typing producing real input events, a screenshot
  validated as PNG/IHDR, and a closed-port failure bounded in time. Chromium is discovered with the
  production discovery path; the tests skip cleanly only when no browser exists.
- TUI activity rows use one shared indent computation (`widgets/indent.rs`), so activity, tool-call
  and message rows align, wrapped lines keep their indent, and nesting steps by one width.


## Reactions, polls, drafts (20th pass)

- Reactions: Telegram (`setMessageReaction`), Discord (`PUT …/reactions/{emoji}/@me`), Slack
  (`reactions.add/remove`), Matrix (`m.annotation`; removal redacts the bot's own event).
- Polls: WhatsApp bridge (`send-poll`, 2–12 options, 255/100-char caps), default refusal elsewhere.
- Native draft streaming: Telegram (`sendMessageDraft`, UTF-16 cap, parse-retry) and Slack
  (`chat.startStream` / `chat.appendStream` deltas, cursor stripping, unsupported-marker latch).


## Rich cards and SimpleX media (21st pass)

- `send_card(chat_id, &Value)`: Slack Block Kit `blocks` + derived text fallback, Feishu
  `interactive` with `content {"card": …}`, Teams Adaptive Card attachment, Google Chat `cardsV2`.
  Invalid payloads refuse locally; `ok:false`/`code != 0`/missing ids are typed failures.
- SimpleX `send_media`: bytes are staged to a temp file (RAII cleanup) and sent as the reference's
  `filePath` item — `image` for PNG/JPEG (thumbnail empty, as the reference does without Pillow),
  `file` for documents/video and non-PNG images, `voice` for audio. URLs refused truthfully.


## Reactions 2, Discord embeds, computer tests (22nd pass)

- Feishu reactions: `POST …/messages/{id}/reactions` with `emoji_type`, reaction id cached per
  (message, emoji); delete targets the cached id, unknown id refuses without a request.
- Discord `send_card`: `embeds[]` plus a plain-content mirror (title/description) so the card is
  visible on clients that hide embeds; ≤10 embeds, invalid payloads refused locally.
- `crates/vikram-runtime/tests/computer_live.rs`: real screenshot (PNG IHDR, `grim` on Wayland),
  real OCR through tesseract (reads `VIKRAM OCR 12345` from an ImageMagick-rendered fixture),
  install-hint paths, and the absent-input-helper refusal.


## Inbound media pipeline (23rd pass)

`MediaEvent.media_urls` were parsed but unused. Now `crates/vikram-gateway/src/media_fetch.rs`
fetches them and the CLI submits the turn with images:

- http(s) only; `localhost`, `*.local`, `*.internal`, loopback/private/link-local/ULA literals and
  numeric IP spellings refused (lexical guard; DNS rebinding documented as out of scope);
- magic-byte sniffing (PNG/JPEG/GIF/WebP), declared content-type ignored, 4 items × 10 MiB caps,
  per-URL skips that never drop the text turn;
- `Transport::execute_bytes` (default refuses) with a real reqwest implementation;
- images become `ImageSource::Data` blocks with `Trust::External` — pixels inform, never authorize.
