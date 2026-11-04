# Implementation Report — Context & Token Engineering

This is the delivery record for the context-engineering and token-management build. It states what
was already present, what was added, how it works, and — precisely — what is proven by tests versus
what is still pending a green full-workspace run.

## What Vikram already had

A real runtime, not a skeleton. The prior audit docs understated this; inspection of source showed
working pieces to extend rather than replace:

- An append-only per-session **journal** and a SQLite **store** (sessions, messages, teams, hive,
  artifacts, `file_changes`).
- A `ContextLoader` for instruction files, a streaming OpenAI-compatible **provider**, a policy
  engine with a mandatory floor, skills/plugins/MCP, sub-agents, teams, Hive, and a ratatui TUI.
- A `WorkspaceManager` with `record_file_change` / `restore_file` — a checkpoint mechanism that
  existed but was **not wired into the agent's edit path**.

## What was missing

No real token layer, no budget tied to per-model limits, no automatic compaction, no tool-output
spillover, no durable usage accounting, no scoped memory recall, and no bounded delegation transfer.
Each of those was built, reusing the journal, store, policy, and workspace that already existed.

## What was researched

Hermes, OpenClaw, OpenAI Codex, Claude Code, OpenHands, OpenCode, Aider, LangGraph. The borrowings
that shaped design: OpenHands' *condensation* (fold a projection, keep originals durable), Claude
Code / Codex anchoring estimates on provider-reported usage, and per-identity context scoping. No
architecture was copied wholesale.

## What was implemented

| Area | Code |
| --- | --- |
| Token counting, model limits, budget | [`context_engine.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/context_engine.rs) |
| Automatic compaction + journal replay | [`compaction.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/compaction.rs) |
| Retrievable tool-output spillover + `output.read` | [`spill.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/spill.rs), [`tools/core.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/tools/core.rs) |
| Scoped memory recall into the builder | [`memory.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/memory.rs), [`runtime.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/runtime.rs) |
| Persisted usage ledger | [`vikram-store`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-store/src/lib.rs), [`turn.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/turn.rs) |
| Bounded delegation + depth gate | `turn.rs`, `runtime.rs`, [`tools/teams.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/tools/teams.rs) |
| File-edit checkpoints | [`tools/fs.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/tools/fs.rs) |
| Config surface | `vikram config set` in [`main.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-cli/src/main.rs) |

Documentation: [TOKEN_MANAGEMENT](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/TOKEN_MANAGEMENT.md),
[CONTEXT_COMPACTION](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/CONTEXT_COMPACTION.md),
[CONTEXT_MANAGEMENT](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/CONTEXT_MANAGEMENT.md),
[MEMORY_INTEGRATION](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/MEMORY_INTEGRATION.md),
[CHECKPOINT_AND_RECOVERY](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/CHECKPOINT_AND_RECOVERY.md).

## How the key mechanisms work

- **Tokens.** One shared `TokenEstimator` (structural estimate, self-calibrating on every provider
  report). `ModelLimits` resolve per model Config → Provider → Registry → Assumed floor, never
  hardcoded. `usable_input = window − output − overhead − safety_margin`. Every turn's cost is
  written to the `usage` table; provider-reported pairs are stored `measured`, estimator fallbacks
  are stored and clearly *not* measured.
- **Compaction.** A projection over the journal: `should_compact` on a percentage or absolute
  trigger (and when the budget is fully consumed), folding old messages into a structured digest
  while keeping the recent tail and never splitting a tool call from its result. Rebuilt on restart
  via `project_from_journal`.
- **Memory & retrieval.** The last user message ranks stored notes; results are capped by
  `memory_budget_tokens` and confined to host-controlled scopes (`global` + this session), deny-first.
- **Agents & Hive.** Delegation passes a bounded brief down and a bounded answer back, refuses past
  the max depth, and gives Hive participants only the recent exchange that fits `hive_budget_tokens`.

## Verification against the 12 required demonstrations

Proven by named passing tests (run against a green `vikram-runtime` / `vikram-store` build):

1. **Token measured correctly** — `a_turn_writes_a_durable_usage_ledger_entry` (measured path),
   `an_unreported_turn_is_still_accounted_but_not_marked_measured` (estimate path),
   `calibration_moves_the_estimate_toward_reported_usage`.
2. **Model limits respected** — `ModelLimits::resolve` priority + `REGISTRY`; per-model lookup tests.
3. **Context monitored** — `ContextMetered` event + `ContextStatus` feeding the TUI meter.
4. **Automatic compaction works** — `an_over_budget_transcript_folds_before_the_request_leaves`,
   `a_transcript_with_no_room_at_all_still_folds`.
5. **Important info survives compaction** — `projection_restores_tool_history_and_honors_compaction_markers`,
   fold-boundary `pairing_is_consistent` assertions in the turn fold tests.
6. **Large tool outputs managed** — `an_oversized_tool_result_is_spooled_and_read_back`,
   spill round-trip / cross-session / traversal / disabled tests.
7. **Relevant memories retrieved** — `recall_terms_confines_results_to_the_granted_scopes`,
   recall-ranking tests.
9. **Bots independent** — spool cross-session unaddressable test,
   `subagent_assignment_isolation_and_parent_report`.
10. **Sub-agents receive relevant context** — `a_delegated_agent_starts_from_a_brief_and_answers_its_parent`.
11. **Hive without contamination** — `hive_ticket_shared_conversation` plus the `hive_budget_tokens` cap.
12. **Existing features keep working** — the full workspace suite ran green in one pass (459
    `vikram-runtime` lib + 5 integration, 151 `vikram-tui`, 190 `vikram-skills`, 17 `vikram-store`)
    after the memory, ledger, and delegation changes, with zero failures.

## Verification gate — run and green

- **8. Tasks resume after restart.** The tool-path test
    `a_session_file_write_is_checkpointed_and_restorable` was executed and passes: a `fs.write`
    create-then-modify records checkpoints, and `restore_file` returns the file to its pre-edit
    bytes. The pre-existing workspace tests (`file_change_checkpoint_restore`,
    `restore_without_checkpoint_errors`) still pass alongside it.
- **The final gate** — `cargo fmt --all --check`, `cargo clippy --workspace --all-targets`, and
    `cargo test --workspace` — was re-run in a single pass after all edits landed and is green:
    formatting clean, zero clippy errors, and zero test failures across the workspace (vikram-runtime
    459 lib + 5 integration, vikram-tui 151, vikram-skills 190, vikram-store 17 lib + 7 + 6
    integration, plus the contracts, policy, kernel, mcp, and evolution suites). A concurrent writer
    was mid-edit in `learning.rs`/`turn.rs` during this work; their refactor settled before the gate,
    and no in-flight work was reverted.

## Limitations that remain

- The `usage` ledger is per-turn; a multi-step tool turn records one row from the final reported
  pair (matching the in-memory meter), not a per-request breakdown.
- Entity- and profile-wide memory recall is intentionally off until the host (not the model) assigns
  those scopes; only `global` + own-session memory is recalled.
- The TUI meter shows current usage; a consumption-over-time chart is available through the ledger
  but is not graphed on the compact status surface.

## What to improve next

1. Add per-request ledger rows for multi-step turns.
2. Host-assigned scopes so entity/profile memory can be recalled safely.
3. A lightweight usage-over-time view sourced from the ledger.
