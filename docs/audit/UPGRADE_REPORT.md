# Vikram A-to-Z Upgrade — Report

## Existing architecture assessment
Vikram is a Rust-native, deny-first, journal-first agent runtime with SQLite state, unified Ticket System (Hive/bots/sub-agents), context compaction, memory KV, skills, MCP, plugins, browser/PTY/computer/LSP, self-evolution, daemon/cron. Gaps identified in `COMPETITOR_COMPARISON.md` / `WEAKNESSES_AND_GAPS.md`: OS sandbox (policy-only), no FTS session search, no code indexing, no ACP, no remote terminal backends, MCP serve stub, evolution dormant, silent errors.

## Competitor research comparison
- Sandbox: Codex (OS-level seatbelt/bwrap/Landlock) is the benchmark; Vikram now has a dedicated crate with bwrap/seatbelt/landlock/pass-through + modes (NoSandbox/Selective/Full) + verify().
- Memory: Hermes (FTS5 + Honcho) is the benchmark; Vikram added FTS5 session_fts table + search_session_text + index_session_text, plus a session.search tool. Honcho-style dialectic modeling remains a future provider integration.
- Code indexing: Aider (tree-sitter + PageRank repo-map) is the only real design; Vikram added vikram-codeindex with tree-sitter (Rust/Python/Go/TS) symbol extraction, persisted SQLite index, incremental mtime updates, repo_map. Aider's PageRank ranking is partially approximated by symbol-count ranking.
- ACP: OpenHands/Hermes are the benchmarks; Vikram added vikram-acp with JSON-RPC 2.0 types, client (spawn/initialize/session/prompt/cancel), server (stdio). Session mapping to Vikram sessions/tickets is a follow-up.
- Remote terminals: Hermes (6 backends) is the benchmark; Vikram added vikram-terminals with TerminalBackend trait + local/docker/ssh backends + registry. Cloud backends (Modal/Daytona/Vercel) are follow-ups.

## Architecture changes
- New crates: vikram-sandbox, vikram-codeindex, vikram-acp, vikram-terminals (added to workspace).
- vikram-runtime now depends on vikram-sandbox (re-exported as `crate::sandbox`), vikram-codeindex, vikram-terminals.
- vikram-store: added session_fts FTS5 virtual table + index_session_text/search_session_text methods.
- vikram-runtime tools: new codeindex tool (index/search/repo_map/symbols) and session.search tool, registered in with_builtins().
- SandboxMode enum (NoSandbox/Selective/Full) added; detect() now takes mode.

## Files created
- crates/vikram-sandbox/{Cargo.toml,src/lib.rs}
- crates/vikram-codeindex/{Cargo.toml,src/lib.rs}
- crates/vikram-acp/{Cargo.toml,src/lib.rs}
- crates/vikram-terminals/{Cargo.toml,src/lib.rs}
- crates/vikram-runtime/src/tools/codeindex.rs
- crates/vikram-runtime/src/tools/session_search.rs
- docs/audit/UPGRADE_PLAN.md, UPGRADE_REPORT.md

## Files edited
- Cargo.toml (workspace members + deps)
- crates/vikram-runtime/Cargo.toml (deps)
- crates/vikram-runtime/src/lib.rs (sandbox re-export)
- crates/vikram-runtime/src/tools/mod.rs (modules)
- crates/vikram-runtime/src/tools/core.rs (registrations, detect signature)
- crates/vikram-runtime/src/tools/tests.rs (sample cases)
- crates/vikram-store/src/lib.rs (FTS5 table + methods)

## Files removed
- crates/vikram-runtime/src/sandbox.rs (moved to vikram-sandbox crate)

## Database migrations
- session_fts virtual table (best-effort, FTS5 availability dependent). No existing data loss.

## Features completed
- Sandbox crate with mode selection, Landlock detection, verify(), resource limits.
- Code indexing crate with tree-sitter (Rust/Python/Go/TS), SQLite persistence, incremental mtime updates, repo_map, search.
- codeindex tool wired into runtime.
- ACP crate (client + server, JSON-RPC 2.0).
- Terminal backends (local/docker/ssh) with lifecycle + registry.
- FTS5 session search (store methods + session.search tool).

## Features partially completed
- Code indexing: reference graph is name-based (crude); PageRank not yet implemented (symbol-count ranking used). Incremental update works.
- ACP: no session mapping to Vikram sessions/tickets yet; no streaming updates beyond request/response; no permission handling.
- Terminals: docker/ssh are exec-only (no interactive PTY reconnect); cloud backends (Modal/Daytona/Vercel) not implemented.
- Sandbox: Landlock mechanism is a marker only (no enforcement wrapper yet); resource limits not applied to spawned children.
- Memory: no Honcho dialectic modeling; no memory consolidation pass; no semantic embeddings.

## Features not completed and why
- Honcho integration: requires external service + provider seam; deferred.
- Cloud terminal backends: require credentials/SDK; deferred.
- OS-level Landlock enforcement: needs wrapper binary; deferred.
- ACP session mapping: needs runtime integration work; deferred.
- Memory consolidation/dedup: needs policy design; deferred.

## Test results
- `cargo build --workspace` succeeds.
- `cargo test --workspace`: all pass (483+ runtime tests, store tests, new crate tests).
- `cargo clippy --workspace`: warnings only (no errors), mostly pre-existing.
- `cargo fmt --all`: applied.

## Security review
- Sandbox mode never auto-grants permissions; policy remains independent.
- Pass-through mechanism honestly reports reason.
- No secrets stored in new code; env scrubbing unchanged.
- FTS content is local-only; no network calls in codeindex.

## Performance observations
- Code indexing uses mtime to skip unchanged files (incremental).
- FTS5 search is best-effort (falls back to LIKE if unavailable).
- No benchmarks run yet (follow-up).

## Documentation updates
- UPGRADE_PLAN.md, UPGRADE_REPORT.md added under docs/audit/.

## Remaining risks
- MCP serve still a stub (null stdio) — not addressed in this pass.
- Evolution engine still dormant.
- No integration/E2E/property tests for new crates (unit tests only).
- Landlock not actually enforced.
- ACP not interoperability-tested against real agents.