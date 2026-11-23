# Vikram build plan — from the capability ledger to a working assistant

Status: 2026-11-23. Companion to `docs/plans/2026-11-23-jarvis-capabilities.md`, which holds all
85 capabilities with `path:line` evidence and their status. This file is the work order: what is
built next, in what order, and what test proves it.

Rule of record (from the ledger): a capability moves to `have` only when a production caller exists
and a test names the behaviour. A phase is not done while a doc still describes it wrongly.

## What is already real (do not rebuild)

Memory and learning (1-9, 11), voice with live captions, sentence streaming, `tts_language` and an
interruptible reply (12, 13, 15, 16), `doc.read` (28), `research` (26), `terminal.exec` over docker
and ssh (51, 52), `system.status` (49), plugin tools as `plugin.<name>` (64), durable workers with a
boot restore (44), curator lifecycle with a rollback ledger, per-persona skills, the config surface
for self-editing (67, host half), daemon liveness with TUI `/sleep` `/doze` `/wake` `/presence`.

Not buildable here, with reasons, in the ledger §7: holograms and suit control (71-74, 78, 81, 82,
84), robotics (60), smart home (58), face/object/camera/video/spatial (18-22, 24) which need a sensor
and a model that is not in the tree, wake word (14) which needs an ONNX engine, and IoT (59).

## Phase P1 — the assistant checks its own work

The weakest link in the whole system: it can run `verify`, and nothing makes it. `tools/verify.rs`
passes `|_, _| None` as the repair hook (`tools/verify.rs:44`), so `Budget::attempts > 1` cannot help
and a failed build is a message, not a correction.

1. **Repair hook that actually repairs** — done. `tools/verify.rs` now passes a hook that builds a
   `RepairInstruction` (step, command, `FailureKind`, parsed failing paths, 1,000-char excerpt) and
   returns the paths so the loop retries; `verification.rs` documents it. Tests:
   `a_failed_verify_attempts_a_repair_and_reports_each_attempt`,
   `a_repair_instruction_names_the_file_the_error_points_at`,
   `a_timed_out_attempt_does_not_retry`, `a_huge_failing_output_is_bounded_in_the_instruction`.
2. **Unverified-change reminder** — when a turn edits files through `fs.write`/`fs.edit` and never
   calls `verify`, the next turn's context says so once. Acceptance:
   `an_edited_tree_is_flagged_until_it_is_verified`.
3. **Claim-vs-diff check** — a heuristic that compares the paths and symbols the assistant says it
   changed with what `git diff` reports, and warns on a claim with no matching change. Must be
   advisory and never block a turn. Acceptance: `a_claim_about_a_file_the_diff_does_not_touch_is_flagged`,
   `a_claim_the_diff_supports_is_not_flagged`.

## Phase P2 — the operator model, for real

4. **`entities.profile_id` writer and reader** — done. `vikram entities new --profile <id>` and
   `entities list --profile <id>` (`crates/vikram-cli/src/main.rs`), `list_entities_for_profile`
   and `list_entities_owned_by_profile` (`crates/vikram-store/src/lib.rs`), and the unique slug
   index widened to `(owner_user, kind, slug, COALESCE(profile_id, ''))` so two profiles can each
   own the same slug. Tests: `an_entity_created_for_a_profile_is_listed_only_there`,
   `two_profiles_can_each_own_an_entity_with_the_same_slug`,
   `a_second_machine_wide_entity_with_the_same_slug_is_refused`.
5. **Affect in the dialectic model** — the existing insight kinds are prefers/habits/goals; add a
   tone signal only from explicit operator words, never inferred from punctuation. Acceptance:
   `an_explicit_tone_note_changes_the_next_turn`, `punctuation_does_not_create_a_tone_signal`.
6. **Anticipation from the calendar** — the daemon already schedules; a due-job lookahead that says
   what is about to run, once, at the start of a session. Acceptance:
   `a_session_start_names_the_next_scheduled_job`.

## Phase P3 — autonomous depth

7. **Worker peer channel** — workers can only report to a parent (`orchestrator.rs:502`). Add a
   mailbox a sibling can read, bounded and audited. Acceptance:
   `a_worker_reads_a_note_from_a_sibling_once`.
