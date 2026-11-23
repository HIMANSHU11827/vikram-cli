# Evolution Surface — what is actually reachable

`crates/vikram-evolution` exposes a large `EvolutionEngine` facade. This page
classifies each subsystem honestly. The same classification lives in the
crate-level docs (`vikram-evolution/src/lib.rs`) and in each module's header;
keep them in sync when wiring changes.

Durable state = rows in `evolution.db` (`EvolutionStore`, schema v3
`engine_records` table). Process-local state = in-memory `Vec`s/`HashMap`s
on the engine structs, lost on restart. Because the CLI opens a fresh engine
per invocation, process-local state is also invisible to `vikram evolution`
— CLI output reflects only the store.

## Live via runtime (automatic, no operator action)

| Subsystem | What runs | Where |
|---|---|---|
| `experience` | Turn-end experience recording with redaction | `vikram-runtime/src/learning.rs` (`on_turn_end`, called from `turn::finish`) |
| `reflection` | Heuristic reflection per recorded experience; boot-time `recover_and_process` drain | `learning.rs`, `Runtime::start` |
| `memory` | Layered memory writes/reads on the turn path, corrections, and user/project scopes (`user:<seg>` / `project:<slug>` conventions, `runtime/memory.rs`) | `learning.rs`, `runtime.rs` |
| `mapek.monitor` / `mapek.analyze` | Read-only health probes | hourly self-check, `runtime.rs` `run_self_check` |
| `self_improvement` scan + execute | Opportunity scan runs on every 24th hourly self-check tick (`SELF_CHECK_OPPORTUNITY_EVERY`); the top `AnalysisReport` opportunity is then executed for real (deduped by step description); `RequiresChange` opportunities are surfaced as tracked `Completed` executions that honestly say only analysis ran | `runtime.rs` `run_self_check` / `spawn_self_check` (~line 2047) |
| Scheduled evolution cycles | When `config.evolution.auto_cycle` is set (default **off**), the self-check schedule runs one bounded `mapek.run_cycle` per `cycle_interval_hours` on the blocking pool; `auto_benchmark` (also default off) re-runs every recorded benchmark after the cycle. Last-run stamp lives in `engine_records` so the interval survives restarts. Proposals still pass ordinary approval gates — nothing is auto-approved | `runtime.rs` `maybe_run_evolution_cycle` / `run_evolution_cycle_once`; config in `config.rs` `EvolutionConfig` |
| `autonomy` approval producer | The learning pipeline requests an approval whenever a staged skill proposal needs one; requests are durable `autonomy.approval` rows | `vikram-runtime/src/learning.rs` → `autonomy.request_approval` |
| `hive` (`HiveLearning`) | `hive_run` records a per-hive lesson on completion and injects the hive's 3 most recent lessons into participant context | `hive_chat.rs` |
| `conversational` Ask | Free-form text parses to `Ask` and answers from the store; the 14 named commands still map to phrase-matched patterns | `conversational.rs` `ask_freeform`; CLI `vikram evolution ask` |

## Live via `vikram evolution` CLI (read-only)

`vikram evolution status|experiences|proposals|memory|skills|evaluate|mapek|ask|approvals|recover`
(`crates/vikram-cli/src/evolution.rs`) opens the store at
`~/.vikram/evolution.db` and reads it. `dashboard.snapshot`, store listings,
`mapek.monitor`/`analyze`, the `self_improvement` scan, `conversational`
NL parsing (store-backed answers including free-form `Ask`), `autonomy`
pending approvals, and `self_recovery` restart recovery + drift detection
are all reachable.

Read-only except one verb: `vikram learning forget <id> [--dry-run]` deletes a single
layered-memory row by primary key, prints what it removed, and records a `memory_forgotten`
event. The runtime's own memory store is a different database, reached through
`vikram memory list|search|forget|count|snapshot`.

The evolution loop also runs with no session at all: a daemon job created with
`--job-mode maintenance` drains the reflection queue and runs one MAPE-K cycle against
`$VIKRAM_HOME/evolution.db` on its schedule.

## Operator-gated via `vikram evolution` CLI (mutating, confirmed)

`cycle`, `benchmark`, `approve`, `reject` each require `--yes` or an
interactive confirmation before mutating durable state:

| Command | Engine call | Effect |
|---|---|---|
| `cycle` | `mapek.run_cycle` (one bounded cycle) | monitor→analyze→plan→execute; re-queues failed reflection jobs, records issues as proposals |
| `benchmark [name]` | `self_evaluation.run_benchmark` | persists `EvaluationResult` from recorded experiences |
| `approve`/`reject <id>` | `autonomy.approve_request`/`reject_request` | decides a durable approval request (spends it on approve) |

