# Runtime — Host Integration

`vikram-runtime` supplies the turn loop (the kernel is no longer used; the turn loop is the
execution path) and everything else: where sessions live, which model answers, which tools
exist, MCP, plugins, skills, memory, context, learning, hive, tickets, workspace, sandbox.

## Core

- `Runtime` / `RuntimeHandle` / `RuntimeInner` (process-wide state).
- `SessionRuntime`: live session (messages, turn state, tokens, cancel, queue, agent, project, status).
- `AgentManager` / `ManagedAgent` / `TaskManager`.
- `TeamManager` (JSON-backed) and `HiveManager` (SQLite-backed group chat).
- `TicketManager` (unified ticket engine).
- `WorkspaceManager`, `ContextLoader`, `MemoryStore`, `LearningManager`, `McpRegistry`,
  `PluginRegistry`, `SkillRegistry`, `RuleBook`.

## Boot

`Runtime::start(config)` opens store + memory, loads agents, provider, MCP, plugins,
skills, evolution, workspace; rehydrates sessions from journal; bootstraps default
user/profile; creates a default session if none.

## Handle API

Observations and explicit controls only. Sessions, agents, teams, hive, tickets, memory,
skills, MCP visibility, modes, models, approvals, questions, workspace, evolution.

## Turn loop

`turn::run_turn` / `spawn_turn`: inject skill catalog + identity + preamble, stream
provider, commit text/thinking, run tools under policy gate, append results, repeat;
record usage, emit TurnFinished, dequeue next.

## Sandbox

OS-level isolation (bubblewrap on Linux, sandbox-exec on macOS, pass-through elsewhere)
applied to spawned commands. Policy still gates. Configured via `security.sandbox`.

## Files

- `crates/vikram-runtime/src/runtime.rs`, `session.rs`, `turn.rs`, `agents.rs`,
  `teams.rs`, `hive_chat.rs`, `tickets.rs`, `memory.rs`, `context.rs`,
  `learning.rs`, `workspace.rs`, `sandbox.rs`, `tools.rs`, `mcp.rs`, `plugins.rs`,
  `skills.rs`, `rules.rs`, `provider.rs`, `openai.rs`, `bus.rs`, `config.rs`.

## Tests

~394 unit tests + `tests/tickets.rs` (5 workflow tests).