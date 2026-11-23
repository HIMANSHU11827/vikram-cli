# Vikram: from tool to assistant — the learning upgrade

Status: draft v1, 2026-11-22. Owner: operator. Supersedes the learning sections of `docs/plans/2026-11-22-jarvis-upgrade-plan.md` once reconciled against the code audit.

## 1. What "JARVIS-like" means here, operationally

Films sell a feeling, so the feeling has to be broken into behaviours that can be tested. An assistant is JARVIS-like when, without being asked each time, it:

1. **Knows the operator.** Remembers stack, conventions, tone, working hours, what they hate, and stops repeating introductions. Greets the same person the same way twice.
2. **Learns from being wrong.** After a correction ("no, not like that"), the correction changes future behaviour in the same project within the same session, not "next model release".
3. **Improves itself measurably.** Proposes edits to its own prompts, skills and routing, proves they help on benchmarks, keeps them only if the score holds, and can undo them.
4. **Gets better while nobody is watching.** Overnight cycles consolidate memory, prune what is stale, and re-rank what it knows.
5. **Is proactive.** It raises the thing that is about to break, and it knows when to stay quiet.
6. **Has a stable personality per entity**, not one global tone, and keeps it under pressure (the personality-consistency harness already exists).
7. **Says what it does not know**, and when it cannot verify something it says so instead of confabulating.

Everything below is machinery for those seven. Adjectives are not acceptance criteria; the numbered tests at the end of each phase are.

## 2. Current reality

> Reconciled against source by four parallel audits. Each row distinguishes *exists and is wired* from *exists but process-local* from *does not exist*.

| Layer | Claimed | Actually wired | Evidence |
|---|---|---|---|
| Turn-end experience capture | yes | **wired** — capture runs from the turn finisher | `learning.rs:643-733` called from `turn::finish` per `evolution-architecture.md:236` |
| Heuristic reflection → proposals | yes | **wired** — `HeuristicReflector` emits `AddMemory` / `UpdateMemory` / `CreateSkill` / `ImproveSkill` / `RecordIssue` | `evolution-architecture.md:73-82` |
| Single approval gate | yes | **wired** — `apply_or_stage` is the one gate every proposal source passes | `evolution-architecture.md:84-90` |
| Verification-grounded confidence | partial | **wired** — `verify` verdict → `TurnGrowth` → `Experience.metadata` → `reusable` → reflection confidence | `evolution-architecture.md:359-362`, `learning.rs:368-496` |
| Hourly self-check (read-only) | yes | **wired** | `runtime.rs:89-94`, `:2012-2110` |
| MAPE-K mutating cycle | "never scheduled" in one doc, "opt-in" in another | **opt-in via `evolution.auto_cycle`** → `maybe_run_evolution_cycle` | `config.rs:444-452`, `runtime.rs:2291-2297` |
| `self_*` engines (8) | "real" | **process-local `Vec`s** — state dies with the daemon | `evolution-architecture.md:249-254` |
| Operator model | "dialectic supplement" | **heuristic, derived per turn from turn text, no durable store** | `evolution-architecture.md:128-134` |
| Relationship model | "missing" per `jarvis-class.md:188` | **built**: `relations`, `relation_facts`, `projects` tables + injected via `known_relations` | `vikram-store/src/lib.rs:4612,4624,4640`; `runtime.rs:1425-1443`; test `runtime.rs:8365` |
| Personality consistency | yes | **built and measurable**: `vikram bench personality`, deterministic scoring | `crates/vikram-bench/src/personality.rs`, `:471` |
| Proactive triggers | yes | **substrate built**: `FileChanged` / `JobSettled` / `Postbox` / `Notify` + `QuietHours` | `daemon.rs:36`, `:559-960` |
| Delegation / routing | "not built" per `CONTINUE.md:30-31` | **built**: `spawn_worker`, role routing, scope confinement, monotonic trust | `runtime/orchestrator.rs` + 7 tests (`:877`…`:1519`) |
| Presence session | design only | **absent** — zero `presence` hits in runtime/store | `always-on.md:39-48` vs code |
| Liveness (`awake`/`dozing`/`asleep`, `/sleep`, `/wake`) | design only | **absent** | `always-on.md:50-61` |
| Semantic verification (diff vs claim) | P0 | **absent** — `verify` reports git changes, judges no intent | `CONTINUE.md:25-27` |
| Policy canary + rollback scored on a held-out set | implied | **absent** | no `policy_trials`-equivalent; promotion is operator-gated only (`jarvis-class.md:183`) |