8. **Machine observation** (row 46) — proactive help currently watches only its own jobs and file
   watches. Add disk-fill, failed-service and long-run thresholds to the hourly self-check, with
   quiet-hours and presence respected. Acceptance: `a_disk_above_the_threshold_is_reported_once`.
9. **Network reachability** (row 50) — `mapek` hardcodes `network_active: false`. Probe a configured
   endpoint with a timeout, and report only on a transition. Acceptance:
   `a_lost_endpoint_is_reported_on_the_transition_not_every_tick`.

## Phase P4 — operations the operator actually runs

10. **Database backup/restore** (row 53) — `db.backup` for sqlite (a copy plus integrity check) and a
    shell path for postgres/mysql dumps, workspace-confined, refusing a destination inside the source.
    Acceptance: `a_backup_is_readable_and_passes_integrity`, `a_destination_inside_the_source_is_refused`.
11. **Deployment** (row 54) — `deploy.run` that runs a declared command list from the workspace with
    the Execute gate and a rollback command that must also be declared. Acceptance:
    `a_deploy_without_a_rollback_command_is_refused`, `a_failed_deploy_runs_the_rollback`.
12. **Threat signal** (row 56) — no intrusion detection is in scope; what is honest is a log-shape
    check over journald/auth logs for failed-login bursts. Acceptance:
    `a_failed_login_burst_is_reported_with_its_source`.

## Phase P5 — intelligence depth

13. **`research` ranking** — dedupe exists; add a rank by source agreement and surface contradictions
    instead of concatenating. Acceptance: `two_sources_that_disagree_are_reported_as_a_conflict`.
14. **XLSX cell addressing** (row 28 remainder) — read a named range, not just shared strings.
    Acceptance: `a_named_range_is_read_by_its_sheet_and_bounds`.
15. **Ensemble** (row 63) — a `cross_check` tool that asks two configured models the same question and
    returns agreement or the exact divergence. Acceptance: `two_models_that_diverge_are_both_shown`.
16. **Self-optimization with a proven cause** (row 67) — only after P1 lands: change one allowlisted
    key when a benchmark regression has a measured cause, and roll back if the next run is worse.
    Acceptance: `a_self_change_that_worsens_the_benchmark_is_rolled_back`.

## Phase P6 — the console

17. **Arc-reactor status** — one glyph reflecting liveness and work (`●` awake, `◐` dozing, `◌`
    asleep, `▲` degraded), already half present in the status bar. Acceptance: `the_status_glyph_follows_liveness`.
18. **Team HUD** — a panel listing live workers with status and session, reading the durable `workers`
    table. Acceptance: `the_hud_lists_a_restored_worker`.

## Order and gates

P1 → P2 → P3 → P4 → P5 → P6, but items inside a phase may run in parallel when their files are
disjoint. Every phase ends with `cargo fmt --all`, `cargo clippy --workspace --all-targets`, the
phase's tests, and one `cargo test --workspace` whose total is read and recorded here.

## Test log

| date | what | result |
|---|---|---|
| 2026-11-23 | workspace, before this plan | 3,769 passed, 0 failed (mid-run read) |
| 2026-11-23 | `cargo test --workspace` | 4,074 passed, 0 failed across 36 binaries; the only red binary is the vendored `wavo --test models` (54 failed: it downloads GGUF models) and `wavo`'s `gpu::tests::gemm` hangs on this host. Vendored crate, not Vikram code. |
| 2026-11-23 | `cargo test --workspace --exclude wavo` | **3,946 passed, 0 failed** across 50 result lines, every one `ok`. This is the number to quote for Vikram itself: leaving out the vendored wavo crate also drops its own passing tests, so 3,946 replaces 4,074 as the honest figure. |

## Status 2026-11-23 (checkpoint)

Verified green in this pass, each with a test that names the behaviour:

| item | proof |
|---|---|
| P1.1 repair hook | `tools/verify.rs` builds a `RepairInstruction`; `--lib verify` 33 passed, `--lib verification` 28 passed |
| P2.4 profile-scoped entities | `list_entities_for_profile`, `list_entities_owned_by_profile`, CLI `--profile`; store lib 50 passed |
| P2.4 index | `entities_slug` widened to `(owner_user, kind, slug, COALESCE(profile_id, ''))` by `ensure_profile_scoped_entity_index` |
| P4.10 `db.backup` | `tools/db.rs`, integrity checked before and after the copy; `--lib db::` 7 passed |

Done since this checkpoint: P4.11 `deploy.run` (8 tests, refuses a deploy with no rollback, refuses destructive steps, runs the rollback on failure) and P4.12 `system.threats` (7 tests, counts failed-auth lines and says so when no log was readable) — `tools/deploy.rs`, `tools/threat.rs`, registration in `tools/mod.rs` + `tools/core.rs`, gate counts 108→110 and 97→99. Also fixed the `turn.rs` fixture trap: `an_over_budget_transcript_folds_before_the_request_leaves` now uses `context_window: 24_000`, so tool schemas no longer have to be terse (`cargo test -p vikram-runtime --lib` → 1,126 tests).

Done since this checkpoint: P4.11 `deploy.run`, P4.12 `system.threats`, and P3.9 the default-route probe (`probe_default_route` in `mapek.rs`, tri-state `NetworkRoute`, one durable transition in `mapek.network`; tests `a_lost_default_route_is_reported_on_the_transition_not_every_tick`, `an_unreadable_route_table_is_unknown_not_down`, `a_route_that_never_changed_is_reported_once`; `cargo test -p vikram-evolution` 157 passed).

Done since this checkpoint: P1.2 unverified-change reminder — `turn.rs` keeps a per-session `TreeCheck`, a successful `fs.write`/`fs.edit`/`fs.delete` marks the tree unverified, one `Info` notice per session, and one `unverified edits:` host line in the assembled context until `verify`/`test.run`/`build.run` succeeds (tests `an_edited_tree_is_flagged_until_it_is_verified`, `a_read_only_turn_does_not_flag_the_tree`, `the_reminder_is_said_once_per_session`, `a_second_session_does_not_inherit_the_unverified_mark`; runtime lib 1132 passed, clippy clean).

Done since this checkpoint: P5.14 `doc.read` now reads XLSX named ranges — `defined_names()` parses `xl/workbook.xml`, an optional `range` arg resolves `Sheet!$A$1:$B$9` (case-insensitive, `$` absolute, multi-letter columns), cell parsing covers shared strings/inlineStr/booleans/formulas/numbers, and refusals name what exists (`#REF!`, 3D, external, union and dynamic refs each say why). Tests `a_named_range_is_read_by_its_sheet_and_bounds`, `an_unknown_range_name_lists_the_names_that_exist`, `a_workbook_without_named_ranges_says_so`; `doc::` 15 passed, runtime lib 1132 passed.

Done since this checkpoint: P1.3 claim-vs-diff advisory — `turn.rs` scans the turn's FINAL reply for workspace-relative path tokens and fenced `+++ b/<path>` headers, compares them against the paths a successful mutating tool actually touched, and emits at most one Warn notice naming the first unsupported claim; silent when no mutating tool ran or every claim is backed (tests `a_claim_about_a_file_the_diff_does_not_touch_is_flagged`, `a_claim_the_diff_supports_is_not_flagged`, `a_conversational_answer_naming_a_file_is_not_flagged`; runtime lib 1,141 passed, clippy clean).

Done since this checkpoint: P3.7 worker peer mailbox — `WorkerNote { from_worker, to_worker, text, at_ms }` on `WorkerRecord.notes` (2,000-char cap, `serde(default, skip_serializing_if)`), `WorkerRegistry::post_note` refuses a self-address, empty text, an over-cap note and an unknown worker, `take_notes` drains durably through the existing `workers.record` JSON (tests `a_worker_reads_a_note_from_a_sibling_once`, `a_note_to_an_unknown_worker_is_refused`, `notes_survive_a_reopen_of_the_store`; `worker` filter 11 passed).

