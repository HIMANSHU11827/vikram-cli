# VIKRAM gateway port plan — hermes-agent → Rust

Status: **approved — implementation started** (user @65: "approve everything, use your own brain, don't ask me").
Progress: foundation modules 1–4 DONE — `event.rs` (Platform, MessageType, ProcessingOutcome,
SessionSource, MessageEvent models), `routing.rs` (byte-compatible build_session_key), `platform.rs`
(SendResult, TurnReply, MessageHandler, ChatInfo, PlatformAdapter trait + capability flags),
`access.rs` (default-deny authorizer: two-layer own-policy + central union; truthy {true,1,yes};
allowlist grammar with `*` wildcard, JSON/comma lists; WhatsApp alias expansion; npub→hex; forum-family
group lists vs chat-scoped grants; unauthorized-DM resolver; PairingVerdict trait seam for pairing.rs),
`cargo test -p vikram-gateway` → 36 passed.
Next: pairing.rs → delivery.rs → media.rs → registry.rs, each verified green before moving on. NOTE:
`vikram-runtime` workspace lib does NOT compile right now (user actively rewriting WIP files — do not
touch, only use `cargo check|test -p vikram-gateway`).

## Goal

Port the hermes-agent **messaging gateway** (what talks to Telegram/Discord/Slack/29 platform
adapters) into VIKRAM, in Rust, as a complete port — not a stripped-down basic version.

## Source

`/home/himanshu/.hermes/hermes-agent` (git, v0.22.6 / 2026.9.24 stable, HEAD f0d94882e89a)

Measured scale (on disk):

| Area | Files | LOC |
|---|---|---|
| `gateway/` core | 343 py | ~128,360 |
| `plugins/platforms/` (29 adapters) | 155 py | ~68,685 |
| **Total to port** | ~500 py | **~197K** |

Reference implementation to mirror: `gateway/platforms/ADDING_A_PLATFORM.md`.

## VIKRAM target

Workspace `/home/himanshu/Desktop/VIKRAM-CLI`, 14 crates, resolver 3, edition 2024,
rust-version 1.86, license `MIT OR Apache-2.0`.

Gateway plugs into: `vikram_runtime::{Runtime, RuntimeHandle, EventBus}`
(same pattern as `vikram-daemon::runner::turn`).

- `Runtime::start(config)` → `handle()`
- `handle.create_session(title: Option<String>)` → session id
- `handle.submit(&session, message)`
- `handle.events()` → `broadcast::Receiver<RuntimeEvent>`
- headless reference: `crates/vikram-cli/src/chat.rs` (StreamDelta / TurnFinished loop)

Existing building blocks to reuse (do not duplicate):

- `vikram-contracts` — ids (UserId/SessionId/…), permission model, journal, Message/Block/ToolCall
- `vikram-policy` — deny-first permission floor
- `vikram-store` — SQLite persistence (sessions)
- `vikram-daemon` — cron/notify/postbox pattern, closest sibling, analogue of gateway "proactive delivery"
- `reqwest` (workspace dep, rustls + json + stream + http2), `tokio`, `rusqlite` bundled

VIKRAM has **zero** messaging/egress code today; this is greenfield in-crate territory.

## Port order

1. **Foundation** (this doc gets filled in once agent briefs land)
   - crate skeleton, platform trait + event model
   - gateway config + plugin registry (config keys, env vars, `AUTHZ`)
   - access policy + pairing persistence
   - session routing (platform chat/thread → vikram session)
   - outbound delivery pipeline (chunking, ledger, media cache, send targets)
2. **Telegram** end-to-end with real tests (plugin.yaml, adapter ~7.4K LOC py — largest single file)
3. **Discord**, 4. **Slack**, 5. **WhatsApp**, then remaining 25 one at a time.

Each platform port = one PR-sized unit: contract impl + real tests + doc update.

## Locked decisions (user approved @65)

1. **Placement** — NEW `vikram-gateway` crate. Daemon stays hive/scheduling; gateway is ingress→turn→egress.
2. **Fidelity** — FULL PORT with Rust-idiomatic internals. Every hermes module accounted for in this doc with
   `ported | merged-into-<x> | dropped because <why>`. Not a stripped basic version.
3. **Autonomy** — do not stop to ask the user; use own judgment, keep going.
4. Port one platform at a time; foundation first, then Telegram → Discord → Slack → WhatsApp → remaining 25.

## Findings to integrate (all 4 hermes research briefs — DONE)

- [x] platform adapter contract (base.py / event.py / shared_ingress / ADDING_A_PLATFORM.md)
- [x] boot / config / plugin registry
- [x] access policy / pairing / authz
- [x] delivery / media pipeline
- [ ] telegram specifics (8,726 LOC, 15 files, adapter.py 7,380 LOC, plugin.yaml captured) — after foundation

### Adapter contract (port skeleton)

- `trait PlatformAdapter` with exactly 4 required methods: `connect(is_reconnect) -> bool`,
  `disconnect()`, `send(chat_id, content, reply_to, metadata) -> SendResult`, `get_chat_info(chat_id) -> ChatInfo`.
- Documented required: `send_typing` (no-op default), `send_image`, `MAX_MESSAGE_LENGTH` (default 4096),
  `message_len_fn` (Telegram uses utf16_len), capability flags (splits_long_messages, supports_draft_streaming,
  send_path_degraded, supports_code_blocks, typed_command_prefix, enforces_own_access_policy, authorization_is_upstream).