The same `mapek.run_cycle` + optional benchmark sweep also runs on the
self-check timer when `evolution.auto_cycle`/`auto_benchmark` are enabled —
operator-gated is the default, scheduled is opt-in.

## Durable but dormant (state persists, no production caller)

These engines persist their state to `engine_records` and reload on
`EvolutionEngine::open`, and several now do *real* work over the store when
invoked — but nothing schedules or triggers the mutating paths beyond what
the table above lists.

| Subsystem | Persisted (namespace) | Status / gap |
|---|---|---|
| `autonomy` | every `ApprovalRequest` incl. spent state (`autonomy.approval`) | **partially live**: learning pipeline produces requests; `approve`/`reject` decide them. No caller auto-approves |
| `self_evaluation` | benchmarks, results, metric history (`self_evaluation.*`) | benchmarks runnable via CLI and via `auto_benchmark`; no scheduler *creates* new benchmarks |
| `mapek` (mutating half) | `KnowledgeBase` + `cycle_count` (`mapek.state`) | bounded cycle via CLI and via `auto_cycle`; remediation actions are bounded by what the engine owns |
| `self_improvement` | `executions` history (`self_improvement.execution`) | `AnalysisReport` executes for real on the hourly self-check; other action kinds still only produce tracked intent |

## Self-* engines — durable and partially real, still unwired

All five persist their state to `engine_records` and reload on `open`.
None has a production caller in runtime/daemon/CLI; each module header
documents exactly what is real vs. recorded intent.

| Subsystem | Persisted (namespace) | What is real today | Honest gap |
|---|---|---|---|
| `self_awareness` | `SelfModel` (`self_awareness.*`) | `inspect_runtime` persists a real model: store-derived aggregates (experience/proposal/reflection tallies, live memory counts per layer, recorded skill versions, config-shaped signals producers wrote) plus compiled-surface declarations (crates/modules/subsystems this workspace contains) | dormant — nothing calls it; provider status is `Unknown`, not probed; config signals are recorded intent, not verified live config |
| `self_config` | every `ConfigChange` (`self_config.change`) | durable change history across restart | `applied` becomes `true` only when a host attaches a `ConfigSurface` and the write succeeds. The runtime attaches one that accepts exactly `context:memory_budget_tokens`, `runtime:learning_memory_policy`, `runtime:learning_skill_policy` — so the pipeline can change how it learns, never what the assistant is allowed to do. The daemon and the CLI attach nothing, so there every apply stays honest intent. `rollback_change` restores the value read before the write and flips the record back to `applied=false` |
| `self_adaptation` | `AdaptationResult` history (`self_adaptation.result`) | detection reads real store metrics (`recent_performance`); exactly one action is real — `ConsolidateMemory` runs `LayeredMemory::consolidate` over the store; every other action errors with a reason and becomes a persisted `record_issue` proposal | dormant — no caller; `applied` is `true` only for actions actually carried out |
| `self_development` | projects, plans, comparisons (`self_development.project|plan|comparison`) | per-stage honesty over the store: `Research`/`Inspection` report recorded tallies, `Planning` persists a plan artifact, `Implementation` writes real `SkillVersion` rows for skill targets, test stages review recorded experience evidence (no test execution — the crate runs no build tooling), `Comparison` snapshots before/after, `Integration`/`IndependentReview`/`Recording`/`RollbackPath` document the durable record. **Skill projects can reach `Completed`** when recorded evidence supports every gate | dormant — no caller; change types with no store representation (and non-skill `Implementation`) still record `success:false`; executor ceiling = store evidence, not real builds |
| `self_recovery` | snapshots + state fingerprints (`self_recovery.snapshot`) | drift detection live via `vikram evolution recover`; boot recovery drain runs in `Runtime::start` | rollback ops still record intent; in-session op queue stays process-local; real restore (skills/memory/config) is future work gated on snapshot coverage |

`conversational` sits between the tables: handlers answer from the real
store, structured commands map to the 14 named patterns, and any other
text falls through to free-form `Ask` (`ask_freeform`) which queries the
store and answers honestly — including "no match". Commands with no honest
implementation still return `success:false`; the autonomous toggle still
guards nothing real.

## What "wiring" would still require

- **Automatic scheduling depth**: `auto_cycle`/`auto_benchmark` cover the
  MAPE-K cycle and recorded benchmarks, but nothing proposes *new*
  benchmarks or approval requests on a timer.
- **Conversational depth**: free-form `Ask` answers from the store, but
  coverage is still keyword/phrase matching over recorded data — it is not
  a general question-answerer.
- **Self-* activation**: the engines are durable and partially real but
  dormant; giving them callers (e.g. routing `self_adaptation` results into
  proposals, or `self_development` into a supervised pipeline) is the next
  honest step.
- **Self-recovery actions**: drift detection works; actual rollback still
  records intent — wiring real restore is future work gated on snapshot
  coverage.
