# VIKRAM — master continuation plan

Status: **~2,900 tests passing, clippy clean, HEAD `f01a523`** (~50 commits this
session). Architecture is production-grade; remaining work is coverage depth and
a few honest ceilings.

## Done this session (committed)

- **Modularity**: 10 StagePoints + Pass-decline composition, 14 HookPoints,
  process + WASM plugin tiers that **execute** (wasmi, stage handlers + hooks,
  ABI v2 `vikram.log`/`vikram.read_resource`), provider/memory seams,
  transforms with `--force` ownership handoffs, `vikram registry`
  (list/search/info/install/outdated/refresh/set-url, upgrades + rollback),
  dist export/import, presets suggest, module SDK, plugins update/rollback +
  `plugin-sources.json` source tracking.
- **Evolution**: full learning loop wired (experience→reflection→gated
  proposals→memory/skills→verify signal→benchmarks), durable engines
  (engine_records v3), operator-gated cycle/benchmark/approve/reject **plus
  opt-in scheduled cycles** (`config.evolution.auto_cycle`/`auto_benchmark`,
  default off), real conversational handlers + free-form Ask,
  `self_improvement` AnalysisReport execution on the self-check, autonomy
  approval producer (staged skill proposals), hive lessons, user/project
  memory scopes, hourly self-check, self_* engines durable + partially real,
  skill proposal gate.
- **Runtime/store**: assistant `Block::Image` artifacts from `IMAGE:` tool
  markers (browser.screenshot, computer.screenshot, computer.read_screen) +
  TUI image rows + gateway media-verb delivery; ACP `terminal/*` methods
  (pipe-backed, no PTY); masked gateway secret prompts; memory-table
  consolidation (dedup/contradiction supersession); cheap interrupted-session
  scan; paginated store listings; journal deep-pruning via compaction markers
  (`session_compaction.prunable_through_seq`).
- **Docs**: competitor-research.md, evolution-architecture.md,
  evolution-surface.md, module-sdk.md, modular-platform.md.

## Remaining — prioritized

### P0 — deepest remaining features
1. **Provider-side media**: assistant `Block::Image` now flows from tool
   artifacts to gateway media verbs, but no provider/tool *generates* images —
   delivery exists, generation does not.
2. **Semantic verification**: verify-tool verdict line feeds learning, but
   "tests pass, feature missing" isn't caught — needs diff/semantic checks.
3. **self_development executor ceiling**: skill projects can reach
   `Completed` on recorded store evidence, but the crate runs no build/test
   tooling — test stages review recorded outcomes only, and non-skill
   `Implementation` still records `success:false`.

### P1 — coverage depth
4. WASM host surface beyond `vikram.log`/`vikram.read_resource` (no
   resources/prompts/network imports yet).
5. ~~Registry version upgrades~~ — **done**: `registry install` upgrades via
   `registry.update` (old copy parked), `registry outdated` lists newer index
   versions, cache refresh prevents `plugins update` downgrades.
6. TUI masked secret input (gateway add is masked; TUI prompts aren't).
7. ~~Per-plugin transform conflict UX~~ — **done**: `transforms apply
   --force` takes ownership with recorded handoffs.
8. ~~ACP `terminal/*`~~ — **done** (pipe-backed, cwd-confined, capped). PTY
   support remains absent — programs needing a controlling terminal still
   get `-32601`.
9. Non-text gateway producers: poll/card/draft `OutboundVerb`s are
   deliverable but nothing produces them — no real trigger exists.
10. ~~Journal deep pruning~~ — **done**: `prune_journal` removes
    compaction-covered prefixes via `session_compaction.prunable_through_seq`
    (marker commits before rows it covers).
11. ~~`list_interrupted_sessions` O(total journal)~~ — **done**: SQL CTE over
    the journal kind index finds candidates; replay verifies only those.
12. ~~Semantic memory dedup/contradiction~~ — **done**:
    `MemoryStore::consolidate` covers the memory table (status +
    `superseded_by` lineage) alongside observation consolidation.

### In-flight (uncommitted — do not touch)
- **Contracts/store relations WIP**: uncommitted changes in `contracts/` and
  `store` crates. Finish or land before further contract/store edits.

## JARVIS-class track — see `docs/jarvis-class.md`

The runtime already has the hard half (memory, deny-first policy, tools, gateway, evolution).
The missing half is human-facing:

1. **Voice I/O** — ASR in, TTS out, interruptible. New `vikram-voice` crate; no workspace
   dependency exists today. Without it Vikram is a CLI, not a presence.
2. **Relationship model** — people, projects, preferences, mood as first-class rows that feed
   context assembly, not free-text memories.
3. **Event-driven proactivity** — `vikram-daemon` today is cron-only. Add triggers (file change,
   calendar proximity, inbox arrival, long-job completion) plus an explicit silence policy.
4. **Personality consistency harness** — score the same persona prompts across sessions and
   models so "same person every day" is measured, not hoped for.
5. **Always-on session** — keep working memory warm instead of paying cold start each turn.

Order matters: voice and relationship make it feel like JARVIS; proactivity before those is noise.

### Verification
```bash
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets
```
