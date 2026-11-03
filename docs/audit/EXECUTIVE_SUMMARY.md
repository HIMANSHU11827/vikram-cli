# Executive Summary — Vikram AI Agent

## Where Vikram stands today

VIKRAM is a **terminal AI runtime in Rust** (10 crates, ~15k+ LOC) with a coherent
multi-user/multi-entity architecture, deny-first permissions, a single execution kernel,
SQLite durable state, and an OpenAI-compatible provider. It is **pre-release (0.1.0)**.

It is **well-architected at the core** (turn loop, permission gating, kernel machine,
entity model, journal-based state) but **incomplete in integration, reliability, security
enforcement and capabilities**.

## Strongest capabilities

1. Clean crate boundaries with a clear dependency graph (no cycles).
2. Deny-first security model with a non-widenable mandatory floor and 7 posture modes.
3. Kernel machine design (Goose-inspired operation pipeline) with journaling + idempotency.
4. Rich entity model (identity / title / role / specialization / description / status).
5. Journal-based durable state with crash-safe effect replay.
6. Context loader (mtime invalidation, token budgeting, dedup).
7. Comprehensive skills system (discovery, loading, activation, trust, bundles, history).
8. Rich TUI (widgets, responsive layout, interactive input).
9. Unified Ticket System (Hive teams, bots, sub-agents) — implemented post-audit.
10. Cross-platform scaffolding (Windows/Termux/Docker/Kubernetes).

## Where Vikram is weaker than competitors

- **No OS-level sandbox** (only policy; Codex has Seatbelt/bwrap/Landlock).
- **No taint tracking** on observations (Gemini has a taint model).
- **Silent error swallowing** is pervasive (memory, store, config, context, learning).
- **Evolution crate is dormant** — constructed at boot, never called.
- **Duplicate abstractions** (hive.rs vs teams.rs vs hive_chat.rs; two learning systems).
- **MCP serve is a stub**; MCP runtime uses null stdio.
- **No code indexing**, no ACP, no durable mid-turn resume, no scheduling, no local models,
  no verification loop, no A2A.
- **No OS-level isolation**, no secret scanning, no at-rest encryption.
- **No integration/E2E/property/fuzz/concurrency/security tests.**

## Important features completely missing

Code indexing (tree-sitter), ACP (client+server), durable mid-turn resume, OS sandbox,
scheduling/proactive triggers, local model inference (llama.cpp/MLX), verification loop
(edit→build→test→repair), A2A, desktop/phone notifications, git integration (status/diff/
commit/worktree), test discovery/execution, build validation, checkpoint/undo, code review
agent, regression detection, memory consolidation, semantic memory, hybrid search,
knowledge graph, agent-to-agent messaging, independent review agent, deadlock detection,
agent performance tracking, agent failure recovery.

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

1. Fix kernel trust propagation (Effect carries originating trust).
2. Implement MCP serve (real server, fix null stdio).
3. Fix silent error swallowing (memory, store, config, context, learning).
4. Implement OS-level sandbox (Seatbelt/bwrap/Landlock).
5. Verification loop (edit→build→test→repair).
6. Durable mid-turn resume.

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
are Vikram's differentiation opportunities. Plus: a Rust-native, deny-first, journal-first
runtime with replaceable seams and a unified Ticket System spanning Hive/bots/sub-agents.

## Long-term vision

A world-class autonomous coding agent: Rust-native, secure by construction (OS sandbox +
taint tracking + non-widenable floor), durable (mid-turn resume, scheduling, triggers),
intelligent (code indexing, semantic memory, verification loop), interoperable (ACP/A2A),
self-improving (unified learning + evolution), and collaborative (Hive teams + sub-agents
+ independent bots under one Ticket System).