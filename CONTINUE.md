# VIKRAM — master continuation plan

Status: **~2,900 tests passing, clippy clean, pushed to GitHub at `e3c95dd`**
(47 commits this session). Architecture is production-grade.

## Done this session (committed + pushed)

- **Modularity**: 10 StagePoints + Pass-decline, 14 HookPoints, process +
  WASM (stages+hooks, log/read_resource host fns) tiers, provider/memory
  seams, transforms+force, registry+doctor, signed dists (HMAC+ed25519),
  presets lifecycle, module SDK.
- **Evolution**: full learning loop + verify detail + 8 benchmarks +
  opt-in scheduled cycles + performance metrics + relations tool +
  user/project/hive memory scopes + repeated-failure proposals +
  self_* engines real/durable.
- **Gateway**: all 6 outbound verbs have producers (text/media/poll/card/
  draft/reaction) via marker chains; ACP real PTY terminals; TUI images.
- **Store**: pagination everywhere it matters, journal deep-prune protocol,
  cheap interrupted-session scan, memory-table consolidation.

## Remaining — prioritized

### P0 — deepest remaining
1. **Semantic verification**: "tests pass, feature missing" — verify now
   reports git changes but doesn't judge intent. Needs diff-vs-claim
   analysis (bounded, honest).
2. **WASM `prompts` component type**: needs a real consumer/plugin first —
   don't add speculatively.
3. **Multi-agent orchestration depth**: hive works; delegation graphs,
   agent performance-driven routing not built.

### P1 — coverage
4. Card/Draft/Reaction per-adapter long-tail (verbs refuse honestly today).
5. `plugins update` for non-registry sources edge cases.
6. TUI: richer rendering (markdown, diffs, reasoning collapse states).
7. `session fork` media/artifact carry-over audit.

### P2 — long tail
8. Journal: compaction-state GC for deleted sessions' artifacts.
9. Provider-specific features (prompt caching, structured outputs).
10. ~~`computer.*` remaining helpers (clipboard, notifications)~~ — done:
    `computer.clipboard_read/write` + `computer.notify` landed.
11. Relations: temporal decay, inference from conversations.
12. Distribution: plugin marketplace signing chain beyond manifest sig.

### JARVIS-class track (docs/jarvis-class.md, docs/voice-plan.md, docs/always-on.md)
- [x] docs/voice-plan.md — phased voice plan, honest limits
- [x] docs/always-on.md — presence-session design (phases A–D)
- [x] Event-driven daemon triggers (`on <trigger>` schedules, EventBus,
      daemon_events dedup, quiet hours, daemon_audit trail)
- [x] Personality-consistency harness (`vikram bench personality`,
      deterministic local scoring)
- [ ] Phase A: durable presence session identity
- [ ] Phase B: liveness states (awake/dozing/asleep) + `/sleep` `/wake`
- [ ] Phase C: proactive turns via triggers into presence session
- [ ] Phase D: vikram-voice crate (Phase 1 push-to-talk per voice-plan.md)

### Verification
```bash
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets
```