Done since this checkpoint: P5.13 `research` ranking and the disagreement signal — `gather` counts how many query variants reached a page, `rank` orders by reach then term hits then fetch order, `disagreements()` reports one `possible disagreement:` line per pair whose sentences carry opposite polarity markers (tests `a_source_reached_by_two_queries_ranks_above_one_reached_by_one`, `two_sources_that_disagree_are_reported_as_a_conflict`, `sources_that_agree_produce_no_conflict_line`; `research` filter 12 passed, runtime lib 1,152).
Done since this checkpoint: the worker mailbox reached the model — `worker.note` (Delegate / LocalMutation) and `worker.notes` (Delegate / None) in `tools/worker_note.rs`, registered in `with_builtins` and gated with `hive.delegate`/`spawn_worker`; `from` resolves through `runtime.workers.by_session` when omitted, an empty drain answers `no notes for \`X\`` with `ok: true`, and the pinned gate counts moved 110→112 / 99→101 (tests `a_note_posted_by_one_worker_is_read_once_by_another`, `a_note_to_a_worker_that_does_not_exist_is_refused`, `the_tools_are_registered_and_advertise_their_schemas`; `worker` filter 14 passed, runtime lib 1,147).
Done since this checkpoint: P6.18 team HUD — `WorkerSnapshot` on `StateSnapshot.workers` (`types.rs`), built in `RuntimeInner::snapshot()` from `WorkerRegistry::list()`, surfaced in the TUI as `/workers` (Action, `App::worker_lines()`) and as a `⚙ N worker` status-bar segment whenever any worker is `running` or `partial`; test `the_hud_lists_a_restored_worker` (tui 256 passed, clippy clean apart from the pre-existing `voice.rs` items-after-test-module warning). P6.17 needed no code: `widgets/panels.rs` already draws `◌ asleep` / `◐ dozing` from `App::presence`.

Done since this checkpoint: P2.5 affect from explicit operator words — `Tone { Frustrated, Tired, Pleased, Urgent }` and `explicit_tone(text)` in `crates/vikram-runtime/src/dialectic.rs` require a first-person marker within 48 bytes of the feeling phrase and reject third-person, negated and punctuation-only signals; the tone rides the existing insight store as `InsightKind::Tone`, key `tone:<as_str>`, so it decays and renders through the usual supplement (tests `explicit_tone_phrases_are_recognised`, `punctuation_does_not_create_a_tone_signal`, `a_third_person_feeling_is_not_the_operators_tone`, `a_negated_feeling_is_not_a_tone`, `an_explicit_tone_note_changes_the_next_turn`; `dialectic` filter 32 passed).
Done since this checkpoint: P2.6 next-job lookahead — `RuntimeInner::next_scheduled_job()` reads `daemon_jobs` read-only from the shared database (the scheduler already writes into the same file) and `RuntimeHandle::create_session` emits `next scheduled: <name> in N hours` as one Info notice; `relative_time` rounds to minutes/hours/days and says "is due now" instead of a negative wait (test `a_session_start_names_the_next_scheduled_job`; runtime lib 1,153 passed, 1 ignored).

Done since this checkpoint: P3.8 machine thresholds — `mapek.rs` now reads the real mounted filesystem (`df -Pk`, `disk_usage` share + `disk_usage_known`) and `systemctl --failed` (`failed_services: Option<usize>`), raises `disk.fill` (Medium at 90%, High at 95%) and `services.failed`, and reports each once per crossing from durable `mapek.disk`/`mapek.services` baselines; unknown never reads as zero and never overwrites a baseline. Probes are injectable (`disk_probe`, `services_probe`, `quiet_host()`), and the first readable reading is a baseline so a fresh store cannot fire a false burst. Tests `a_disk_above_the_threshold_is_reported_once`, `a_disk_below_the_threshold_is_silent`, `an_unreadable_disk_is_not_a_zero`, `failed_services_are_counted_from_the_probe`, `a_recovered_disk_clears_the_issue`, `host_probe_output_is_parsed_not_guessed`; `mapek` filter 11 passed, `cargo test -p vikram-evolution` 114 + 20 + 15 + 14, clippy clean.
Recorded residuals: `systemctl`/`df` have no timeout, so a wedged dbus can stall one hourly tick; a filesystem already full on the very first probe of a fresh store is not reported until it changes band.