- Default surface (port as provided helpers, not required overrides): send_voice/video/document/image_file/animation,
  edit_message/delete_message, truncate_message (fence-aware), send_draft, approval API
  (`send_clarify`, `send_slash_confirm`, `send_exec_approval` — callback ids `cl:<id>:<idx>`, `appr:<id>:<choice>`, `sc:<choice>:<id>`).
- `MessageEvent`/`SessionSource`/`MessageType`/`ProcessingOutcome` models; `MessageHandler = Fn(MessageEvent) -> TurnReply`;
  `handle_message` = identity-first admission → dedup → burst-gate → busy-session queue else background turn;
  turn ends via `handler(event)` returning text, then `_extract_response_content` splits text vs `MEDIA:<path>` directives.
- Approvals bypass the agent turn: tap `/approve|deny` route directly to resolvers (vikram: prompt → EventBus → resolve, same shape as `vikram-daemon::runner` auto-refusing approvals).

### Invariants that MUST hold

1. Two-phase plugin registration: bundled platforms register lazily via deferred loaders (no `plugins.enabled` required);
   user/project platforms require explicit `plugins.enabled` opt-in. passive `check_fn` never installs deps; active
   `ensure_deps_fn` may install at connect-time only. Conflation caused boot loops (#79812/#64674).
2. Session routing keys byte-compatible: `<ns>:<platform>:<chat_type>[:<scope_id if slack>][:<chat_id>][:<thread_id>][:<user>]`,
   ns = `agent:main` (default) / `agent:main~` (main profile) / `agent:<name>`. DMs isolate per chat_id; groups per user only if
   `group_sessions_per_user` and not shared thread.
3. Config precedence: env > config.yaml (top-level presence wins; `gateway.<key>` nested only if absent) > legacy `gateway.json` > defaults.
   `_enabled_explicit` marker is load-bearing.
4. Multiplex (`multiplex_profiles`) unset = never multiplex.
5. Flood control: adapters return `flood_control:<seconds>` instead of sleeping; the ledger owns the wait (`flood_not_before`).
6. `partial_overflow` contract: `SendResult` must carry `partial_overflow` + `delivered_chunks`/`total_chunks` — never re-send the whole message.
7. Streamed finals do NOT create ledger obligations (last streamed frame is the ACK).
8. Silent narration suppression (`*(silent)*`, 🔇, bare `.`) unless metadata job_id — anti bot-loop guard.
9. Default-deny access; a DM policy of `open` at boot is refused unless explicit allow-all.

### Module → Rust mapping (foundation pass)

| hermes module | Rust (vikram-gateway) |
|---|---|
| `event.py` (MessageEvent/MessageType/SessionSource models) | `event.rs` |
| `platforms/base.py` contract + capability flags + defaulted helpers | `platform.rs` (trait + defaults) + `helpers.rs` (fence-aware chunker, split_markdown_atoms) |
| `platforms/delivery.py` (DeliveryRouter/DeliveryTarget) + `delivery_ledger.py` | `delivery.rs` (router + ledger; SQLite table via vikram-store conventions, or own `gateway_state.db` — decide when store settles) |
| `delivery_ledger.py` states pending→attempting→delivered/failed→abandoned, MAX_ATTEMPTS=3, 24h stale, flood_not_before | ledger states + constants in `delivery.rs` |
| `media_cache.py` / `media_fetch/policy/repair.py` / docker path translation | `media.rs` (cache dirs, size limit 128 MiB default, `CachedMedia::context_note()`, `MEDIA:` directive parser, magic-byte sniff) |
| `platform_registry.py` / `platform_activation.py` / plugins_discovery | `registry.rs` (PlatformEntry, deferred register, activation predicate, check_fn/ensure_deps_fn split) |
| `config.py` / `config_loader.py` precedence + `bridge_toplevel_keys` + validation | `gateway_config.rs` (env > yaml > json > defaults, `_enabled_explicit`) |
| `access_policy_mixin.py` / `authz_mixin.py` (two-layer: adapter own-policy + central union) | `access.rs` (default-deny, truthy set {true,1,yes}, ENV_ALLOWLIST parse w/ `*` wildcard) |
| `pairing.py` (JSON store, salted hash, 0600, lock, per-platform pending/approved) | `pairing.rs` |
| `session.py` build_session_key | `routing.rs` (byte-compatible key builder + `gateway_routing`-style persistence model) |
| `bot_loop_guard.py` | `botloop.rs` (enabled max_events 20/300s, cooldown 600s) |
| `dead_targets.py` | `dead_targets.rs` (forbidden|not_found whole-chat skip) |
| approvals → resolve path | reuse vikram `EventBus` approval flow (ask→EventBus→AnyDesk chat buttons); prompt formats `cl:<id>:<idx>`, `appr:<id>:<choice>`, `sc:<choice>:<id>` |
| stream_consumer.py / rich_sent_store.py / message_timestamps.py | `stream.rs`, `ts_prefix.rs` as needed |

Deferred/dropped (decide at integration time): multiplex profile routing, shared ingress (/p/<profile>), rich_sent_store,
api_server + webhook + msgraph builtins, tui_gateway/cron coupling, voice/TTS — all still accounted for, not yet built.

### Plugin manifest format (captured from telegram)

`.hermes-plugin/plugin.yaml` → `kind: platform`, `name`, `label`, `version`, `requires_env`/`optional_env`
(name/description/prompt/password), stored under `~/.hermes/plugins/<name>/`; bundled ones live in `plugins/platforms/<name>/`.

## Checks that decide done (vikram standard)

```bash
cd /home/himanshu/Desktop/VIKRAM-CLI
cargo check --workspace --all-targets   # zero warnings
cargo test  --workspace                 # zero failed
cargo install --path crates/vikram-cli --force
```
