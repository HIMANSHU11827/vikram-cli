# Research Sources

## Local reference repositories (in workspace)

- `codex/` — OpenAI Codex (Rust). Architecture, sandbox (Seatbelt/bwrap/Landlock),
  execpolicy DSL, apply_patch, rollout.
- `openhands2/` — OpenHands (Python/TS). ACP, container sandbox, A2A.
- `inspect_ai/` — Inspect AI. Evaluation harness.
- `gitui/` — GitUI (Rust). TUI patterns.

## Competitor projects researched (from docs/research/)

- OpenClaw (TS + Rust) — local-first gateway, cron/interval/on-exit/stream triggers,
  dreaming consolidation.
- Hermes Agent (Python) — self-improving, FTS5 + Honcho + skills, background_review,
  fire-claim TTL leases, monitor mode.
- Letta / letta-code (TS) — stateful agents, memory blocks, sleep-time compute.
- OpenAI Codex (Rust) — shell-centric, strong sandbox, rollout reconstruction.
- Goose (Rust) — operation pipeline, 53 declarative providers, MCP.
- OpenCode (TS) — durable semantics (inbox/steer, compaction-as-data, schedule tool,
  git-tree snapshots).
- Gemini CLI (TS) — policy engine, hook catalogue, taint model.
- OpenHands (Python/TS) — ACP, container sandbox, A2A.
- Aider (Python) — tree-sitter repo-map (dormant since 2026-05).
- Claude Code (proprietary) — plugin-first, mods system.
- SWE-agent, LangGraph, CrewAI, AutoGen — multi-agent frameworks.

## Internal research docs

- `docs/research/competitors/autonomous-agents.md`
- `docs/research/competitors/coding-agents.md`
- `docs/research/architecture/modularity-and-upgrades.md`
- `docs/research/implementations/coding-agent-internals.md`
- `docs/research/security/agent-security-and-permissions.md`
- `docs/research/benchmarks/agent-benchmark-landscape.md`
- `docs/research/skills/*` (claude-code, opencode, hermes, codex, agent-skills-standard)

## VIKRAM docs

- `HARNESS.md` — architecture overview.
- `docs/concepts.md` — core concepts.
- `docs/tickets.md` — unified Ticket System.
- `docs/permissions.md`, `docs/kernel.md`, `docs/store.md`, `docs/runtime.md`.
- `docs/memory.md`, `docs/context.md`, `docs/tools.md`, `docs/plugins.md`.
- `docs/agents.md`, `docs/tui.md`, `docs/cli.md`, `docs/security.md`.
- `docs/session-workspace.md`, `docs/evolution.md`, `docs/termux.md`, `docs/docker.md`,
  `docs/kubernetes.md`, `docs/roadmap.md`.
- `docs/skills/*`, `docs/mcp/*`, `docs/development/*`, `docs/decisions/*`.

## Primary external references

- Rust: https://www.rust-lang.org/ (edition 2024, rust-version 1.86)
- rusqlite: https://github.com/rusqlite/rusqlite
- ratatui: https://ratatui.rs
- crossterm: https://github.com/crossterm-rs/crossterm
- clap: https://docs.rs/clap
- tokio: https://tokio.rs
- OpenAI API: https://platform.openai.com/docs
- MCP spec: https://modelcontextprotocol.io
- ACP: https://agentclientprotocol.com (research)

## Notes

Web research was not available during this audit pass; competitor details are drawn
from the existing research docs and local reference repositories. Verify against
primary sources before acting on specific claims.