### 2a. The docs contradict the code in both directions

This matters because it is the reason the work keeps getting re-planned instead of finished. Verified contradictions:

1. `evolution-architecture.md:256-259` says the MAPE-K cycle is **never scheduled**; `config.rs:444` + `runtime.rs:2291` show it is opt-in scheduled.
2. `jarvis-class.md:188` says the relationship model is **missing**; the tables and injection exist.
3. `CONTINUE.md:30-31` says delegation routing is **not built**; `orchestrator.rs` ships it with tests.
4. `CONTINUE.md:60-61` and `jarvis-class.md:184` say `cpal`/`whisper` are **not dependencies**; `vikram-voice/Cargo.toml` has `cpal 0.18` + `wavo 0.6`.
5. Three different "current HEAD" values across `CONTINUE.md:3`, `design.md:33`, and the actual tree (`189e4d7`).
6. Four incompatible memory-scope vocabularies: `memory.md:4`, `evolution.md:13`, `always-on.md:33`, `MEMORY_INTEGRATION.md:37`.
7. "Semantic verification" means three different things across `CONTINUE.md:25`, `design.md:62`, `plan.md:100`.

Consequence: before any phase below is called done, the doc that claims it must be corrected in the same change. A status line is not evidence; the named test is.

### 2c. Code audit — the actual wiring graph

This is the part that changes the plan. Reading all 21 modules of `crates/vikram-evolution/src/` and every caller shows the learning machinery is mostly **built, tested, and connected to nothing**. The tests pass because they call the engines directly; production never does.

There are already **three** durable stores, which is why "where does a memory live" has no single answer today:

| Store | Owner | Holds |
|---|---|---|
| `evolution.db` (schema v3) | `EvolutionStore` | experiences, layered memory, proposals, reflection jobs, hive lessons, candidates, evaluations, skill versions, `engine_records` |
| `memory.db` | `MemoryStore` (`vikram-runtime`) | FTS5 memory, session scopes, dialectic, recency decay |
| `learning.json` | `LearningManager` (`vikram-runtime/src/learning.rs:10-21`) | staged proposals + audit only |

What is genuinely alive:

- **experience capture** — `experience.rs:24`, redacts secrets, injection-scans, fail-closed, called every tool-bearing/errored/failed/corrected turn (`learning.rs:813`).
- **heuristic reflection** — `reflection.rs:32`, emits proposals, called at `learning.rs:817`.
- **model-driven turn review** — `spawn_learning_review` (`runtime.rs:2634`, prompt `learning.rs:339`): bounded 12,000-char transcript, model answers what is worth keeping, routed through `apply_or_stage`. This is Hermes' post-turn nudge, already built.
- **dialectic user model** — `dialectic.rs:715`: heuristic insights per turn, staleness decay, and repeated corrections (`correction_promote_at` = 3) promoted into a durable `memories/USER.md`. Deterministic, no approval.
- **runtime memory consolidation + recency decay** — `memory.rs:637` and `:911-973`, 7-day half-life fused into recall scoring, ranking only, never deletes.
- **hive lessons** — `hive.rs:24` recorded per hive run, injected into participants next run (`runtime.rs:5758`).
- **benchmark scoring over recorded work** — `self_evaluation.rs:179` computes real aggregates, wired to `vikram evolution benchmark` and the opt-in scheduled `auto_benchmark`.

What is dead in production (built, tested, zero callers):

