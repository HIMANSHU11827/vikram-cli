# VIKRAM Complete Repair — Engineering Progress

Status vocabulary (per mission spec §9): NOT INVESTIGATED · VERIFIED EXISTING · BROKEN ·
PARTIALLY IMPLEMENTED · IN PROGRESS · IMPLEMENTED · TESTED · BLOCKED

Every entry below was checked against current source, not against `docs/audit/*`.
Measured 2026-11-03. Workspace: 10 crates, ~64k LOC Rust, `cargo build --workspace` was
green (0 warnings) at the start of this session.

## 1. The existing audit is substantially wrong

`docs/audit/` (commit 5662c10) lists six P0 items. Verification against source refutes
most of them — they describe defects that were already fixed in commits *earlier* than the
audit itself.

| Audit P0 claim | Verdict | Evidence |
|---|---|---|
| W-001 kernel hardcodes `Trust::Model` | **REFUTED** | `Effect::RequestTool` carries `originating_trust` ([effects.rs:25](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-kernel/src/effects.rs#L25)), forwarded at :54-72; evaluated at [machine.rs:218-221](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-kernel/src/machine.rs#L218); regression test exists at effects.rs:135-147 |
| W-002 `mcp serve` is a stub | **REFUTED** | Real JSON-RPC stdio loop at [server.rs:522-562](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-mcp/src/server.rs#L522); `initialize`/`tools/list`/`tools/call` at :212-313; wired to real local tools at [main.rs:1542-1594](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-cli/src/main.rs#L1542) |
| W-003 MCP runtime has null stdio | **REFUTED** | stdin/stdout piped and used at [runtime.rs(mcp):66-140](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-mcp/src/runtime.rs#L66); only stderr is `Stdio::null()` at [mcp.rs:457](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/mcp.rs#L457), which is correct |
| W-125 no OS sandbox | **PARTIALLY IMPLEMENTED** | bwrap + Seatbelt + documented pass-through in [sandbox.rs](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/sandbox.rs) (305 LOC); Windows/Termux have no isolation |
| W-129 no verification loop | **REFUTED** | `verify()` with repair callback + attempt budget + workspace isolation in [verification.rs](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/verification.rs) (466 LOC); exposed as the `verify` tool, wired at [tools.rs:4337-4340](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/tools.rs#L4337) |
| W-124 no durable mid-turn resume | **CONFIRMED MISSING** | checkpoint/resume appear only in `types.rs`/`workspace.rs`/`runtime.rs` for file-change snapshots; no execution-state resume across a crash mid-turn |

Also refuted as "missing": git integration (`Git` tool, status/diff at tools.rs:982-988),
test execution (`TestRun`), build validation (`BuildRun`), code review (`CodeReview`),
LSP (5 tools). The audit docs contradict even each other — `EXECUTIVE_SUMMARY.md:28` says
"no OS sandbox" while `FEATURE_AUDIT.md:78` says it is partially built.

**Action required:** rewrite `docs/audit/*` from source, or mark it superseded. Until then
it must not drive prioritisation.

## 2. Work completed this session

### 2.1 `connect_all` reported success when every MCP server failed — BROKEN → IMPLEMENTED

- Root cause: [mcp.rs:287](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/mcp.rs#L287)
  returned `(self, Ok(()))` unconditionally; failures were only recorded per-server, so the
  `Err` branch of the caller at [runtime.rs:607](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/runtime.rs#L607)
  was dead code.
- Change: `connect_all` collects per-server failures and returns
  `Err(RuntimeError::Mcp("N of M configured MCP servers failed to connect: <name: err>; …"))`
  when any server fails. Partial success still keeps the servers that connected (one failing
  server must not block the rest), but the operator is told.
- New error variant `RuntimeError::Mcp(String)` in error.rs.
- Tests added: `connect_all_reports_servers_that_never_answered`,
  `connect_all_with_nothing_configured_is_a_clean_success` (mcp.rs tests module).
- **TESTED** — both pass; full suite 921 passed / 0 failed (§3).

### 2.2 Memory store turned DB errors into "nothing remembered" — BROKEN → IMPLEMENTED

- Root cause: `MemoryStore::{get, search, list, list_all, count, total_count}` swallowed every
  rusqlite error (`.ok().flatten()`, `Err(_) => return Vec::new()`, `unwrap_or(0)`). A corrupt or
  missing table was indistinguishable from genuinely empty memory. The `memory.search` tool then
  returned `ok: true` with "no memory matches" ([tools.rs:1185-1190](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/tools.rs#L1185)),
  which is exactly the failure-as-success pattern the mission forbids.
- Change: all six methods now return `Result<…>` and propagate `RuntimeError::Store`; row-level
  errors inside `query_map` iteration propagate too (previously `filter_map(|r| r.ok())` dropped
  bad rows silently). Callers updated: `Runtime::{memory_search, memory_list, memory_list_all}`,
  `MemorySearch`/`MemoryList` tools, the learning auto-apply path, and the CLI memory picker
  ([terminal.rs:694-706, 1602-1620](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-cli/src/terminal.rs#L694)),
  which now shows "memory could not be read: …" instead of an empty list.
- The learning loop no longer records `outcome: "applied"` for a save that failed; it records
  `outcome: "failed"` with the error, and skips dedup when the existing-memory read failed
  rather than deduping against an empty list.
- Tests added: `a_read_failure_is_reported_instead_of_looking_like_an_empty_memory`,
  `a_write_failure_is_reported` (memory.rs tests module), plus 7 existing tests updated for the
  new signatures.
- **TESTED** — all 9 `memory::tests` pass, including the two new failure tests; full suite green.

### 2.3 Store cascades committed as success after a failed cleanup — BROKEN → TESTED

- Root cause: `delete_session` wrapped the `teams` and `hive_sessions` clear-outs in `.ok()`
  and `delete_user` wrapped the `active_identity` clear-out in `.ok()`, inside transactions that
  then committed. A failed cleanup left dangling `session_id` / `user_id` references and still
  returned `Ok(())`. All three tables are created unconditionally by `migrate()`
  ([store/lib.rs:2378, 2447, 2454](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-store/src/lib.rs#L2378)),
  so there was no "table may not exist" justification for swallowing.
- Change: three sites now propagate with `?` (`StoreError: From<rusqlite::Error>` already
  existed), so a failed cascade rolls the transaction back instead of half-deleting.
- Tests added: `a_broken_session_cascade_is_reported_instead_of_committed_as_success` (also
  asserts the session survives, i.e. the transaction really rolled back) and
  `a_broken_identity_clear_is_reported_when_a_user_is_deleted`.
- **TESTED** — `cargo test -p vikram-store`: 27 passed / 0 failed.

### 2.4 A secrets home that could not be made private was reported as ready — BROKEN → TESTED

- Root cause: `ensure_home()` did `let _ = set_permissions(&home, 0o700)`
  ([config.rs:407](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/config.rs#L407)).
  That directory holds `.env` with provider API keys, so a failed chmod meant Vikram kept writing
  secrets into a possibly world-readable directory and told every caller (`runtime.rs:427`,
  `rules.rs:113`, `doctor.rs:54`) that the home was ready.
- Change: the chmod failure now returns `RuntimeError::Config` naming the path and the reason.
  Verified safe first: `~/.vikram` is already `drwx------` on this machine, so tightening to an
  error cannot break an existing install, and all four callers already handle `Err`.
- Tests added: `the_secrets_home_is_made_private_instead_of_left_as_created` (creates the home at
  0o755 and asserts it ends at 0o700 — enforcement, not intention) and
  `a_home_that_cannot_be_created_is_reported_instead_of_reported_ready`.
- Not covered by a test: the chmod-fails branch itself could not be forced without another user or
  `CAP_LINUX_IMMUTABLE`; it is compile-enforced by `?` rather than executed. Stated as a limitation
  rather than claimed as proven.

## 3. Verification method, and the live coordination problem

`cargo build --workspace` was green with 0 warnings at session start.

The second writer is active in this tree and has twice left the **core crate uncompilable** during
this session — first `context_engine.rs` (3 production errors, with the error line numbers moving
between two consecutive builds), then `vikram-evolution` (8 errors). `vikram-runtime` depends on
`vikram-evolution`, so in the main checkout `cargo test -p vikram-runtime` could not run at all.

Verification was therefore done in an isolated detached worktree at the last green commit
(`.git/worktrees` → `../VIKRAM-VERIFY-WT`), containing HEAD plus exactly my 8 changed files and
none of their untracked work:

```
cargo test --workspace   →  passed: 921   failed: 0
rustfmt --check           →  all 5 edited files clean
```

Mission spec §10 forbids two agents editing the same critical files without coordination, so their
in-flight files (`context_engine.rs`, `compaction.rs`, `vikram-evolution/src/{autonomy,mapek,
self_*}.rs`, `docs/evolution-architecture.md`) were left untouched — no patch, no revert, no
`cargo fmt --all` over them.

Statuses above are `TESTED` against that isolated run. They must be re-confirmed against the merged
tree once the main checkout compiles; `BLOCKED` is no longer accurate for verification, only for
in-tree confirmation.

## 4. Real remaining P0 (re-derived from source)

1. Durable mid-turn resume — genuinely absent; journal + effect idempotency give a head start.
2. OS isolation on Windows and Termux — pass-through today; must be reported as a limitation,
   never silently unrestricted (spec §C).
3. Silent error swallowing — 237 discard sites across runtime/store/evolution; next worst after
   the two fixed above: `context.rs:332/363` (cache dir + cache write failures),
   `config.rs:407` (`set_permissions` on the secrets home — security-relevant),
   `evolution/skills.rs:81` (backup write fails before a skill overwrite),
   `store/lib.rs:612/617/1212` (cascade cleanup inside a committing transaction).
4. `Trust::Model` defaults in tool `permission()` builders — correct today only because every
   request is re-stamped at [turn.rs:535-539](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/turn.rs#L535);
   any new evaluation path that skips it silently inherits Model trust. Hardening, not a live bug.
5. MCP protocol is hand-rolled (no `rmcp` dependency anywhere) — `initialize`/`tools`/`resources`/
   `prompts` exist but every conformance claim needs testing against a real MCP client.

## 5. Adjudicated as already VERIFIED EXISTING — do not rebuild

Kernel trust propagation · MCP server + stdio transport · verification loop with repair budget ·
git/test/build/code-review/LSP tools · sandbox detection (Linux/macOS) · context loader ·
unified Ticket System · skills system · deny-first floor with posture modes · journal replay.
