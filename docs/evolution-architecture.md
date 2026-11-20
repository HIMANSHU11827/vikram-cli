# Evolution Architecture — What Is Actually Built

This document describes the self-improvement architecture as it exists in the
tree at commit `3d69c5e`. Every claim cites a file. Where something is a stub,
process-local, or unscheduled, it says so.

The earlier design vision this implements — the planned `self_*` engines and
MAPE-K loop — is preserved in [`evolution-design.md`](evolution-design.md).
This document is the "what is actually built" counterpart: several engines the
design promised are implemented but process-local or read-only (see §3).

---

## 1. The learning loop, end to end

### 1.1 Capture: `TurnGrowth`

The turn loop (`crates/vikram-runtime/src/turn.rs`) accumulates a
`TurnGrowth` record while tools run (`turn.rs:204`, `:1300`, `:2295`):

```rust
pub struct TurnGrowth {
    pub tools: Vec<String>,
    pub errors: Vec<String>,
    pub verification: Option<Verification>,   // learning.rs:384-389
}
```

- `record_call` / `record_failure` dedupe tool names and bound error notes to
  200 chars (`learning.rs:392-413`).
- `record_verification` (`learning.rs:422-445`) parses the `verify` tool's
  `verify: PASS` / `verify: FAIL step=… kind=…` verdict line (falling back to
  the JSON report's `ok`). The recorded verdict is `ok && reported` — a plugin
  that flips the call's `ok` cannot make a failed verification pass.

### 1.2 Trigger: `on_turn_end`

`learning::on_turn_end` (`learning.rs:643-684`) runs from `turn::finish` on
every non-cancelled turn:

1. Both learning policies `Disabled` → return early (`:655-658`).
2. `note_turn` increments a process-local per-session counter (`:591-596` —
   explicitly *not persisted*; it bounds this run's spend only).
3. `detect_corrections(request)` — regex-level phrase matching
   (`learning.rs:919-964`: `CORRECTION_STARTS`, `CORRECTION_ANYWHERE`). A
   heuristic, not a classifier; misses rephrased corrections, can false-positive
   on "instead".
4. `record_and_reflect` runs when the turn had tools, errors, failed, or
   contained a detected correction (`:662-667`).
5. The paid half — a background model review — is gated by
   `review_is_due` (`:602-604`): tool-bearing turn at
   `config.learning_review_every` interval, or a memory-nudge. The transcript
   snapshot is budgeted at `REVIEW_SNAPSHOT_BUDGET = 12_000` chars (`:633`,
   `:607-624`) and handed to `spawn_learning_review`.

### 1.3 Record + reflect: `record_and_reflect`

`learning.rs:686-733` is the free half of the pipeline, synchronous on the
turn-end path:

1. Build a `vikram_evolution::Experience` — agent id from the session's bot or
   `"main"`, task category = first tool used, outcome via
   `experience_outcome` (failed turn → `Failure`; errors present → `Partial`;
   else `Success`, `:756-768`), `reusable` via `experience_reusable`
   (`:774-780`: ≥2 tools, *or* any tool + a passed `verify` — a green verdict
   is stronger evidence than a second tool call).
2. `engine.experience.record(experience)` — `ExperienceCollector::record`
   (`vikram-evolution/src/experience.rs:24-60`) redacts secrets and runs an
   injection scan on every field; a hit anywhere rejects the whole record
   (fail-closed). `Sensitivity::Secret` records collapse to a redacted stub.
   The verification verdict rides in `metadata` (`learning.rs:737-751`) since
   `Experience` has no dedicated field.
3. `engine.reflection.run_reflection(&recorded.id, auto_memory)` — the
   `HeuristicReflector` (`vikram-evolution/src/reflection.rs:29-91`) produces
   `LearningProposal`s with **no model in the loop**:
   - user corrections → `AddMemory` on the `user` layer (confidence 0.8),
   - failures → `RecordIssue` (0.7),
   - reusable clean success → `CreateSkill` (0.6, `requires_approval: true`).
4. Each evolution proposal is mapped through `proposal_from_evolution`
   (`learning.rs:501-564`) — only `AddMemory`/`UpdateMemory`/`CreateSkill`/
   `ImproveSkill` map onto the approval surface; other kinds stay in the
   evolution store. Then `apply_or_stage`.

### 1.4 Gate: `apply_or_stage`

`learning.rs:785-861` is the single gate every proposal source passes through:

- Kind → policy: `memory` → `policy_memory`, `skill` → `policy_skill`; any
  other kind is audited `skipped` — "a decision that applies nothing when
  approved is worse than no decision" (`:794-801`).
- `Disabled` → skip; memory proposals are dedup-checked against existing
  entries first (`:807-830`).
- `Auto` → apply now; `ApprovalRequired`/`SuggestOnly` → `stage_proposal` into
  `learning.json` (`:831-837`). Every outcome is written to the audit log.
- Memory writes go through `inner.memory.save` scoped by `scope_of`
  (`learning.rs:866-869`) — always the *session id*, because automatic recall
  only reads `global` + session scope and a literal `user`/`memory` scope would
  be invisible to recall and readable by every session.
- Skill proposals go through `apply_skill_proposal` (`:876-915`) →
  `vikram_skills::Manager::create`/`edit`, honoring `patch`/`improve`/`replace`
  (patch on a missing skill falls back to create). `Manager::edit`
  (`vikram-skills/src/manager.rs:125`) preserves frontmatter and records
  history — `History::rollback` (`history.rs:148`) restores prior snapshots.

The model-driven review produces the same `LearningProposal` shape and goes
through the same gate — there is no second, less-supervised path.

### 1.5 Recall: `retrieved_memory`

`RuntimeInner::retrieved_memory` (`runtime.rs:1230-1348`) assembles the memory
block injected each turn, inside `context.memory_budget_tokens`:

1. **Base store** — `MemoryStore::hybrid_recall` (BM25 FTS5 + token overlap +
   recency/importance, MMR-reranked; no embeddings — noted honestly at
   `:1256-1258`) over scopes `["global", session.id]`.
2. **MEMORY.md / USER.md** — free-form entries from
   `~/.vikram/memories/` (`memory_files.rs:131-135`, budgets 8 000/4 000 chars)
   matched by term containment.
3. **Evolution layered memory** — `engine.memory.recall` under
   `agent:<id>` scope (`runtime.rs:1290-1310`), deduplicated against already
   injected values.
4. **Recent experiences** — last 3 outcomes for the agent as compact lines
   (`recent_experiences`, `runtime.rs:1354-1380`).

Header text warns the model the notes "may be stale, verify before relying on
it" (`:1315`).

### 1.6 Layer 2: dialectic supplement

`dialectic.rs` is a deterministic, local, heuristic user model derived from
turn text (`observe_dialectic_turn`, `runtime.rs:1218-1224`; `persist_to` into
the memory store). `retrieved_memory` is layer 1; `dialectic_supplement`
(`runtime.rs:1210-1215`) is layer 2 — budgeted and labelled separately so the
context meter can price them (`turn.rs:2439`). No model tokens spent.

---

## 2. Platform surfaces

### 2.1 Stage pipeline — 10 `StagePoint`s

`pipeline.rs:53-102` defines the replaceable steps of a turn:

`UserInput`, `ContextBuild`, `MemoryRecall`, `ProviderRequest`, `ToolDispatch`,
`ResultCommit`, `ResponseCommit`, `TurnEnd`, `SessionLifecycle`,
`AgentLifecycle`.

`StageRegistry::run` dispatches each point to exactly one handler;
`ProcessStageHandler` (`pipeline.rs:430-520`) lets a **process plugin** act as
a stage: the builtin default runs first (a subprocess cannot assemble context
or dispatch tools), then the plugin's entry is spawned with the stage point as
`argv[1]` and a hook-shaped JSON payload on stdin. The reply is a `HookEffect`
— `deny` refuses the stage, other fields amend the payload. Spawn failure,
non-zero exit, timeout, or malformed reply is a `StageError`: **fail-stage,
never silent fallback** (`pipeline.rs:10-11`).

Plugins declare stages via `components.stages` in the manifest
(`plugins/manifest.rs:117-127`, `declared_stages`); the runtime rebuilds them
through `reload_plugin_stages` (`runtime.rs:712-717`, called at boot `:2630`
and on plugin reload `:4609`). Each stage maps to a `HookPoint` for permission
gating (`pipeline.rs:128-139`).

### 2.2 Hooks — 14 `HookPoint`s

`hooks.rs:81-95`: `SessionStart`, `UserInput`, `TurnStart`, `ContextBuild`,
`MemoryRecall`, `ModelRequest`, `ToolCall`, `MemoryWrite`, `ToolResult`,
`ResponseDelivery`, `TurnEnd`, `DelegationStart`, `DelegationEnd`,
`SessionEnd`. Declarative + process hooks, permission-gated, payload-capped,
panic-isolated. Hooks load only from installed plugins
(`runtime.rs:2336-2337`).

### 2.3 Provider / memory seams

- `Provider` trait (`provider.rs`) — model access is a trait; config selects
  the implementation.
- `MemoryProvider` trait + `MemoryProviderManager`
  (`memory_provider.rs:8`, `:164-243`) — pluggable memory backends;
  `config.memory_provider` names the backend (`"builtin"` ships;
  `add_external`/`register` accept others). `BuiltInMemoryProvider` wraps
  `MemoryStore` plus MEMORY.md/USER.md files.
- `Tool` trait (`tools/core.rs`) — tools are objects; `verify` is one of them
  and is the verification signal the learning loop consumes.

### 2.4 Transforms

Plugin manifests may declare `transforms` — file ops applied explicitly via
`vikram transforms apply`, tracked in `~/.vikram/transforms.json`, reverted via
`vikram transforms rollback` (`transforms.rs:1-11`, `manifest.rs:44-47`). v1
writes only under `skills/`; ownership is deny-first (a transform may create
or replace only paths it already owns). This is Vikram's file-level
patch/rollback mechanism — distinct from skill versioning.

### 2.5 Presets

`vikram presets` (`presets.rs`) — named JSON bundles under
`~/.vikram/presets/<name>.json` holding a strictly-validated subset of
`Config` (model/provider, context budgets, memory provider, posture,
`context.*`/`search.*` leaf-by-leaf). `security` and `theme` are deliberately
not presettable — a preset must never widen restrictions. `presets suggest`
is **read-only**: it scans `evolution.db` experiences/proposals and
`learning.json` for config-shaped keys and prints what it saw; it never writes
a preset (`presets.rs:17-19`).

### 2.6 Distributions

`vikram dist` (`dist.rs:1-16`) — export/import a `.tar.gz` bundle
(`MANIFEST.json` + SHA-256-verified copies) of operator-editable state:
`config.json`, `gateway.json`, `transforms.json`, installed plugins, user
skills with `.vikram` state, entity context dirs. Never contains `.env`, the
store DB, sessions, pairing state, or logs; `config.json` is scanned for
secret keys before writing. Import verifies every hash before touching the
live home and installs each kind through its own command's path.

### 2.7 CLI surfaces

- `vikram evolution` (`evolution.rs`) — **read-only** inspection of
  `evolution.db`: status, experiences, proposals, layered memory, skill
  versions, self-* analyses. "Approvals and every other mutation stay in
  `vikram learning`."
- `vikram learning` — the approval surface: pending proposals, per-kind
  policies, approve/reject (`main.rs:191-196`, handler `:2173`).
- `vikram bench learning` (`bench.rs`, `vikram-bench/src/learning.rs`) —
  deterministic offline benchmarks over the real stores: recall hit rates,
  ordering, version/rollback state, proposal statuses, consolidation
  dedup/supersession. `--baseline`/`--save-baseline` for regression
  comparison. No network, no provider credentials.

---

## 3. What's real vs. what's limited

### Real and wired

| Component | Evidence |
|---|---|
| Turn-end experience recording + heuristic reflection | `learning.rs:643-733`, called from `turn::finish` |
| Approval-gated memory/skill mutation | `learning.rs:785-915` |
| Model-driven background review | `spawn_learning_review`, gated by `review_is_due` |
| Skill versioning + rollback | `vikram-evolution/src/skills.rs:185-227`, `vikram-skills/src/history.rs:148` |
| Memory consolidation lifecycle | `consolidation.rs` — staged→active→superseded→expired, never silent-delete |
| Hourly self-check (read-only) | `runtime.rs:89-94`, `:2012-2110` — `mapek.monitor()`/`analyze()` + opportunity scan |
| Boot-time recovery drain | `recover_and_process` at `runtime.rs:2747` |
| Stage pipeline + process-plugin handlers | `pipeline.rs`, `runtime.rs:712-717` |
| 14 hook points | `hooks.rs:81-95` |
| Verification-grounded learning | `learning.rs:368-496`, `:737-780` |

### Process-local / unscheduled / dead ends

- **`self_*` engines hold in-memory `Vec`s** — `self_improvement.rs:99-100`
  (`opportunities`, `executions`) and siblings keep state in process memory;
  only store-backed analyses persist. The engine docs state it plainly: "it is
  not `Sync` for the stateful `self_*` engines … the runtime keeps the whole
  engine behind a `Mutex`" (`vikram-evolution/src/lib.rs:22-26`). A restart
  loses pending opportunities/executions.
- **MAPE-K mutating cycle is never scheduled.** The hourly self-check runs
  only `monitor()` + `analyze()` (read-only probes) and the read-only
  `identify_improvement_opportunities()` scan. `plan`/`execute`/`run_cycle`
  "stays operator-driven" (`runtime.rs:89-94`, `:2042-2045`). Nothing calls
  them on a timer.
- **`recover_and_process` drains only on boot** — "queued jobs still wait for
  an explicit caller between boots" (`lib.rs:28-33`). Turn-end reflection is
  synchronous, not queued.
- **Review cadence counters are process-local** (`learning.rs:588-596`) —
  restart resets the `learning_review_every` interval.
- **Weight-level evolution is out of scope.** `optimization.rs` scores
  outcomes (success+correctness weighting) but nothing mutates model weights,
  prompts-as-parameters beyond skills, or provider configs. Evolution targets
  *artifacts* (memories, skills), not the model.
- **Correction detection is regex-level** (`learning.rs:919-964`) — documented
  as "a heuristic, not a classifier."
- **`evolution.db` failure degrades silently-ish** — the runtime falls back to
  `open_in_memory` with a stderr warning (`runtime.rs:2348-2368`); evolution
  history is lost for that run.
- **`proposals suggest` has no producer keys yet** — the preset suggester
  scans for "config-shaped keys" but there is no structured producer that
  emits them; it prints raw observations (`presets.rs:17-19`).

---

## 4. Data flow

```
user request
   │
   ▼
turn loop (turn.rs) ── TurnGrowth { tools, errors, verification }
   │                        │ tool_call / tool_result hooks (deny-first)
   │                        ▼
   │                  stages: UserInput→ContextBuild→MemoryRecall→
   │                          ProviderRequest→ToolDispatch→ResultCommit→
   │                          ResponseCommit→TurnEnd→(Lifecycle)
   │                        ▲
   │                  process plugins (components.stages) amend/deny
   │
   ├─ context build ◄── retrieved_memory() ─┬─ MemoryStore.hybrid_recall
   │                                       │   (FTS5+overlap+MMR, scopes
   │                                       │    global + session.id)
   │                                       ├─ memories/MEMORY.md, USER.md
   │                                       ├─ evolution.db LayeredMemory
   │                                       │   (agent:<id> scope)
   │                                       └─ recent_experiences (3 lines)
   │                  + dialectic supplement (heuristic user model)
   │
   ▼ turn::finish
on_turn_end (learning.rs:643)
   │
   ├─► record_and_reflect ─► Experience ─► ExperienceCollector.record
   │        (sanitize+inject-scan)         (evolution.db)
   │                              │
   │                              ▼
   │                    HeuristicReflector ─► LearningProposal[]
   │                              │
   │                              ▼
   │                    proposal_from_evolution ─► apply_or_stage
   │                              │
   │              ┌───────────────┼────────────────┐
   │              ▼ Auto          ▼ Approval/      ▼ Suggest/other
   │         memory.save /   stage → learning.json  audit "skipped"
   │         skills.Manager   (operator approves →
   │         .create/.edit    same apply path)
   │              │
   │              ▼
   │        audit log (learning.json)
   │
   └─► review_is_due? ─► spawn_learning_review ─► model proposes
        (every N tool turns)      same LearningProposal shape ─► same gate

background:
  boot ─► recover_and_process (drain reflection jobs)
  hourly ─► self-check: mapek.monitor/analyze + opportunity scan
            (read-only; emits RuntimeEvent::Notice)
```

---

## 5. Storage layout

Under `$VIKRAM_HOME` (default `~/.vikram`, `config.rs:559-570`; created `0o700`
because it holds `.env`):

| Path | Contents |
|---|---|
| `config.json` | `Config` — provider, model, context budgets, `learning_review_every`, `memory_nudge_interval`, `memory_provider` |
| `vikram.db` | `Store` (entities, sessions, hive, tickets) **and** `MemoryStore` (scoped key/value + FTS5 + consolidation observations) — one SQLite file, two opens (`runtime.rs:2287-2290`) |
| `evolution.db` | `EvolutionStore` — experiences, reflection jobs/proposals, `LayeredMemory`, skill versions, learning events, MAPE-K knowledge (`runtime.rs:2348`) |
| `learning.json` | `LearningManager` — **approval surface only**: staged proposals + audit log + 4 policies. Not a second memory store (`learning.rs:8-21`) |
| `memories/MEMORY.md`, `memories/USER.md` | Free-form durable notes, `§`-delimited, 8 000/4 000 char budgets, frozen snapshot for prompt-cache stability (`memory_files.rs`) |
| `skills/` | Evolution-tracked skills (`SKILL.md` + frontmatter + version rows); also the only transform target root |
| `plugins/` | Installed plugin dirs (manifest + integrity hashes) |
| `presets/` | Named config bundles (`presets.rs`) |
| `transforms.json` | Applied file-transform ownership/rollback state |
| `mcp.json`, `mcp-visibility.json` | MCP server config + tool visibility |
| `.env` | Provider API keys (never leaves the machine; excluded from dist bundles) |

---

## 6. Honest gaps and roadmap

**Done:** verification is wired end-to-end — `verify` verdict → `TurnGrowth` →
`Experience.metadata` → `reusable` gating → reflection confidence. The
approval surface cannot be bypassed: every proposal kind, from heuristic or
model source, passes `apply_or_stage`.

**Gaps:**

1. **Semantic verification.** `record_verification` reads a verdict *line*, not
   semantics — a step that "passed" by exiting 0 still passes. Deeper
   verification (did the artifact actually do the task?) needs a checker, not
   a parser.
2. **Benchmark-gated skill promotion.** `vikram bench learning` measures store
   behavior, not agent outcomes. Promoting a learned skill from `Suggest` to
   `Auto` should require benchmark evidence, not just approval.
3. **Preset suggestion producer keys.** `presets suggest` scans for
   config-shaped keys but nothing emits structured suggestions yet — the
   learning loop would need a proposal kind that maps outcomes to config
   changes (e.g. "context window too small" → preset).
4. **Durable self-\* state.** Pending opportunities/executions live in
   process `Vec`s; persisting them to `evolution.db` is required before any
   scheduled mutating MAPE-K cycle is safe.
5. **Scheduled MAPE-K.** The mutating half is deliberately operator-driven;
   scheduling it needs (4) plus an autonomy level check (`AutonomyManager`
   exists but is not consulted by the self-check path).
6. **Cross-session learning scope.** Memory writes land in session scope;
   promotion of a session-learned fact to `global` is manual today.
7. **Embedding recall.** Hybrid recall is lexical only; a local embedder
   would upgrade the cosine signal the code already leaves a slot for
   (`runtime.rs:1256-1258`).
