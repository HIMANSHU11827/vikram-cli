# Vikram A-to-Z Architecture Upgrade — Plan

## Goal
Make Vikram a top-tier agent framework across 5 subsystems: Sandbox, Memory/Sessions, Code Indexing, ACP, Remote Terminals — competing with and exceeding Hermes/OpenClaw/OpenHands/OpenCode/Codex/Claude/Aider.

## Guiding principles
- Preserve existing functionality; no duplicate systems; no weakened security.
- Rust-native, modular crates behind typed contracts.
- No fake features; no silent error swallowing; honest reporting of partial work.

## Architecture decisions
1. **Sandbox**: Promote `sandbox.rs` into a dedicated `vikram-sandbox` crate. Add Landlock (Linux), resource limits (rlimit), isolation verification, audit events, and a `SandboxMode` (NoSandbox/Selective/Full) with session-level selection. Keep policy independent.
2. **Memory/Sessions**: Add FTS5 virtual table + `session_search` tool (Hermes-shaped). Add memory consolidation (dedup/contradiction/provenance) helpers. Add memory-provider seam for Honcho-style plugins. Keep existing KV store as the durable base.
3. **Code indexing**: New `vikram-codeindex` crate using tree-sitter. Symbol extraction for Rust/Python/TS/JS/Go, persisted SQLite index, incremental update via mtime, repo-map with graph ranking, queryable tool family. Optional/degradable.
4. **ACP**: New `vikram-acp` crate. JSON-RPC 2.0 types, client (spawn + stdio + initialize/session/prompt/stream/cancel), server (stdio). Map ACP sessions to Vikram sessions/tickets.
5. **Remote terminals**: `TerminalBackend` trait + local/docker/ssh backends with lifecycle (create/exec/read/write/resize/interrupt/close/reconnect). Plugin registry (Hermes-shaped). Env scrubbing + secret isolation.

## Integration
All wired through existing seams: ToolRegistry extensions, Runtime boot, ToolContext, config, store migrations. No second orchestration framework.

## Testing
Unit tests per crate + integration tests across crate boundaries + escape-attempt tests for sandbox + FTS search tests + ACP handshake tests + terminal lifecycle tests.

## Staging
Stage 1: sandbox crate + Landlock + mode selection.
Stage 2: FTS5 session search + memory consolidation.
Stage 3: codeindex crate (tree-sitter).
Stage 4: ACP crate.
Stage 5: TerminalBackend trait + docker/ssh.
Stage 6: Integration + docs + report.

## Honest scope note
Each subsystem is large; this plan delivers real, compilable, tested foundations wired into the workspace, with partial features clearly marked. Full production maturity requires follow-up work beyond a single session.