| Symbol | Where | Consequence |
|---|---|---|
| `ReflectionEngine::enqueue` | `reflection.rs:144` | `reflection_jobs` is never filled, so the boot drain (`runtime.rs:3208`), the retention prune, **and MAPE-K's only real actuator** (`mapek.rs:1232`) all act on an empty queue |
| `SkillEvolution::{create_skill,improve_skill,rollback}` | `skills.rs:113,149,185` | `skill_versions` and `.versions/vN.md` are never written in production; live skill writes go through `vikram_skills::Manager` with its own history |
| `OptimizationEngine::*` | `optimization.rs:18-158` | `candidates` / `evaluations` populated only by tests — the GEPA-style Pareto machinery exists and is unused |
| `SelfAwarenessEngine::inspect_runtime` | `self_awareness.rs:310` | its "architecture snapshot" is a hardcoded `vec![...]` of 5 crates with `modules: vec![]` — not introspection |
| `SelfConfigurationEngine::{apply_change,rollback_change}` | `self_config.rs:321,362` | both **always return `Err`** ("no live configuration surface"); it is a recorder that refuses |
| `SelfDevelopmentEngine::integrate_change` | `self_development.rs:350` | always errors; rollback only flips a status field |
| `SelfEvaluationEngine::track_metric` | `self_evaluation.rs:139` | never called, so `metrics_history` stays under 2 entries and `detect_regression` always answers "no regression" |
| `LayeredMemory::forget` | `memory.rs:164` | unreachable; evolution memories are superseded, never deleted |
| `Store::prune_journal` / `prune_retention` | `vikram-store/src/lib.rs:4209,4186` | no timer, no command — journal pruning is bench-only |
| `SelfRecoveryEngine` recovery ops | `self_recovery.rs:131-133` | `Vec`s never hydrated; `recover_after_restart` honestly returns `[]` |

Also true, and relevant to the design:

- `vikram-daemon` touches evolution **not at all** — its `EventBus` carries job triggers only (`Postbox`/`FileChanged`/`JobSettled`/`Notify`, `triggers.rs:33-43`). No cron, no cycle, no self-check.
- The same correction can be marked **applied** in `evolution.db` (`reflection.rs:204`) and simultaneously **staged** in `learning.json` (`learning.rs:1460`). Two gates, two verdicts, one event.
- `vikram learning` supports `approve`/`reject` and **no un-apply**.
- There is one learning policy set (`LearningState.policy_memory/skill/instruction/identity`, `learning.rs:92-105`) for all entities — per-agent learning state does not exist.
- No scheduled forgetting of individual memories exists anywhere.

The sting: MAPE-K's actuator is "requeue failed reflection jobs", and the queue is permanently empty. The self-improvement loop is a wheel whose only input is a list nobody writes to. Wiring beats building here, which is why §6 changed.

## 2b. "Hermes-like" — what the reference actually does

Verified against the Hermes Agent docs (`hermes-agent.nousresearch.com/docs`, fetched 2026-11-22) and the papers that predate it. The important correction up front: **the Hermes models learn nothing at runtime** (Hermes 3→4 is a bigger training corpus, per arXiv 2508.18255). The learning lives entirely in the harness, as text state + SQLite. Nothing below needs a fine-tune or a cloud service.

What Hermes Agent ships, mechanism by mechanism:

