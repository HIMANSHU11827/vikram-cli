# Executive Summary — Vikram AI Agent

> Status: corrected 2026-11-22 against tree `5a12d42`. The sections below were
> originally written from the 2026-11-03 audit; superseded claims are corrected
> inline. Live status: `CONTINUE.md`, `docs/comparison.md`.

## Where Vikram stands today

VIKRAM is a **terminal AI runtime in Rust** (17 crates, ~247k LOC, 3,387 tests passing
at `5a12d42`) with a coherent multi-user/multi-entity architecture, deny-first
permissions, SQLite durable state, and OpenAI-compatible + native Anthropic providers.
It is **pre-release (0.1.0)**.

It is **well-architected at the core** (turn loop, permission gating, entity model,
journal-based state) but **incomplete in integration, reliability, security
enforcement and capabilities**.

## Strongest capabilities

1. Clean crate boundaries with a clear dependency graph (no cycles).
2. Deny-first security model with a non-widenable mandatory floor and 7 posture modes.
3. Journal-first execution — every effect recorded before it is applied (the
   Goose-inspired kernel machine crate itself was later deleted, `fd929d9`; the
   journal + per-origin trust stamping remain).
4. Rich entity model (identity / title / role / specialization / description / status).
5. Journal-based durable state with crash-safe effect replay.
6. Context loader (mtime invalidation, token budgeting, dedup).
7. Comprehensive skills system (discovery, loading, activation, trust, bundles, history).
8. Rich TUI (widgets, responsive layout, interactive input).
9. Unified Ticket System (Hive teams, bots, sub-agents) — implemented post-audit.
10. Cross-platform scaffolding (Windows/Termux/Docker/Kubernetes).

## Where Vikram is weaker than competitors

- **OS sandbox is partial** — bubblewrap/Seatbelt/Landlock via `vikram-sandbox`;
  Windows has a JobObjects stub and Termux passes through honestly (Codex's sandbox
  is broader).
- **No taint tracking** on observations (Gemini has a taint model).
- **Silent error swallowing** remains in pockets (store, config, context, learning);
  the memory store and `connect_all` were fixed — see
  `docs/VIKRAM_COMPLETE_REPAIR_ROADMAP.md` §2.
- **Evolution is wired but uneven** — the learning loop runs (experiences →
  reflection → gated proposals, benchmarks, scheduled cycles), but several engines
  are still process-local (`vikram-evolution/src/lib.rs`).
- ~~Duplicate abstractions~~ — the duplicate team models were resolved by the
  team→hive-roster rename (`docs/RENAME-TEAM-HIVE-AGENT-BOT.md`); the two learning
  systems (JSON `learning.rs` + SQLite `vikram-evolution`) remain separate.
- ~~MCP serve is a stub; MCP runtime uses null stdio~~ — **refuted 2026-11-22:**
  serve is a real deny-first server, stdio is piped (`client.rs:483-485`).
- Code indexing (`vikram-codeindex`), ACP (`vikram-acp`), scheduling/triggers
  (`vikram-daemon`), the verification loop (`verification.rs`) and an A2A gateway
  module all exist now; still missing: local models, and mid-turn
  **execution**-resume (interrupted-session recovery landed, `a1fecc9`).
- No secret scanning, no at-rest encryption.
- No E2E/property/fuzz/concurrency/security tests; integration coverage exists
  only in pockets (store, skills, mcp, tickets).

## Important features completely missing

Still genuinely missing (verified 2026-11-22): local model inference (llama.cpp/MLX),
semantic memory (embeddings), taint tracking, secret scanning, at-rest encryption,
deadlock detection, and mid-turn **execution**-resume (interrupted-session recovery
landed as `vikram sessions interrupted [--recover]`, `a1fecc9`).

Built since this summary was first written: code indexing, ACP, OS sandbox (partial),
scheduling/proactive triggers, the verification loop, an A2A gateway module,
desktop/phone notifications (`computer.notify`), git integration, test execution,
build validation, checkpoint/undo, the code-review tool, memory consolidation,
FTS5 hybrid search, agent-to-agent messaging via tickets, and agent performance
tracking.

