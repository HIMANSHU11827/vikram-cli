# Evolution Surface — what is actually reachable

`crates/vikram-evolution` exposes a large `EvolutionEngine` facade, but not
every subsystem on it has a production caller. This page classifies each one
honestly. The same classification lives in the crate-level docs
(`vikram-evolution/src/lib.rs`) and in each module's header; keep them in
sync when wiring changes.

Durable state = rows in `evolution.db` (`EvolutionStore`, schema v3
`engine_records` table). Process-local state = in-memory `Vec`s/`HashMap`s
on the engine structs, lost on restart. Because the CLI opens a fresh engine
per invocation, process-local state is also invisible to `vikram evolution`
— CLI output reflects only the store.

## Live via runtime

| Subsystem | What runs | Where |
|---|---|---|
| `experience` | Turn-end experience recording with redaction | `vikram-runtime/src/learning.rs` (`on_turn_end`, called from `turn::finish`) |
| `reflection` | Heuristic reflection per recorded experience; boot-time `recover_and_process` drain | `learning.rs`, `Runtime::start` |
| `memory` | Layered memory writes/reads on the turn path and corrections | `learning.rs`, `runtime.rs` |
| `mapek.monitor` / `mapek.analyze` | Read-only health probes | hourly self-check, `runtime.rs` `run_self_check` |
| `self_improvement.identify_improvement_opportunities` (+ `last_opportunity_note`) | Store-evidence opportunity scan | same self-check; also `vikram evolution evaluate` |

## Live via `vikram evolution` CLI (read-only)

`vikram evolution status|experiences|proposals|memory|skills|evaluate|mapek|ask|approvals|recover`
(`crates/vikram-cli/src/evolution.rs`) opens the store at
`~/.vikram/evolution.db` and reads it. `dashboard.snapshot`, store listings,
`mapek.monitor`/`analyze`, the `self_improvement` scan, `conversational`
NL parsing, `autonomy` pending approvals, and `self_recovery` restart
recovery are all reachable — mutating paths stay unexposed.

## Durable but dormant (state persists, no production caller)

These engines now persist their state to `engine_records` and reload on
`EvolutionEngine::open`, so a future caller gets correct cross-restart
behavior for free — but nothing in runtime/daemon/CLI invokes the mutating
paths yet.

| Subsystem | Persisted (namespace) | Gap |
|---|---|---|
| `autonomy` | every `ApprovalRequest` incl. spent state (`autonomy.approval`) | nothing calls `request_approval`; `approvals` lists only what tests/other engines queued |
| `self_evaluation` | benchmarks, results, metric history (`self_evaluation.*`) | no scheduler runs `run_benchmark`; CLI cannot create benchmarks yet |
| `mapek` (mutating half) | `KnowledgeBase` + `cycle_count` (`mapek.state`) | `run_cycle`/`execute` still never scheduled; monitor/analyze are the live half |
| `self_improvement` (execute half) | `executions` history (`self_improvement.execution`) | `execute_improvement`/`track_improvement` have no caller; opportunities rebuild per scan |
| `hive` (`HiveLearning`) | lessons persist in `hive_lessons` | no production caller of `record`; dashboard `hive_lessons` count stays 0 |

## Process-local scaffolds (documented, not wired to fakes)

| Subsystem | Process-local state | Notes |
|---|---|---|
| `self_awareness` | `current_model` | scaffold: `inspect_*` returns representative constants |
| `self_config` | `change_history` | records intent only; `applied` can never become `true` |
| `self_adaptation` | `adaptation_history` | detection reads store metrics; every action errors with a reason → `record_issue` proposal |
| `self_development` | `projects` | 14-stage lifecycle records intent; produces no deployable artifacts |
| `self_recovery` | `snapshots`, `recovery_operations`, `failed_changes` | snapshots capture no real state; persisting intent-only records would fake durability |
| `conversational` | `autonomous_enabled`, `command_history` | reachable via `vikram evolution ask`, but handlers return canned scaffold responses |

## What "wiring" would still require

- **Scheduler ownership**: `mapek.run_cycle`, `self_evaluation.run_benchmark`,
  and `autonomy` approval flows need a caller — the runtime self-check is
  the natural host, but mutating cycles should stay operator-gated until
  their actions are proven safe (they can re-queue reflection jobs and spend
  approvals).
- **Conversational handlers**: `ask` parses and routes, but responses are
  canned — real answers need handlers that query the store instead of
  returning templates.
- **Self-recovery**: needs real state snapshots (config/skill/memory
  fingerprints) before "rollback" means anything.