Done since this checkpoint:

- **P5.15 `cross_check`** — `crates/vikram-runtime/src/tools/cross_check.rs` asks the same question of two models and shows both answers. Divergence is claimed only on evidence: a clean one-sided clash on one of 12 fixed polarity pairs, or numeric mismatch in the same unit. Otherwise the line reads `no divergence detected … Two agreeing answers are not proof of correctness.` Refuses a missing `other_model`, a self-comparison, and a provider error (naming the failed side). Tests `two_models_that_diverge_are_both_shown`, `two_models_that_agree_are_not_flagged`, `a_missing_other_model_is_refused`, `the_tool_is_registered_and_advertises_its_schema` plus a numeric-rule test; runtime lib 1,158 passed.
- **P5.16 measured self-change** — `crates/vikram-evolution/src/self_trial.rs` (`trial_change`, `trial_history`, `TrialOutcome`): apply an allowlisted setting, run the benchmark once, keep it when it is no worse and roll it back when any metric dropped. No baseline reading means inconclusive, which rolls back rather than keeping. Every trial is recorded in `engine_records` under `self_trial`. Seven tests in `crates/vikram-evolution/tests/self_trial.rs`; `cargo test -p vikram-evolution` 170 passed. Exposed as `vikram evolution trial <key> <value> <benchmark>` through `ConfigFileSurface`, which can write exactly `context:memory_budget_tokens` and refuses the rest because the learning policies live in the running runtime.
- **P6.18 team HUD**, **P6.17 liveness glyph** (already drawn by `widgets/panels.rs`), **P2.5 tone**, **P2.6 lookahead**, **P3.7 worker mailbox + `worker.note`/`worker.notes`**, **P5.13 research ranking**, **P3.8 host watch**, **P1.1-P1.3**, **P2.4**, **P4.10-P4.12**.

In flight: nothing.

Open, in order: nothing on the buildable list. The remaining 22 capability rows need hardware, a model that is not in the tree, or are fiction (see the ledger).

Resolved trap: `turn::tests::an_over_budget_transcript_folds_before_the_request_leaves` used a 14,000-token window while ~110 tool schemas charged ~12.1k tokens inside the same window, so `usable_input()` sat at zero and no fold could trigger. The fixture now uses `context_window: 24_000`; tool schemas no longer need to be artificially terse.

## Cycle 2 — deepening the partials

The buildable list was empty because it was scoped to the first pass. Cycle 2 targets the
`partial` rows that are finishing-off work rather than new organs:

- **47 Predictive Assistance → partial, done.** `likely_next_step` in
  `crates/vikram-runtime/src/learning.rs` counts success → next-first-tool pairs inside one
  agent/workspace sequence and answers only at three or more occasions; the session start prints
  `likely next step: ... after N of the last M ...`. Tests
  `a_repeated_pattern_produces_a_likely_next_step`, `fewer_than_three_occasions_say_nothing`,
  `failed_outcomes_do_not_build_a_pattern`, `a_session_start_names_the_likely_next_step`.
  A pattern in history is evidence of what followed, never a prediction of intent.
- **20 Screen Understanding → have, done.** The last screenshot of a turn is attached to the next
  request as a real image block, once, dropped after three turns, capped at 1.5 MiB and labelled with
  the tool and path; a model that is not on the vision allowlist gets a text note instead of pixels.
  Tests `a_screenshot_from_the_previous_step_reaches_the_next_request`,
  `a_screenshot_is_attached_once_and_then_forgotten`, `an_oversized_image_is_not_attached`,
  `a_turn_without_a_screenshot_sends_no_image`,
  `a_model_that_cannot_see_images_is_told_about_the_screenshot`.
  Known edges: one image at a time, no history, and a fallback model can still receive the bytes.

## Cycle 3 — finishing, undoing, and acting on the host

Cycle 2 closed two partial rows. Cycle 3 closes the loop where a learned change could be kept by
mistake and where a signal never reached an actuator:

- **`vikram learning undo <id>` — done.** An `Auto` memory write records key, scope and the previous
  value (`LearningApplication`, `#[serde(default)] applied` in `learning.json`); undo restores it or
  deletes the key when there was none, refuses a never-applied proposal with a pointer to
  `vikram learning reject <id>`, and says `already reverted` on a second undo. Tests
  `an_auto_applied_memory_write_can_be_undone`,
  `undoing_a_write_with_no_previous_value_removes_the_entry`, `an_unknown_id_reports_nothing_to_undo`,
  `undo_refuses_a_proposal_that_was_only_staged`; the real binary printed all four messages and wrote
  `"reverted": true`.
- **70 Emergency Response → second actuator, done.** The maintenance job reads the disk share from
  the MAPE-K pass and puts the daemon to `Dozing` at >=95%, waking it below 90% **only if that
  actuator set the doze** (`presence.reason = "disk"`); a hand-set doze is never reverted and an
  unreadable disk changes nothing. `react_to_disk` in `crates/vikram-daemon/src/runner.rs`, tests
  `a_full_disk_puts_the_daemon_to_doze_once`,
  `a_hand_set_doze_is_not_reverted_by_a_recovered_disk`, `an_unreadable_disk_changes_nothing`;
  `cargo test -p vikram-daemon` 164 passed.
- **49 System Monitoring → processes and GPU, done.** Busiest five processes by resident bytes from
  `/proc/<pid>/statm` x 4096 with `/proc/<pid>/comm`, ranked in a pure `busiest()`; `probe_gpu()` runs
  `nvidia-smi --query-gpu=...,--format=csv,noheader` through `wait_with_timeout` (3 s) and the report
  says `gpu: no GPU tool was found (nvidia-smi is not installed)` instead of failing; the whole report
  is now inside the shared 20 KB `truncated()`, which it previously was not. Tests
  `the_busiest_processes_are_listed_by_resident_memory`,
  `a_missing_gpu_tool_says_so_instead_of_failing`; `vikram-runtime --lib` 1,173 passed / 1 ignored.
  Not exercised: the `nvidia-smi` success path, because no NVIDIA binary is installed on this host.
- **67 Self-Optimization → partial.** The surface allowlist, `self_trial`, and
  `vikram evolution trial` exist; nothing proposes a change on its own yet.
- **`entities(profile_id)` index added** so the profile-scoped listings do not table-scan.
- **50 Network Monitoring → have.** `endpoint_probe` actually asks the gateway (`TcpStream::connect_timeout`
  on port 53, 1.5 s): `Ok` and `ConnectionRefused` both count as reachable, a timeout counts as lost, an
  unreadable routing table as unknown. Durable `mapek.endpoint`, reported once per crossing. Tests
  `an_endpoint_timeout_is_reported_once`, `a_refused_connection_still_counts_as_reachable`,
  `an_unreadable_routing_table_is_not_a_lost_endpoint`.
- **83 Predictive Maintenance → disk trend.** A 24-sample history in `mapek.disk_history` plus a pure
  `project_disk_fill` (>=3 samples over >=1 h, positive growth, share under 95%) that reports the hours
  until 95%, Medium under 24 h. Tests `a_growing_disk_is_projected_before_it_fills`,
  `a_flat_disk_is_not_projected`, `a_single_reading_is_not_a_trend`;
  `cargo test -p vikram-evolution` 176 passed.
- **44 Persistent Autonomy → resume door.** `worker.resume` (in `tools/worker_note.rs`, registered in
  `tools/core.rs` at 114 builtins / 103 unattended / 53 plan) calls `RuntimeHandle::resume_worker`,
  refuses a worker whose status is not `partial` while naming that status, and lists the resumable ids
  when none is named. Tests `a_partial_worker_can_be_resumed_through_the_tool`,
  `resuming_a_finished_worker_is_refused_with_its_status`,
  `an_unknown_worker_lists_the_ones_that_can_be_resumed`.