## Best competitor per major area

| Area | Best | Why |
|---|---|---|
| Sandbox | Codex | Seatbelt/bwrap/Landlock, execpolicy DSL |
| Config layering | Codex | 10-layer config with non-widenable requirements tier |
| Provider model | Goose | Declarative JSON providers, capability flags |
| Durable semantics | OpenCode | Inbox/steer, compaction-as-data, stale-row settlement, schedule tool |
| Policy engine | Gemini CLI | Tiered TOML, BeforeToolSelection hook |
| Hook catalogue | Gemini CLI | Rich event hooks, loop detection |
| ACP | OpenHands / Hermes | Agent Client Protocol both directions |
| Container sandbox | OpenHands | Remote workspace, A2A |
| Multi-agent | OpenHands | Subagents + continuation sessions |
| Memory | Hermes / OpenClaw | FTS5 + Honcho + skills; dreaming consolidation |
| Scheduling | OpenClaw / Hermes | cron/interval/on-exit/stream triggers, fire-claim leases |
| Self-improvement | Hermes | background_review fork + skill creation |
| Security model | Codex | execpolicy DSL, pure approval function |
| Code understanding | Aider (dormant) | tree-sitter repo-map, def/ref graph |
| Extensibility | Claude Code | Plugin-first, mods system, dogfood rule |

## What to build first (P0)

The 2026-11-03 P0 list has since landed or been refuted (2026-11-22):

1. ~~Fix kernel trust propagation~~ — landed as per-origin trust stamping
   (`turn.rs:57-65, 2061`); the kernel crate itself was later deleted (`fd929d9`).
2. ~~Implement MCP serve (real server, fix null stdio)~~ — done: real deny-first
   serve, piped stdio.
3. ~~Fix silent error swallowing (memory, store, config, context, learning)~~ —
   memory store and `connect_all` fixed (see
   `docs/VIKRAM_COMPLETE_REPAIR_ROADMAP.md` §2); other discard sites remain.
4. ~~Implement OS-level sandbox~~ — `vikram-sandbox` (bubblewrap/seatbelt/landlock;
   Windows/Termux pass-through is the honest limit).
5. ~~Verification loop~~ — `verification.rs` + `verify` tool.
6. ~~Durable mid-turn resume~~ — partial: interrupted-session recovery landed
   (`a1fecc9`); execution-resume is still open.

The live P0 list is in `CONTINUE.md`: semantic verification, the WASM `prompts`
consumer, and multi-agent orchestration depth.

## What to avoid

- Rebuilding bots/sub-agents/Hive — extend the unified Ticket System instead.
- A second learning system — unify JSON + SQLite.
- Duplicate team models — consolidate hive.rs/teams.rs/hive_chat.rs.
- Heavy abstractions before evidence of need.

## Architecture improvements necessary

- Remove duplicate abstractions (hive.rs, second learning system, duplicate team model).
- Integrate evolution engine into runtime/TUI/CLI.
- Refactor RuntimeInner god struct.
- Enrich ToolContext (session/agent identity, store access).
- Add real token counting (replace len/4).
- Persist learning policies.
- Fix store cascades and user/profile linkage.
- Add cancellation for spawn_blocking tools.

## What could make Vikram genuinely different

No competitor excels at: **code intelligence** (nobody indexes), **durable mid-turn
resume** (nobody does it), and a **unified verification loop** (nobody has it). These
are Vikram's differentiation opportunities. *(2026-11-22: VIKRAM has since built the
code-index and verification pieces — `vikram-codeindex`, `verification.rs`; mid-turn
execution-resume is still the open differentiator.)* Plus: a Rust-native, deny-first, journal-first
runtime with replaceable seams and a unified Ticket System spanning Hive/bots/sub-agents.

## Long-term vision

A world-class autonomous coding agent: Rust-native, secure by construction (OS sandbox +
taint tracking + non-widenable floor), durable (mid-turn resume, scheduling, triggers),
intelligent (code indexing, semantic memory, verification loop), interoperable (ACP/A2A),
self-improving (unified learning + evolution), and collaborative (Hive teams + sub-agents
+ independent bots under one Ticket System).