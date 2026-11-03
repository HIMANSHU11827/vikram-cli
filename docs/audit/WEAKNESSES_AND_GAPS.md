# Weaknesses & Gaps

## P0 — Critical

- W-001 Kernel trust hardcoded to Model → undermines provenance/floor.
- W-002 MCP serve is a stub (prints success, no server).
- W-003 MCP runtime uses null stdio → cannot talk to children.
- W-004 Silent error swallowing in memory store.
- W-005 connect_all always Ok even on failure.
- W-006 Evolution engine silent fallback to in-memory.
- W-125 No OS sandbox (policy-only).
- W-129 No verification loop (edit→build→test→repair).
- W-124 No durable mid-turn resume.

## P1 — High

- Duplicate abstractions: hive.rs vs teams.rs vs hive_chat.rs; two learning systems.
- RuntimeInner god struct (20+ concerns).
- Evolution dormant; not integrated.
- Store: sessions disconnected from users; create_project stale id; TeamSnapshot.id from name.
- Silent errors: store replay, learning, config, context.
- Poisoned mutex panics (`.expect()`).
- No provider rebuild path; no cancellation for spawn_blocking; ToolContext only workspace_root.
- Skills loaded metric always 0; context_used heuristic (len/4).
- Learning policies write-only (instruction/identity).
- Hive messages Trust::User for all; hive entities get no tools.
- Kernel grants not self-enforcing; approval tickets unaddressable; deferred leaves awaiting unset.
- Policy floor vetoes (deny) vs docs ("human"); floor only vetoes Write; normalize_request only Read/Write.
- MCP visibility inconsistent persistence; auth_required not enforced; approved_tools empty = all.
- TUI UTF-8 byte slicing panic risk; activity cap index off-by-one.
- CLI doctor terminal_size from env; chat forces Ask→Unattended.
- Evolution: silent error drops, self-referential supersession, search after LIMIT, no path guard, no async runner, no migration versioning, stringly typed, no store trait, no approval framework, no candidate validation.

## P2 — Medium

- TUI Outcome::Info unused; layout thresholds hardcoded; theme hardcoded; session_by_id asymmetry.
- CLI runtime() `.expect()`; skills history global load; MCP List empty arm; skills validate global manager.
- MCP base_dir dead; ManagedServerConfig never used.
- Store TeamRow missing updated_ms; delete cascades broken; user/profile linkage missing.
- Policy Rule::actions empty = wildcard; ResourcePattern::as_str lossy; has_allow ignores floor.
- Kernel CancellationToken SeqCst; PendingCall no originating trust.
- Contracts journal insertion-order dependency; NewRecord ts_ms default 0; no error chaining; macro doc-loss.
- TokenMeter record_reported indistinguishable.
- Evolution: unused deps/variants, public mutable fields, no Dashboard store accessor, ReflectionEngine constructs own subsystems, HiveLearning partial sanitization, EvaluationResult no PK, create_skill versioning inconsistency, skill versions split fs/DB, reject() doesn't persist reason, rollback() doesn't restore content.
- Skills: ActivationPolicy doc/code mismatch, TrustPolicy silently defaults, History staleness, builtin root special-cased, Manager state_dir divergence, install no timeout/--branch SHA, loader hardcoded script refusal.
- TUI terminal restore ignores results; MCP runtime cleanup ignores kill result; skills remove_path ignores errors; evolution SkillEvolution ignores write errors; OptimizationEngine no-ops.

## P3 — Low

- Hardcoded values scattered (context 128k, bus 4096, checkpoint 5MB, tool output 20k, pty 256k, search 10, etc.).
- Context loader scans workspace on every metrics call.
- Skills budgets hardcoded.
- TUI collapse thresholds hardcoded.
- CLI column widths hardcoded.
- Dead code (hive.rs, assistant_with_calls, mcp_visibility, skills merge/scan_dir, learning policies, HiveManager::update_state, ContextLoader with_budget/invalidate_all, TUI press, MCP base_dir/ManagedServerConfig, store session_count/list_all_sessions/search_sessions/delete_file_changes_for_session, Skills TrustPolicy::load_in).

## Security gaps

- No OS sandbox (now partially: bubblewrap/seatbelt; Windows/Termux pass-through).
- No taint tracking.
- Floor bypassable via Trust hardcoding.
- No secret scanning.
- No at-rest encryption.
- No dependency vulnerability scanning.
- MCP serve stub; MCP null stdio.
- Compound command splitting: floor doesn't check all segments.
- No output validation before context entry.

## Performance gaps

- Hardcoded limits everywhere.
- Heuristic token counting (len/4).
- No actual profiling/optimization.
- No local model support (requires API).
- No offline mode.
- No resource limits enforcement (memory/CPU).

## Testing gaps

- No integration tests across crate boundaries (except skills, mcp, tickets, store).
- No E2E tests for full turn workflows.
- No property-based, fuzz, concurrency, failure-injection, performance, security, agent-task-benchmark tests.
- Tests validate execution, not behavior in many cases.

## Documentation gaps

- Docs may not match code (floor says "human" but vetoes).
- Macro doc-loss in ids.rs.
- ActivationPolicy doc/code mismatch.