| Mechanism | How Hermes does it | Vikram today | Decision |
|---|---|---|---|
| Bounded curated memory | two files: `MEMORY.md` ~2,200 chars (environment, conventions) and `USER.md` ~1,375 chars (preferences, style), rendered into the system prompt as a **frozen snapshot at session start**; writes via a `memory` tool with `add`/`patch`/`remove`; an over-limit write **errors** and forces consolidation in the same turn | nothing durable — the "dialectic supplement" is recomputed from the turn text each time | adopt: two char-budgeted stores + frozen snapshot + overflow error |
| Post-turn review ("nudge") | a forked agent replays the turn and answers only "did anything durable get learned, corrected, or solved non-trivially?", then writes memory or a skill; deferred to idle on a local GPU; routable to a cheap model | `HeuristicReflector` at turn end (`learning.rs:643-733`) — deterministic, no model judgement | adopt: model-backed review behind a config flag, heuristic stays as fallback |
| Skills = procedural memory | `~/.hermes/skills/<slug>/SKILL.md`, progressive disclosure (`skills_list` ≈3k tokens → `skill_view(name)` → reference files on demand), linter penalties for sprawl and oversized bodies | `vikram-skills` crate + `apply_or_stage` approval exists | adopt: progressive disclosure + linter rules |
| Curator (forgetting) | deterministic `active → stale (14d) → archived (30d)` on use counts, 7-day interval, JSONL ledger with before/after sha256, single-entry rollback, never hard-deletes | consolidation lifecycle staged→active→superseded→expired exists in the store | adopt: decay windows + ledger on top of the existing lifecycle |
| Scored retrieval | (Generative Agents, not Hermes) `score = recency + importance + relevance`, recency `0.995^hours`, importance rated 1-10 at write, relevance cosine | recall is scope-filtered, unscored | adopt the formula, no embeddings in P1 |
| Explicit user model | `USER.md` curated by the agent; deeper modelling is the optional Honcho plugin (server-side dialectic) | per-turn heuristic, nothing persisted with provenance | adopt facts with `source_turn_ids` + confidence, skip Honcho |
| Persona vs rules | `SOUL.md` owns identity at slot #1, `AGENTS.md` owns project rules, `/personality` overlays are session-only | entity definitions + `$VIKRAM_HOME/agents/<name>/` | map onto entity persona; agent never writes its own persona |
| Write gate | `memory.write_approval` stages agent writes for approval; provenance on every artifact | `apply_or_stage` is the same shape | reuse it — do not build a second gate |

Honest framing: this is not weight-level learning. It is a **write → decide → retrieve → dispose loop over text**, which is what Reflexion (arXiv 2303.11366), Generative Agents (2304.03442) and Voyager (2305.16291) demonstrated, and what Hermes packaged. Vikram already owns most of the parts; the audit says what is missing is durability and a scored gate, not machinery.

The failure modes come free with the pattern, so each one is guarded in §7: stale facts asserted as current, reflections compounding until unfalsifiable, a poisoned README becoming permanent doctrine, and a write gate so annoying that nobody approves and the agent never learns.



## 3. Architecture — eight layers

Names are chosen to fit existing vocabulary (`self_*` engines, `entities` table, `$VIKRAM_HOME`, StagePoints, HookPoints, EventBus, hive, memory scopes).

### L1 — Episode store (the raw material)
Every turn is already an event stream. Persist it as *episodes*: one goal, N steps, one outcome, plus operator interventions (correction, interrupt, approval, rejection). Nothing is learned until episodes are durable and addressable.

- Written from the existing turn pipeline through a HookPoint, not new plumbing.
- Cheap: summary text per step, no full transcripts (the journal already owns those).
- Retention: episodes roll up after 30 days into a lesson digest; the rollup is the thing kept forever.

### L2 — Reflection worker
Two cadences, deliberately different:
- **After a correction or a failed turn** (immediate, single episode): "what did the operator expect that I did not do?"
- **Nightly** (window): cluster recent episodes, extract lessons, detect repeats ("this is the fourth time it forgot cargo fmt").

Two engines behind the same trait, exactly as Hermes does it:
- `HeuristicReflector` — already exists, always on, no model call, catches mechanical repeats.
- model-backed review — a forked agent replayed the turn and answers only one question: *did anything durable get learned, corrected, or solved non-trivially?* Deferred to idle, routable to a cheap model, and off by default until the gate in L4 can score it.

Output is *lessons*, never prose logs: each lesson has scope (user/project/hive/entity), a statement, evidence episode ids, and a confidence. A lesson with no evidence ids is rejected at insert time.

### L3 — Operator model
Facts and traits about the operator with provenance, in **two char-budgeted stores** (adopted from Hermes, which separates `MEMORY.md` from `USER.md` for a reason: environment truth churns, the person does not):

- **Facts** (`operator_facts`): stack, paths, conventions, names, do-not-do list. High confidence, no decay, superseded not deleted. Budgeted ~2,000 chars.
- **Traits** (`operator_traits`): tone, verbosity, pace, risk appetite. Low confidence, decayed, updated from evidence counts. Budgeted ~1,200 chars.
- A write that would exceed a budget **fails loudly and forces consolidation in the same turn** — silently dropping the oldest line is how a memory model starts lying.
- **Frozen snapshot**: the block is rendered once at session start and stays fixed for that session. Writes land in the store and become visible next session (or on explicit `/memory refresh`). This is the single cheapest defence against mid-conversation drift, and it is what Hermes does.
- Every injected line carries an id so the operator can say "forget that" and it actually disappears.

