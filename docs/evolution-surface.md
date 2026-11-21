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
NL parsing (store-backed answers), `autonomy` pending approvals, and
`self_recovery` restart recovery + drift detection are all reachable.

## Operator-gated via `vikram evolution` CLI (mutating, confirmed)

`cycle`, `benchmark`, `approve`, `reject` each require `--yes` or an
interactive confirmation before mutating durable state:

| Command | Engine call | Effect |
|---|---|---|
| `cycle` | `mapek.run_cycle` (one bounded cycle) | monitor→analyze→plan→execute; re-queues failed reflection jobs, records issues as proposals |
| `benchmark [name]` | `self_evaluation.run_benchmark` | persists `EvaluationResult` from recorded experiences |
| `approve`/`reject <id>` | `autonomy.approve_request`/`reject_request` | decides a durable approval request (spends it on approve) |

## Durable but dormant (state persists, no production caller)

These engines now persist their state to `engine_records` and reload on
`EvolutionEngine::open`, so a future caller gets correct cross-restart
behavior for free — but nothing in runtime/daemon/CLI invokes the mutating
paths yet.

| Subsystem | Persisted (namespace) | Gap |
|---|---|---|
| `autonomy` | every `ApprovalRequest` incl. spent state (`autonomy.approval`) | decisions reachable via `vikram evolution approve/reject`; nothing calls `request_approval` automatically yet |
| `self_evaluation` | benchmarks, results, metric history (`self_evaluation.*`) | benchmarks runnable via `vikram evolution benchmark`; no scheduler creates/runs them automatically |
| `mapek` (mutating half) | `KnowledgeBase` + `cycle_count` (`mapek.state`) | one bounded cycle via `vikram evolution cycle`; still not scheduled |
| `self_improvement` (execute half) | `executions` history (`self_improvement.execution`) | self-check records a `Completed` execution for the top opportunity (analysis-only, deduped); `execute_improvement` still has no caller |
| `hive` (`HiveLearning`) | lessons persist in `hive_lessons` | live: `hive_run` records a per-hive lesson on completion and injects the hive's 3 most recent lessons into participant context |

## Process-local scaffolds (documented, not wired to fakes)

| Subsystem | Process-local state | Notes |
|---|---|---|
| `self_awareness` | `current_model` | scaffold: `inspect_*` returns representative constants |
| `self_config` | `change_history` | records intent only; `applied` can never become `true` |
| `self_adaptation` | `adaptation_history` | detection reads store metrics; every action errors with a reason → `record_issue` proposal |
| `self_development` | `projects` | 14-stage lifecycle records intent; produces no deployable artifacts |
| `self_recovery` | snapshots + state fingerprints persist (`self_recovery.snapshot`); in-session op queue stays local | drift detection live via `recover`; failed-change ops still record intent only |
| `conversational` | `autonomous_enabled`, `command_history` | handlers answer from the real store; commands with no honest implementation return `success:false`; autonomous toggle still guards nothing real |

## What "wiring" would still require

- **Automatic scheduling**: `mapek.run_cycle`, `self_evaluation.run_benchmark`,
  and autonomy approval *requests* are operator-gated via CLI — safe, but
  nothing proposes them on a timer. The runtime self-check could queue
  candidates; execution should stay confirmed until actions are proven.
- **Conversational depth**: handlers now answer from the store, but NL
  coverage is phrase-matched — free-form questions beyond the 15 patterns
  still get an honest unsupported response.
- **Self-recovery actions**: drift detection works; actual rollback still
  records intent — wiring real restore (skills/memory/config) is future
  work gated on snapshot coverage.
