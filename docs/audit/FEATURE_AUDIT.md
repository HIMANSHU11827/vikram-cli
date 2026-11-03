# Feature Audit — Implemented / Partial / Broken / Missing

Classification: Implemented · Partially implemented · Broken · Missing ·
Needs improvement · Not applicable.

## Core runtime

| Feature | Status | Notes |
|---|---|---|
| Turn loop | Implemented | run_turn/spawn_turn, streaming, tool calls, approvals |
| Operation/effect kernel | Implemented | Ordered ops, idempotent effects, cancellation |
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
| Team (JSON) | Partially implemented | duplicate with hive.rs (dead) |
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
| Evolution engine | Partially implemented | dormant (constructed, never called) |
| Memory consolidation | Missing | dedup/conflict/forgetting |
| Semantic memory | Missing | no embeddings |
| Hybrid search | Missing | no BM25/vector |

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
| Client discovery/connections/tools | Partially implemented | connect_all always Ok |
| Server (serve) | Broken (stub) | prints success, no server |
| Runtime stdio | Broken | null pipes |
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
| MCP serve | Broken (stub) | |
| Doctor terminal size | Needs improvement | reads env |
| UTF-8 safety | Needs improvement | byte slicing panic risk |

## Security

| Feature | Status | Notes |
|---|---|---|
| Deny-first + floor | Implemented | |
| Floor vetoes | Needs improvement | only Write; docs say "human" |
| Trust propagation | Needs improvement | hardcoded to Model in kernel |
| Grants self-enforcement | Needs improvement | convention only |
| OS sandbox | Partially implemented | bubblewrap/seatbelt; Windows/Termux pass-through |
| Secret scanning | Missing | |
| At-rest encryption | Missing | |
| Taint tracking | Missing | |

## Evolution

| Feature | Status | Notes |
|---|---|---|
| Engine | Partially implemented | dormant, not integrated |
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
- Broken: 3 (MCP serve, MCP stdio, trust hardcoding-as-broken-semantics)
- Missing: ~30+
- Needs improvement: ~25+