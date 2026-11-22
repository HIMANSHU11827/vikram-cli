# Jarvis-class Upgrade Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use godmode:delegated-execution (user pre-chose
> parallel sub-agents) — dispatch one worker per track, disjoint crate ownership,
> main agent integrates.

**Goal:** Convert Vikram into a JARVIS-class CLI assistant — multi-agent orchestrator
core, finished voice loop, Jarvis console (arc-reactor glyph + team HUD), daemon
proactivity — per `docs/plans/2026-11-22-jarvis-upgrade-design.md`.

**Architecture:** Step 0 lands the uncommitted Phase D voice WIP on master, then four
parallel workers on disjoint crates (runtime+policy+store / voice+cli / tui / daemon),
then integration gate. No git worktrees: uncommitted WIP would be orphaned by them;
isolation is enforced by crate ownership.

**Tech Stack:** Rust workspace (16 crates), cargo fmt/clippy/test, Voxtype (external
ASR, already installed + VAD on), existing vikram-policy trust machine, vikram-store
FTS5 memory.

**Deviations from default planning:** (1) shared tree instead of worktrees (reason
above); (2) contracts + test commands instead of full code listings (workers read the
module first and own the TDD cycle — fabricating unseen code would be invention).

---

### Task 0: Land the uncommitted voice WIP [MAIN AGENT — GATE]

