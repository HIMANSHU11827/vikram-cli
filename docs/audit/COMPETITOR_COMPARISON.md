# Competitor Comparison

## Competitors investigated

Hermes Agent, OpenClaw, OpenAI Codex, OpenHands, Claude Code, Cursor, OpenCode,
Aider, SWE-agent, LangGraph, CrewAI, AutoGen, plus local reference repos (codex,
openhands2, inspect_ai, gitui).

## Capability matrix (Vikram vs leaders)

| Capability | Vikram | Codex | Goose | OpenCode | Hermes | OpenClaw | OpenHands | Aider |
|---|---|---|---|---|---|---|---|---|
| Language | Rust | Rust | Rust | TS | Python | TS+Rust | Python/TS | Python |
| Execution model | Turn loop + kernel machine | 3 nested loops | Operation pipeline | Durable semantics | Gateway + cron | Gateway process | ACP | Edit blocks |
| Sandbox | Policy only (now OS-level planned) | Seatbelt/bwrap/Landlock | MCP | Shell | Terminal backend | Docker/Podman/SSH | Container | None |
| Memory | SQLite KV | Rollout JSONL + SQLite index | MCP | File-diff | FTS5 + Honcho + skills | SQLite + dreaming | Container | Git |
| Providers | OpenAI-compatible | OpenAI Responses | 53 declarative | OpenAI-compat | 6+ | 4-layer | Multiple | Multiple |
| Scheduling | None | SleepHandler + wait_agent | None | Schedule tool | cron (60s) | cron/interval/on-exit/stream | None | None |
| Multi-agent | Agents/Bots/Hives | Subagents (depth-limited) | MCP | None | Hosted-room discussions | Subagents + continuation | Subagents + A2A | None |
| Security | Deny-first + floor + posture | OS sandbox + execpolicy DSL | Approval floors | Permission triple + Unattended | Approval floors + guardian LLM | Tool policy ≠ sandbox ≠ elevated | Tool policy | None |
| Extensibility | Skills + plugins + MCP | MCP + hooks + plugins | Extensions | MCP | Skills + hooks + MCP | Skills + hooks + MCP | MCP | None |
| Code indexing | None (agentic grep) | None (agentic grep) | None | grep/glob/read | None | None | None | Tree-sitter repo-map (dormant) |
| ACP | None | None | None | None | Both directions | None | Client+server | None |
| Self-improvement | Learning (JSON) + Evolution (SQLite, dormant) | None | None | None | background_review + skill creation | Dreaming consolidation | None | None |

## Where Vikram does better

- **Rust-native** with clean crate boundaries and replaceable seams (provider, policy,
  journal, tools) behind typed contracts.
- **Deny-first permission model** with a non-widenable mandatory floor and 7 postures.
- **Entity model** with identity/title/role/specialization separation; titles are
  metadata, never permissions.
- **Journal-based durable state** with idempotent effect replay.
- **Unified Ticket System** spanning Hive/bots/sub-agents (new).
- **Context loader** with mtime invalidation, token budgeting, dedup.
- **Skills system** breadth (discovery, loading, activation, trust, bundles, history,
  rollback).

## Where Vikram does worse

- **No OS sandbox** (policy-only).
- **No code indexing** (Aider's repo-map dormant; Codex/Goose use agentic grep).
- **No ACP** (OpenHands/Hermes).
- **No durable mid-turn resume** (all punt, but Vikram's journal gives a head start).
- **No scheduling/triggers** (OpenClaw/Hermes).
- **No local model support** (Goose has llama.cpp/MLX).
- **No verification loop** (edit→build→test→repair) — biggest gap.
- **Evolution dormant**, two learning systems.
- **MCP serve stub**, null stdio.
- **Silent error swallowing** pervasive.
- **No integration/E2E/property/fuzz/concurrency/security tests.**

## Which competitor is best in each major area

- Sandbox: **Codex** (OS-level).
- Config layering: **Codex** (10-layer, non-widenable requirements tier).
- Provider model: **Goose** (declarative JSON, capability flags).
- Durable semantics: **OpenCode** (inbox/steer, compaction-as-data, schedule tool).
- Policy engine: **Gemini CLI** (tiered TOML, BeforeToolSelection).
- Hook catalogue: **Gemini CLI** (loop detection).
- ACP: **OpenHands / Hermes**.
- Container sandbox: **OpenHands**.
- Multi-agent: **OpenHands** (subagents + continuation).
- Memory: **Hermes / OpenClaw** (FTS5, dreaming).
- Scheduling: **OpenClaw / Hermes**.
- Self-improvement: **Hermes**.
- Security model: **Codex** (execpolicy DSL).
- Code understanding: **Aider** (tree-sitter, dormant).
- Extensibility: **Claude Code** (plugin-first, dogfood rule).

## Missing features (Vikram vs competitors)

Code indexing, ACP client+server, durable mid-turn resume, OS sandbox, scheduling/
proactive triggers, local model inference, verification loop, A2A, desktop/phone
notifications, git integration (status/diff/commit/worktree), test discovery/execution,
build validation, checkpoint/undo, code review agent, regression detection, memory
consolidation, semantic memory, hybrid search, knowledge graph, agent-to-agent messaging,
independent review agent, deadlock detection, agent performance tracking, agent failure
recovery, monitor mode, fire-claim TTL leases.

## Architectural notes

- Leaders are converging on **harnesses, not engines**; ACP is the emerging orchestration
  rail.
- **Nobody indexes code** — zero embedding/vector retrieval across competitors; agentic
  grep won on capability.
- **Goose's Operation/Effect kernel** is the best Rust template.
- **OpenCode's durable semantics** are the reference.
- **Codex's security kernel** is the reference.
- **Claude Code's dogfood rule** (first-party features as plugins on same hook API) is the
  architectural lesson.