This is the difference between personalization and flattery: a fact is only stored if it came from an episode, and it is only used if it is still true.

### L4 — Policy self-edit (the self-improving part)
The only layer that may change the assistant's own instructions. Constrained on purpose:

- Targets: prompt fragments, skill descriptions, routing hints, retrieval weights. **Never** permission policy, the mandatory security floor, or anything under `docs/security.md`.
- Flow: `propose → benchmark → canary → promote | rollback`, every step recorded.
- A proposal carries before/after text, the lesson that motivated it, and the benchmark set it will be judged on.
- Promotion requires no regression on the benchmark suite. Canary means it runs for N turns with the old path still available.

Without the rollback half this layer is a liability, so rollback ships in the same phase as promotion.

### L5 — Evaluation
The existing bench crate is the referee. Needs three things to be usable as a gate:
- a **held-out** set the proposer never saw,
- deterministic scoring (pass/fail per task, not vibes),
- a recorded baseline per policy target, so "did this help" is a subtraction, not an opinion.

### L6 — Retrieval
What actually reaches the prompt each turn, in priority order:
1. entity persona (always),
2. operator facts relevant to the task (ranked),
3. lessons in scope (ranked, capped),
4. episodic recall (only when the current goal matches an old episode),
5. skills matching the task — **by index first**: a one-line-per-skill index in the prompt, full skill bodies loaded on demand (`skill_view`), because a growing skill library that is always fully in context is a context leak with extra steps.

Ranking uses the Generative Agents formula rather than a hand-tuned heuristic:
`score = recency + importance + relevance`, where recency = `0.995^hours_since_use`, importance is rated 1-10 **at write time** (deterministic, stored, never re-guessed), and relevance is lexical overlap in P1 (embeddings are a later, optional upgrade). Storing importance at write time is what makes reflection cheap later: sum the importance of the window and only synthesize when it crosses a threshold.

Budgeted per section, not globally, so one bloated category cannot starve the others. Every recalled item is logged (`recall_log`) so a bad memory can be traced to the turn it poisoned.

### L7 — Forgetting (the curator)
A memory that only grows becomes a liability, so retirement is deterministic and auditable, not a model decision:
- `active → stale (unused 14d) → archived (unused 30d)`; archived items leave the prompt but stay queryable.
- Nothing is ever hard-deleted by the curator; `forget` is a separate operator action, and it is the only destructive path.
- Every transition lands in an append-only ledger with before/after content hashes, and a single transition can be rolled back by id.
- Runs on a slow interval (daily, plus when the session has been idle a while), never inside a turn.

### L8 — Governance
Non-negotiable, and the reason the rest is safe to enable:
- deny-first permissions unchanged; nothing here widens the floor
- provenance on every stored line (episode → lesson → fact → injected prompt)
- poisoning defence: lessons sourced only from operator or tool-verified evidence, never from model self-praise
- secrets never enter lessons; the redaction pass runs before insert, not after
- full audit: who proposed, what changed, what scored, who promoted
- a single `vikram learn forget --everything` that provably leaves no residue

## 4. Data model (SQLite, `crates/vikram-store`)

Schema sketch; final column names follow existing store conventions.

```sql
episodes(id, session_id, entity_id, profile_id, goal, outcome, importance, started_at, ended_at, steps, corrections)
episode_steps(id, episode_id, idx, kind, summary, tool, ok, latency_ms)
lessons(id, episode_id, scope, kind, statement, evidence_json, confidence, importance, created_at, last_used_at, superseded_by)
operator_facts(id, profile_id, key, value, source_lesson, confidence, first_seen, last_seen, evidence_count, status)
operator_traits(id, profile_id, trait, value, confidence, evidence_count, decay_at, updated_at)
policies(id, kind, target, before, after, rationale, status, score_before, score_after, approved_by, created_at)
policy_trials(id, policy_id, benchmark, baseline, candidate, outcome, ran_at)
recall_log(id, turn_id, memory_kind, memory_id, score, injected)
reflection_runs(id, window_start, window_end, episodes, lessons, tokens, model, ok)
curator_ledger(id, memory_kind, memory_id, from_state, to_state, before_hash, after_hash, at, reverted_by)
```