**Files:** 16 modified (vikram-tui/*, vikram-runtime/{runtime,session,types,compaction}.rs,
vikram-cli/{main,terminal}.rs, Cargo.toml/lock) + untracked crates/vikram-voice/ +
crates/vikram-cli/src/voice.rs.

**Step 1:** Collect Step 0 verification results (background shell
`sh_202acb3130016PmqK9HJyD10FK`, log `/tmp/opencode/vikram-step0.log`):
FMT_EXIT, CLIPPY_EXIT, TEST_EXIT.

**Step 2 (decision tree):**
- fmt diffs only → `cargo fmt --all`, re-check, proceed.
- clippy/test failures NOT in voice files → fix as separate minimal commits first.
- clippy/test failures IN voice files, trivial → fix, proceed.
- failures IN voice files, deep → commit the 16 non-voice modified files as
  `feat: jarvis daemon triggers and personality harness follow-up` (only if they
  pass independently), and hand the voice files to Task 2's worker as their
  starting state (still uncommitted, owned by that worker).

**Step 3:** Commit everything that is verified green as ONE commit:
`feat: land jarvis phase D voice wip (push-to-talk loop)` — voice crate, voice.rs,
cli/tui/runtime integration points.

**Verify:** `git status --porcelain` empty; `git log --oneline -1` shows the commit;
`cargo test --workspace` green (TEST_EXIT=0 re-confirmed if anything changed).

**NO WORKER DISPATCHES BEFORE THIS GATE IS GREEN.**

---

### Task 1: Orchestrator MVP — `spawn_worker` with typed briefs [WORKER A: harness]

**Owns:** `crates/vikram-runtime/**`, `crates/vikram-policy/**`, `crates/vikram-store/**`.
Forbidden: every other crate.

**Step 1 (read before writing):** the agent turn loop, tool registration pattern,
`permission()`/trust machine in vikram-policy, store put/get API. Follow existing
patterns exactly (pattern-matching skill).

**Step 2 — TDD, one behavior at a time:**

| # | Test name (in the crate's existing test location) | Contract |
|---|---|---|
| 1 | `spawn_worker_creates_child_with_brief` | `spawn_worker(brief)` tool, orchestrator-only; brief struct: `{objective, output_format, tool_allowlist, file_scope, boundary, model: Option, effort: Option}`; returns worker id |
| 2 | `worker_cannot_write_outside_file_scope` | child tool call outside `file_scope` → DENIED by the policy machine (not prompt), assert the denial event is audit-logged |
| 3 | `child_trust_is_monotonic_narrower` | child capability set ⊂ parent; a denied action cannot be re-obtained via a sibling/child spawn |
| 4 | `worker_results_handoff_via_store` | worker writes result artifact to vikram-store, returns reference id; orchestrator sees the reference, never the child transcript |
| 5 | `max_turns_marks_partial_and_resumable` | worker hitting `max_turns` → status `partial` + resume handles; team keeps running |
| 6 | `worker_failure_one_retry_then_reassign` | first failure → retry once; second → reassign or degrade; team never aborts |
| 7 | `role_model_routing_overrides_default` | brief `model`/`effort` override the orchestrator's; absent → inherit |

**Step 3:** Implement per test, minimal. Reuse the existing tool/permission machinery —
do not build a parallel mechanism.

**Step 4:** `cargo test -p vikram-runtime -p vikram-policy` → PASS;
`cargo clippy -p vikram-runtime -p vikram-policy --all-targets` → clean;
`cargo fmt --all --check` → clean.

**Step 5:** Commit: `feat: orchestrator spawn_worker with typed briefs and trust narrowing`.

---

### Task 2: JARVIS Phases A+B, relationship model, semantic verification [WORKER A, after Task 1]

**Owns:** same crates as Task 1. Starts only after Task 1 commits.

**Contracts (TDD, same cycle):**
- Phase A `durable_presence_session_identity`: daemon restarts resume the same
  presence session id — test: create session, simulate restart, id survives.
- Phase B liveness: states awake/dozing/asleep; `/sleep` and `/wake` commands
  transition with audit events; invalid transitions rejected.
- Relationship model (store): typed rows people / projects / preferences / mood;
  context assembly injects them before every turn — test: row present → appears in
  assembled context; row absent → context unchanged.
- Semantic verification seed (CONTINUE.md P0): for ONE existing feature (pick the
  memory store), add an acceptance test that would FAIL if the feature were removed
  — proving "tests pass" requires "feature exists".

**Verify:** per-crate tests green, clippy clean, fmt clean.
**Commit:** `feat: jarvis phases a+b — presence, liveness, relationship rows`.

---

### Task 3: Finish Phase D voice loop [WORKER B: voice]

**Owns:** `crates/vikram-voice/**`, `crates/vikram-cli/src/voice.rs`,
voice-related lines in `crates/vikram-cli/src/main.rs`.
Forbidden: tui, runtime, policy, store, daemon crates.

**Step 1:** Read the WIP state after Task 0's commit — what exists, what's stubbed.
**Step 2 — Contracts (TDD):**
- Push-to-talk round trip: trigger → capture → transcribe → text delivered to the
  active session. If Voxtype CLI is the ASR path (installed, VAD on), drive it as a
  subprocess; do not write a new ASR engine.
- TTS output path: text → speech (existing system TTS if present — check what the
  WIP already chose — do not invent a new dependency; Cargo.toml additions only if
  the WIP already added them).
- Interruptible: speaking/processing aborts cleanly on new input or cancel — test the
  cancellation path, not just the happy path.
- Every external-process failure (Voxtype missing, TTS missing) → honest user-facing
  error + degraded mode flag, never a silent pass.

**Verify:** `cargo test -p vikram-voice` green (integration tests may be
`#[ignore]`-tagged if they need audio hardware — then a unit-level harness must
still cover the state machine), clippy, fmt.
**Commit:** `feat: voice push-to-talk loop via voxtype with interrupt and degrade`.

---

### Task 4: Jarvis console in the TUI [WORKER C: console]

**Owns:** `crates/vikram-tui/**` only. Parallel with Tasks 1/3/5 — no shared files.
If it needs events from voice/daemon, define the event TYPE it consumes against the
existing TUI event bus pattern and wire it — do not edit other crates to emit new
events; leave TODO-free stub consumers only if the event already exists.

**Contracts (TDD where the TUI has an existing test pattern; otherwise thin
view-model tests):**
- Arc-reactor state glyph component: `●` idle muted / `◐` thinking (pulse) / `▲`
  approval (urgent) / `✓` done (fade) — pure view model, state → glyph+style
  mapping unit-tested.
- Team HUD panel: workers list with brief summary, status (running/partial/done),
  result reference — renders from orchestrator status events; empty state shows the
  muted idle glyph, never a fake panel.
- Glyph state transitions driven by existing TUI events only; no new threads.

**Verify:** `cargo test -p vikram-tui`, clippy, fmt.
**Commit:** `feat: tui arc-reactor glyph and team hud panel`.

---

### Task 5: Phase C daemon triggers + silence policy [WORKER D: daemon]

**Owns:** `crates/vikram-daemon/**` only. Parallel with Tasks 1/3/4.

**Contracts (TDD):**
- Event-driven trigger kinds beyond cron: file-change (path watch), long-job
  completion, inbox-arrival — at minimum ONE non-cron trigger working end to end.
- Silence policy: config-gated rules for when NOT to speak (user busy flag, quiet
  hours, dedupe window) — violating a rule suppresses the notification AND logs the
  suppression.
- Trigger firing → one audit event + one notification (existing channels) — no
  direct agent turn spawn without an approval boundary unless the trigger's config
  explicitly allows it.

**Verify:** `cargo test -p vikram-daemon`, clippy, fmt.
**Commit:** `feat: daemon event triggers with silence policy`.

---

### Task 6: Integration gate + docs [MAIN AGENT]

**Step 1:** `cargo fmt --all --check` && `cargo clippy --workspace --all-targets` &&
`cargo test --workspace` — all green on the merged tree.
**Step 2:** `cargo run -p vikram-cli --` smoke: TUI boots, glyph renders idle state.
**Step 3:** Fold in the 3 doc-check reports (root/docs/periphery) → update
CONTINUE.md, AGENTS.md, and any flagged md file per the user's standing order
("update your agents .md osurl .md and others").
**Step 4:** One integration commit: `docs: jarvis upgrade integration and verified state`.
**Step 5:** Report to user: what landed, test counts (observed, not claimed), what
remains (Phase C breadth, semantic verification rollout beyond the seed, GUI later).

---

## Execution

User pre-selected: parallel sub-agents in this session (delegated-execution).
Dispatch order: Task 0 (main, blocking) → Tasks 1+3+4+5 in parallel → Task 2 after
Task 1 → Task 6 last. Each worker brief carries: owned paths, forbidden paths,
contracts table, verify commands, commit message, and "read the module before
writing; follow existing patterns; no comments unless non-obvious; never swallow an
error".
