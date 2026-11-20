# Evolution Surface — what is actually reachable

`crates/vikram-evolution` exposes a large `EvolutionEngine` facade, but not
every subsystem on it has a production caller. This page classifies each one
honestly. The same classification lives in the crate-level docs
(`vikram-evolution/src/lib.rs`) and in each module's header; keep them in
sync when wiring changes.

Durable state = rows in `evolution.db` (`EvolutionStore`). Process-local
state = in-memory `Vec`s/`HashMap`s on the engine structs, lost on restart.
Because the CLI opens a fresh engine per invocation, process-local state is
also invisible to `vikram evolution` — CLI output reflects only the store.

## Live via runtime

| Subsystem | What runs | Where |
|---|---|---|
| `experience` | Turn-end experience recording with redaction | `vikram-runtime/src/learning.rs` (`on_turn_end`, called from `turn::finish`) |
| `reflection` | Heuristic reflection per recorded experience; boot-time `recover_and_process` drain | `learning.rs`, `Runtime::start` |
| `memory` | Layered memory writes/reads on the turn path and corrections | `learning.rs`, `runtime.rs` |
| `mapek.monitor` / `mapek.analyze` | Read-only health probes | hourly self-check, `runtime.rs` `run_self_check` |
| `self_improvement.identify_improvement_opportunities` (+ `last_opportunity_note`) | Store-evidence opportunity scan | same self-check; also `vikram evolution evaluate` |

## Live via `vikram evolution` CLI (read-only)

`vikram evolution status|experiences|proposals|memory|skills|evaluate|mapek`
(`crates/vikram-cli/src/evolution.rs`) opens the store at
`~/.vikram/evolution.db` and reads it. `dashboard.snapshot`, store listings,
`mapek.monitor`/`analyze`, and the `self_improvement` scan are all pure reads
— the CLI mutates nothing, and process-local engine state never appears.

## Store-backed but dormant

| Subsystem | State | Gap |
|---|---|---|
| `hive` (`HiveLearning`) | lessons persist in `hive_lessons` | no production caller of `record`; the table stays empty outside tests, so the dashboard's `hive_lessons` count is always 0 |

## Process-local analysis (dormant, awaiting a caller)

These engines' *analyses* read the durable store, but their own bookkeeping
is in-memory and nothing in the runtime schedules or calls the mutating
paths. They are honest scaffolding — kept, documented, not wired to fakes.

| Subsystem | Process-local state | Notes |
|---|---|---|
| `self_awareness` | `current_model` | scaffold: `inspect_*` returns representative constants, not live runtime state |
| `self_config` | `change_history` | records intent only; `applied` can never become `true` — no live config surface exists |
| `self_adaptation` | `adaptation_history` | detection reads store metrics; every `AdaptationAction` errors with a reason and is persisted as a `record_issue` proposal |
| `self_improvement` (execute half) | `opportunities`, `executions` | scan is live (above); `execute_improvement`/`track_improvement`/history have no caller |
| `self_development` | `projects` | 14-stage lifecycle records intent; produces no deployable artifacts |
| `self_evaluation` | `metrics_history`, `benchmarks`, `evaluation_results` | benchmark runs evaluate recorded experiences; unmeasurable metrics report `0.0` + "not measured" |
| `self_recovery` | `snapshots`, `recovery_operations`, `failed_changes` | scaffold: snapshots capture no real state; rollbacks record intent |
| `mapek` (mutating half) | `knowledge`, `cycle_count` | `plan`/`execute`/`run_cycle` never scheduled; only failed-cycle events reach the store |
| `autonomy` | `action_history`, `pending_approvals` | approval queues are per-process; the self-check does not consult it |
| `conversational` | `autonomous_enabled`, `command_history` | library with no entry point; several handlers return canned outcomes |

## What "wiring" would require

Before any mutating path (`mapek.run_cycle`, `self_*` execution, autonomy
approvals, conversational commands) can be scheduled safely, its state must
first persist to `evolution.db` — otherwise a restart silently drops pending
work and cross-process approval is impossible. See gap #4/#5 in
`docs/evolution-architecture.md` §6.
