# VIKRAM — master continuation plan

Status: **~2,900 tests passing, clippy clean, committed at `8af22df`** (30 commits this
session). Architecture is production-grade; remaining work is coverage depth.

## Done this session (committed)

- **Modularity**: 10 StagePoints + Pass-decline composition, 14 HookPoints,
  process + WASM (wasmi, stages+hooks) plugin tiers, provider/memory seams,
  transforms, `vikram registry`, dist export/import, presets, module SDK,
  plugins update/rollback + source tracking.
- **Evolution**: full learning loop wired (experience→reflection→gated
  proposals→memory/skills→verify signal→benchmarks), durable engines
  (engine_records v3), operator-gated cycle/benchmark/approve/reject,
  real conversational handlers + free-form Ask, autonomy approval producer,
  hive lessons, user/project memory scopes, hourly self-check, self_* engines
  made real/durable, skill proposal gate, journal deep-pruning protocol.
- **Docs**: competitor-research.md, evolution-architecture.md,
  evolution-surface.md, module-sdk.md, modular-platform.md.

## Remaining — prioritized

### P0 — deepest remaining features
1. **Contracts-level media output**: assistant messages never emit
   `Block::Image`; gateway `deliver_verb` media path is ready and waiting.
   Needs provider/tool support for image generation or attachment commit.
2. **Semantic verification**: verify-tool verdict line feeds learning, but
   "tests pass, feature missing" isn't caught — needs diff/semantic checks.
3. **self_development executor stages**: Implementation/Tests/Comparison
   stages are honest `success:false` — need real executors.

### P1 — coverage depth
4. WASM hook points beyond stage/hook JSON contract (resources/prompts).
5. Registry: `plugins update` pulling newer registry versions (currently
   cached-copy reinstall).
6. TUI masked secret input (gateway add is masked; TUI prompts aren't).
7. Per-plugin transform conflict UX (currently refuse-on-conflict).

### P2 — long tail
8. ACP `terminal/*` methods (needs vikram-terminals bridge).
9. Non-text producers for polls/cards/drafts (no real trigger exists).
10. Journal: deeper pruning needs runtime marker-protocol extension.
11. `list_interrupted_sessions` O(total journal) startup scan.
12. Semantic memory dedup/contradiction in `memory` table (consolidation
    covers observations table only).

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
