# Feature Audit — Implemented / Partial / Broken / Missing

Classification: Implemented · Partially implemented · Broken · Missing ·
Needs improvement · Not applicable.

> **Correction (2026-11-22, verified against tree `5a12d42`):** rows below are
> corrected in place where re-verified against current source in this pass —
> MCP serve (real, not a stub), MCP runtime stdio (piped), trust propagation
> (per-origin; the kernel crate was deleted in `fd929d9`), connect_all (now
> reports failures), the team model (renamed to hive rosters; the dead
> `hive.rs` duplicate was deleted), the evolution engine (wired; runtime runs
> bounded auto-cycles), and hybrid search (BM25 over FTS5 exists). Every other
> row — including the "Broken: 3" era counts and the "~900" test count — is
> the 2026-11-03 snapshot and was **not re-verified** in this pass. Tests
> today: 3,387 passing at `5a12d42`. Live status: `CONTINUE.md`,
> `docs/comparison.md`.

## Core runtime

| Feature | Status | Notes |
|---|---|---|
| Turn loop | Implemented | run_turn/spawn_turn, streaming, tool calls, approvals |
| Operation/effect kernel | ~~Implemented~~ moot (2026-11-22) | Kernel crate deleted in `fd929d9`; journal-first execution, idempotent journal replay and per-origin trust now live in the runtime turn loop (`turn.rs`) |
| Permission gating | Implemented | Deny-first, floor, postures, grants |
| Token meter | Implemented | len/4 heuristic (needs real counting) |
| Event bus | Implemented | tokio broadcast, cap 4096 |
| Cancellation | Implemented | finalize synthesizes pending results |

## Sessions & workspaces

| Feature | Status | Notes |
|---|---|---|
| Session CRUD | Implemented | create/list/get/search/rename/archive/duplicate/delete |
| Project association | Implemented | internal/external, status refresh |
| Session workspace | Implemented | lazy, session/project modes |
| Artifacts | Implemented | indexed, no copy |
| File changes + checkpoints | Implemented | restore supported |
| User/profile linkage | Partially implemented | create_session never sets user_id/profile_id |

## Agents / bots / hive

| Feature | Status | Notes |
|---|---|---|
| Agent profiles | Implemented | identity/title/role/specialization/status |
| Sub-agent spawning | Implemented | dedicated session per agent |
| Bot entities | Implemented | own context files |
| Hive group chat | Implemented | sequential + parallel runs |
| Hive roster (was Team JSON) | Implemented (renamed) | `teams` became hive rosters (`hive_roster.rs`, `hive_rosters` tables); the dead in-memory `hive.rs` duplicate was deleted — runtime now has `bots.rs`/`hive_roster.rs`/`hive_chat.rs` (`docs/RENAME-TEAM-HIVE-AGENT-BOT.md`) |
| Hive message trust | Needs improvement | Trust::User for all participants |

## Ticket System (unified)

| Feature | Status | Notes |
|---|---|---|
| Ticket CRUD | Implemented | post-audit |
| Participants + permissions | Implemented | backend-enforced |
| Messages (shared/private/internal) | Implemented | visibility filtering |
| Assignments | Implemented | linked to parent ticket |
| Events/history | Implemented | audit trail |
| Hive/bot/sub-agent integration | Implemented | create_hive_ticket, create_bot_ticket, create_subagent_assignment |
| CLI | Implemented | vikram tickets ... |
| Persistence/recovery | Implemented | SQLite, survives restart |
| Execution-attempt retry state machine | Missing | relies on journal idempotency |

## Memory

| Feature | Status | Notes |
|---|---|---|
| SQLite KV memory | Implemented | per user/profile/entity |
| Learning proposals | Partially implemented | instruction/identity policies write-only |
| Evolution engine | ~~Partially implemented~~ Implemented (wired) | not dormant: runtime tick runs bounded auto-cycles (`runtime.rs` `EVOLUTION_CYCLE_NAMESPACE`, dedup across restarts); CLI runs benchmarks/cycles; several engines still process-local (`vikram-evolution/src/lib.rs`) |
| Memory consolidation | Missing | dedup/conflict/forgetting |
| Semantic memory | Missing | no embeddings |
| Hybrid search | ~~Missing~~ Partially implemented | BM25 over FTS5 is real (`memory.rs`: `memory_fts` virtual table, porter tokenizer, `bm25` score field); no vector/embedding leg yet |

## Context

| Feature | Status | Notes |
|---|---|---|
| Markdown context loader | Implemented | mtime invalidation, budget, dedup |
| with_budget / invalidate_all | Partially implemented | few callers |