- **Self-check timing fixed, not hidden.** The hourly check now probes the host (routing table, a
  gateway connect, `df`, `systemctl`), so one tick takes ~3 s on this machine and the old 5 s deadline
  for five ticks was wrong: `wait_for_ticks` and the three-tick loops in `runtime.rs` now allow 120 s
  with a comment saying the assertion is that ticks happen, not how fast the host answers. Three
  previously red tests (`self_check_interval_fires_and_cannot_be_scheduled_twice`,
  `scheduled_evolution_cycle_never_runs_when_disabled`, `..._runs_once_per_interval`) now pass.
- **85 Contextual Awareness → have.** `where_we_left_off` in `crates/vikram-runtime/src/learning.rs` turns
  the newest experience into one line — ``last time: `port the exporter` (edit) ended with an error — that
  is where the work stopped`` — and `RuntimeHandle::create_session` emits it as an `Info` notice beside
  `next scheduled:` and `likely next step:`. Failure / partial / unverified-success are worded
  separately; a chat-only turn is skipped; absolute paths are reduced to the file name. Tests
  `a_failed_last_session_is_named_as_unfinished`, `a_successful_verified_session_says_nothing`,
  `no_history_says_nothing`, `a_session_start_names_where_the_last_work_stopped`.
- **76 Threat Assessment → count shapes named.** `assess()` in `tools/threat.rs` turns the existing
  tally into one hedged sentence: one address repeating (>=5) is a likely brute-force attempt, >=3
  addresses with >=2 tries each looks like a coordinated scan, many single failures look like
  automated scanning, a clean spread is called what typos look like, and an unattributable log says so.
  Tests `one_source_repeating_is_named_a_brute_force_attempt`,
  `many_addresses_trying_twice_is_named_a_scan`,
  `failures_with_no_address_are_said_to_be_undecidable`, `a_clean_log_has_no_assessment`.
- **69 Multi-Device Presence → abandoned, evidence stale.** The ledger cited `gateway.rs:1214`, but
  `crates/vikram-gateway/src/gateway.rs` does not exist; the real files are `routing.rs`, `pairing.rs`,
  `ingress.rs` and friends. Row left unchanged rather than writing another unverified line.
- **Vikram-only suite: 3,946 passed, 0 failed** across 50 binaries

- **42 Self-Correction → have.** When a `verify` fails with a non-timeout failure whose report names
  files, the turn hands the verifier's own repair instruction back to the model once, as a host-trust
  message, and never twice: `AutoRepair` is a per-turn local, a second failing `verify` only warns,
  and a timeout is never retried. One `Info` notice on the hand-back, one `Warn` if the retry fails
  again. Tests `a_failed_verify_is_handed_back_to_the_model_once`,
  `a_second_failing_verify_in_the_same_turn_is_not_retried`,
  `a_timed_out_verify_is_not_retried_automatically`,
  `a_repair_with_no_named_file_is_not_handed_back`; `cargo test -p vikram-runtime --lib turn::tests`
  → 78 passed.
- **68 Digital Twin → the machine has a model too.** `HostProfile` in
  `crates/vikram-evolution/src/mapek.rs` (CPUs, RAM, one filesystem's total/free, failed services,
  default route, `captured_ms`), captured by the probes already there, persisted once at
  `mapek.host`/`profile`, rendered by `describe_host_profile`. `RuntimeInner::machine_line()` puts
  that one line in every prompt as a `Trust::Host` system message labelled `machine`, frozen per
  process so it costs the prompt cache nothing. Real line on this host:
  `machine: 8 CPUs, 15.2 GiB RAM, 19.7 GiB free of 474.9 GiB, 0 failed services, network up`.
  Unknown facts are omitted, never printed as zero. Tests `a_host_profile_names_what_the_probes_saw`,
  `unknown_facts_are_left_out_of_the_line`, `a_host_profile_survives_a_store_reopen`,
  `the_machine_line_reaches_the_prompt`; `cargo test -p vikram-runtime --lib` → 1,189 passed,
  1 ignored.
  (`cargo test --workspace --exclude wavo`). The vendored wavo crate contributes its own tests and
  two known hazards on this host: `--test models` downloads GGUF files (54 failures offline) and
  `gpu::tests::gemm` hangs.