Indices: `lessons(scope, confidence)`, `lessons(last_used_at)`, `operator_facts(profile_id, status)`, `episodes(ended_at, importance)`.

## 5. Wiring

| Concern | Where it attaches |
|---|---|
| Episode capture | existing turn StagePoint/HookPoint on turn end |
| Correction capture | the UI path that already records interrupts and rejections |
| Immediate reflection | after a correction, one background task, off the hot path |
| Nightly reflection | daemon schedule → EventBus topic `reflection.cycle` |
| Promotion/rollback | existing proposal/approval UI — no second approval mechanism |
| Retrieval | the context assembly seam, per-section budgets |
| Surfaces | `vikram learn status|lessons|forget`, TUI `/memory`, `/learn` |
| Voice | spoken summary only for promoted changes; never reads lessons aloud unprompted |

No new permission system, no new event bus, no new approval flow. The audit's job is to name the exact existing symbol at each row; this doc names the seam.

## 6. Phases and acceptance criteria

Each phase ends with a runnable test, named here, so "done" is not a feeling.

### P0 — Durability and episodes (the reason it forgets)
- episodic store + step summaries persisted
- evolution/reflection state survives a daemon restart
- `vikram learn status` shows counts that match the database
**Acceptance:** `a_restarted_daemon_still_knows_its_last_reflection` and `episodes_from_a_session_are_queryable_by_goal`.

### P1 — Operator model, immediate reflection
- correction → immediate reflection → lesson → applied in the same session
- operator facts extracted with provenance, injected under budget, frozen for the session
- an over-budget write fails loudly and forces consolidation instead of evicting silently
- `/memory` lists what is stored, with the episode it came from
**Acceptance:** `a_correction_changes_the_next_turn_in_the_same_session`, `an_injected_fact_traces_back_to_its_episode`, `an_over_budget_write_is_refused_rather_than_truncated`.

### P2 — Policy self-edit with a real gate
- propose/canary/promote/rollback, benchmark-scored, held-out set
- regression blocks promotion automatically
**Acceptance:** `a_proposal_that_scores_worse_is_rolled_back` and `a_promoted_prompt_change_is_undone_by_one_command`.

### P3 — Proactivity and persona
- nightly consolidation, pruning, trait decay
- the curator: `active → stale → archived`, ledger with hashes, one-transition rollback
- skills load by index, bodies on demand, with a linter that rejects sprawl
- per-entity persona evolution, consistency preserved
- proactive turns gated by the existing trigger rules and a quiet-hours setting
**Acceptance:** `a_stale_trait_decays_out_of_the_injected_prompt`, `an_overnight_cycle_shortens_memory_without_losing_a_lesson`, `a_curator_transition_can_be_rolled_back_by_id`.

## 7. Failure modes this design has to survive

| Failure | Guard |
|---|---|
| Memory poisoning (model writes its own praise as fact) | provenance rule: operator/tool evidence only |
| Sycophancy drift (persona evolves into flattery) | personality-consistency harness as a gate, not a report |
| Context bloat | per-section budgets, lessons capped |
| Stale facts asserted as current | confidence decay + `last_seen`, surfaced as stale rather than deleted |
| Self-edit loop that degrades quietly | held-out benchmark + canary + rollback |
| Secrets leaking into memory | redaction before insert, test asserts no secret reaches a lesson |
| Operator loses trust in what it "knows" | every injected line addressable and forgettable |

## 8. Non-goals

- no fine-tuning, no cloud dependency for learning
- no hidden always-on microphone (see `2026-11-22-voice-upgrade.md` §5)
- no second approval mechanism, no second event bus, no second memory store
- no personality that changes to please
- no learning from other operators' data outside authorised hive context

## 9. Build order

P0, then P1, then P2, then P3 — in that order, because each later phase's evidence comes from the previous one's store. Attempting P2 before P0 reproduces today's situation: a loop that runs and then forgets.

## 10. Status 2026-11-23 — what is wired now