## Tools

| Feature | Status | Notes |
|---|---|---|
| Built-in tools | Implemented | fs, shell, web, git, browser, computer, pty, lsp, mcp, skills, memory |
| Tool gating | Implemented | policy + posture + grants |
| Cancellation for spawn_blocking | Missing | no cancel handle passed |
| Sandbox | Partially implemented | policy only + OS sandbox (bubblewrap/seatbelt) added; Windows/Termux pass-through |
| Hardcoded timeouts | Needs improvement | per tool |

## Skills

| Feature | Status | Notes |
|---|---|---|
| Discovery/loading/activation | Implemented | |
| Trust file handling | Needs improvement | silently defaults on corrupt file |
| History consistency | Needs improvement | latest() in-memory vs record() re-reads |
| Bundle index side effect | Needs improvement | Catalog::discover in library |
| Install timeout | Missing | git clone no timeout |

## MCP

| Feature | Status | Notes |
|---|---|---|
| Client discovery/connections/tools | Partially implemented | ~~connect_all always Ok~~ fixed 2026-11-22: returns `Err` naming failed servers (`mcp.rs:20`; regression test `client.rs:767`) |
| Server (serve) | ~~Broken (stub)~~ Implemented | REFUTED 2026-11-22: real JSON-RPC stdio serve behind a deny-first approved-tools gate (`main.rs:2595`, `server.rs:285-639`) |
| Runtime stdio | ~~Broken~~ Implemented | REFUTED 2026-11-22: stdin/stdout piped and used (`client.rs:483-485`); stderr inherited so server logs stay visible |
| Visibility persistence | Partially implemented | set_exposed_tools not persisted |

## Providers

| Feature | Status | Notes |
|---|---|---|
| OpenAI-compatible | Implemented | SSE streaming |
| Provider rebuild | Missing | set_model can't instantiate if missing at boot |
| Local models | Missing | no llama.cpp/MLX |

## TUI / CLI

| Feature | Status | Notes |
|---|---|---|
| TUI | Implemented | widgets, layout, input |
| CLI operator commands | Implemented | many groups |
| MCP serve | ~~Broken (stub)~~ Implemented | refuted 2026-11-22 — real serve (`main.rs:2595`, `server.rs:285-639`) |
| Doctor terminal size | Needs improvement | reads env |
| UTF-8 safety | Needs improvement | byte slicing panic risk |

## Security

| Feature | Status | Notes |
|---|---|---|
| Deny-first + floor | Implemented | |
| Floor vetoes | Needs improvement | only Write; docs say "human" |
| Trust propagation | ~~Needs improvement~~ Implemented | REFUTED 2026-11-22: requests are stamped per-origin — `Origin::trust` (`turn.rs:57`) and the `originating_trust` re-stamp (`turn.rs:2061`); the kernel crate that hardcoded `Trust::Model` was deleted (`fd929d9`) |
| Grants self-enforcement | Needs improvement | convention only |
| OS sandbox | Partially implemented | bubblewrap/seatbelt; Windows/Termux pass-through |
| Secret scanning | Missing | |
| At-rest encryption | Missing | |
| Taint tracking | Missing | |

## Evolution

| Feature | Status | Notes |
|---|---|---|
| Engine | ~~Partially implemented~~ Implemented (wired) | not dormant: runtime auto-cycle tick (`EVOLUTION_CYCLE_NAMESPACE`) + CLI benchmarks/cycles; some engines still process-local |
| Error handling | Needs improvement | silent swallowing |
| Async runner | Missing | |
| Path traversal guard | Missing | |
| Migration versioning | Missing | |

## Testing

| Feature | Status | Notes |
|---|---|---|
| Unit tests | Implemented | ~900 |
| Integration tests | Partially implemented | skills, mcp, tickets, store |
| E2E / property / fuzz / concurrency / security | Missing | |

## Cross-platform

| Feature | Status | Notes |
|---|---|---|
| Linux | Implemented | primary |
| macOS | Implemented | Seatbelt |
| Windows | Partially implemented | PTY shell, skills home, LSP URIs, which(), plugins fixed; sandbox pass-through |
| Termux/Android | Partially implemented | works via HOME/sh; desktop automation unsupported |
| Docker/Kubernetes | Partially implemented | scaffolding added |

## Summary counts (approx)

- Implemented: ~40
- Partially implemented: ~25
- Broken: 3 → all three refuted 2026-11-22 (MCP serve is real, stdio is piped,
  trust is per-origin); no confirmed-broken items remain from the original list
- Missing: ~30+
- Needs improvement: ~25+