The P0/P1 wiring this plan called for is done and covered by tests. Every item below was dead,
unreachable, or contradicted by the code before that day.

| Capability | Where it lives now | Test |
|---|---|---|
| Operator memory files reach the prompt | `memory_snapshot()` called from `assemble_context` (`turn.rs`) | `a_note_written_this_turn_reaches_the_next_prompt` |
| A note taken mid-session is honoured next turn | `commit_tool_results` → `refresh_memory_snapshot` | above |
| Session search finds message text | `RuntimeInner::journal` mirrors `RecordKind::Message` into `session_fts` | `a_committed_message_becomes_searchable_by_its_text` |
| A fold leaves a cross-session recap | `journal_compaction` → `remember_session_recap` (600 chars, `global`) | `a_fold_leaves_a_recap_that_outlives_the_session` |
| Reflection jobs are actually queued and closed | `reflect_now` from `record_and_reflect`; drain no longer applies memory on its own | `reflection_run_on_the_turn_path_closes_its_job`, `a_drain_records_its_proposals_without_writing_memory` |
| The operator's policy is the only gate | `writes_without_approval` (only `Auto`) | `a_correction_is_written_on_auto_and_only_staged_on_approval` |
| One decision per proposal | `stage_proposal` replaces a pending row with the same id | `a_growing_approval_queue_is_announced_once_per_proposal` |
| The queue is visible | one `RuntimeEvent::Notice` per newly staged proposal | same |
| An approval lands where recall looks | `MemoryStore::session_scopes` used by `vikram learning approve` | `a_session_reports_the_shelves_it_was_bound_to` |
| The first turn of a session reads its shelves | `bind_session_memory_scopes` before every recall | `the_first_turn_of_a_new_session_already_reads_its_project_shelf` |
| The user model is per account | `scope_for_identity`, `persist_for_identity`, legacy migration | `one_accounts_user_model_is_never_loaded_for_another` |
| Benchmarks build a real time series | `run_benchmark` → `track_metric` | `a_run_benchmark_records_the_metrics_it_measured` |
| Retention actually prunes | hourly self-check → `prune_retention` + `prune_journal` | covered by store tests |
| Self-awareness reads the real workspace | `discover_workspace` in `self_awareness.rs` | 6 module tests, 17 members found |
| Recovery state survives a restart | `self_recovery` persists into `engine_records` | 9 module tests |
| Learning runs unattended | daemon `--job-mode maintenance` drains reflection + one MAPE-K cycle | `a_second_maintenance_run_advances_the_durable_cycle_count` |
| A forgotten memory is really gone | `vikram learning forget <id>`, `vikram memory forget <key>` | 2 CLI tests + evolution round-trip |
| The operator can see what is known | `vikram memory list\|search\|count\|snapshot` | 9 `memory_cmd` tests |
| A persona runs on the model it declares | `default_model()` at session creation | `the_selected_persona_runs_on_the_model_it_declares` |
| A persona carries its own skills | `SkillRegistry::for_workspace_and_persona` + `active_persona_skills` | `a_persona_skill_is_offered_to_its_own_turns` |
| Learning may change three settings, nothing else | `ConfigSurface` trait, `RuntimeConfigSurface` allowlist attached at boot | 5 `config_surface` tests + 8 `self_config` tests |
| What stopped being used sinks out of the prompt | `Curator::sweep` from the hourly self-check and the daemon maintenance run | `a_sweep_ages_an_unused_entry_and_rollback_undoes_it` |
| Every ageing step is undoable | `vikram evolution curator [--sweep] [--rollback <id>]`, ledger in `engine_records` | same |
| A skill that could never load is refused | `vikram_skills::lint` from `Manager::validate` and `skill_gate` | `a_skill_body_too_long_to_ever_load_is_refused_before_it_is_written` |

Still open, in value order: semantic verification of the assistant's claims against the diff (the
diff-vs-declared-change half already exists in `tools/verify.rs`), presence and liveness (JARVIS
Phase A/B), `entities.profile_id` which is still declared and unread, and the dead scaffolding in
`SkillEvolution`/`OptimizationEngine` that duplicates `vikram_skills::Manager`.
