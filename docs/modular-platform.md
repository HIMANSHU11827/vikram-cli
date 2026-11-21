# Modular Platform Roadmap

Goal: evolve Vikram into a complete, open, deeply modular, core-modifiable
AI-agent platform — defaults provided, never mandatory; plugins modify
defaults in place; complete subsystem replacement is optional.

## What already exists (verified in tree)

- **Plugins**: manifest + permissions (deny-first) + integrity (sha256) +
  version constraints + install/audit/exec/update/rollback
  (`crates/vikram-runtime/src/plugins/`). Workspace plugins are listed but
  never executable (`installed` provenance flag, `plugins/tests.rs`);
  hooks load only from installed plugins. `plugin-sources.json` records
  each install's origin so `plugins update` can re-resolve the recorded
  source before falling back to `--from`/registry.
- **Plugin registry**: `vikram registry list|search|info|install|outdated|
  refresh|set-url` (`crates/vikram-cli/src/registry.rs`) merges bundled +
  remote (http(s)-only sources, curl-fetched archives) + local indexes;
  `install` upgrades an installed plugin through `registry.update` when the
  index carries a newer version (old copy parked for rollback), and refreshes
  the plugin-registry cache so `plugins update` cannot silently downgrade.
- **Stage pipeline**: 10 `StagePoint`s (`pipeline.rs` `StagePoint` —
  context_build, memory_recall, provider_request, tool_dispatch,
  result_commit, user_input, response_commit, turn_end, session_lifecycle,
  agent_lifecycle) compose handlers with `StageOutcome::Pass` decline
  semantics: a handler may mutate the payload and pass to the next
  registration, or resolve the stage itself. Process plugins and WASM
  modules can both act as stage handlers (`ProcessStageHandler`,
  `WasmStageHandler` in `pipeline.rs`).
- **Hooks**: 14 `HookPoint`s (`hooks.rs` `HookPoint::ALL`) — session_start,
  user_input, turn_start, context_build, memory_recall, model_request,
  tool_call, memory_write, tool_result, response_delivery, turn_end,
  delegation_start, delegation_end, session_end. Declarative + process +
  wasm hooks, permission-gated, payload-capped, panic-isolated, priority
  ordered (`priority` field, higher first, ties by plugin id).
- **WASM tier (executes)**: `.wasm` modules under `components.wasm` or a
  `.wasm` manifest entry run in-process through wasmi — no subprocess, no
  WASI (`plugins/wasm.rs`). ABI v2 is JSON-over-memory: exports
  `memory`/`alloc`/`dealloc`/`handle`, host imports `vikram.log` (bounded
  per-request capture merged into the reply as `_logs`) and
  `vikram.read_resource` (confined file reads inside the plugin dir,
  symlink/`..` escapes refused). Fresh store + fuel budget
  (`WASM_FUEL_LIMIT`) per call; only `process`/`network` permissions allow
  execution, same as process plugins.
- **Managed transforms**: plugins ship file ops applied by
  `vikram transforms apply`; conflicts refuse the whole apply unless the
  operator passes `--force`, which records the displaced owner's op for a
  path so ownership handoffs stay reversible (`crates/vikram-cli/src/transforms.rs`).
- **ACP**: `vikram-acp` implements `terminal/create|output|release|
  wait_for_exit|kill` behind `with_terminal_support` — pipe-backed (no PTY),
  `cwd`-confined, capped live terminals and per-terminal output buffers
  (`crates/vikram-acp/src/lib.rs`). Programs requiring a controlling
  terminal still get `-32601`.
- **Replacement seams (partial)**: `Provider` trait (provider.rs),
  `MemoryProvider` trait + `BuiltInMemoryProvider` (memory_provider.rs),
  `Tool` trait (tools/core.rs), `GatewayPlatform`/`PlatformAdapter` traits.
  `memory_recall` is a real pipeline stage; memory consolidation runs as
  the `memory.consolidate` tool (observations + memory-table dedup/
  contradiction supersession, `tools/memory.rs`, `MemoryStore::consolidate`).
- **Module catalog**: `vikram modules` — bundled hook-module catalog with
  install/confirm; `vikram module init|validate|pack` scaffolds plugin,
  skill-pack and agent-pack directories (see `module-sdk.md`).
- **Distributions**: `vikram dist export|import` packages a customized
  installation; `vikram presets suggest` is a read-only scan of
  evolution.db experiences/proposals for preset signals.
- **Gateway**: non-text `OutboundVerb`s (media, reaction, poll, card,
  draft) are deliverable through `deliver_verb`; assistant image artifacts
  (`Block::Image` committed from `IMAGE:` tool markers — `browser.screenshot`,
  `computer.screenshot`, `computer.read_screen`, `turn.rs`) are delivered as
  ledgered media verbs by the gateway CLI (`gateway.rs` `outbound_media`).
  `gateway add` masks TOKEN/SECRET/PASSWORD/KEY prompts via termios
  echo-off (`gateway_config.rs` `tty_ask`).
- **Store**: session/artifact/file-change/usage/hive-message listings are
  paginated (`LIMIT`/`OFFSET` throughout `vikram-store`); `Store::open`
  runs best-effort `prune_retention` + `prune_journal`, and
  `prune_journal` deep-prunes journal prefixes covered by a committed
  compaction marker (`session_compaction.prunable_through_seq`) — the marker
  always precedes the prunable rows, and replay is verified identical
  pre/post prune.
- **Events**: `RuntimeEvent` bus exists for UI/host consumption.
- **MCP**: client + server, plugin-adjacent tool surface.
- **Skills**: discovery/loader/trust/disabled-state, fail-closed, staged
  proposals gated through evolution approvals.

## Gaps vs the vision

1. ~~Hook coverage~~ — **done**: 14 hook points wired across the turn and
   session lifecycles.
2. **Middleware ordering** — hooks have priority ordering; transform/
   short-circuit semantics exist via hook rules and stage `Pass`, but there
   is no general "replace this stage" UX beyond `register_stage`.
3. **Typed event bus for plugins** — RuntimeEvent is host-facing, not a
   plugin subscription surface.
4. **Policy providers** — plugins can't register execution/model/tool/
   memory/context policies; permissions are static manifests only.
5. **Service providers / overrides** — Provider/MemoryProvider traits exist
   but selection is not user-configurable end-to-end (no config key, no
   provider discovery, no fallback chain UI).
6. **Subsystem replacement** — `memory_recall` is a replaceable stage and
   `MemoryProvider` is a trait seam, but orchestrator/context-engine
   replacement is not user-configurable.
7. ~~Module manager depth~~ — **mostly done**: install/enable/disable/
   update/rollback/pin/version-constraints/dependency resolution/registry
   (`vikram registry`, `plugins update`, `plugin-sources.json`). Remaining:
   signed indexes, namespaced registries.
8. ~~SDK~~ — **done (v1)**: `vikram module init|validate|pack` +
   `docs/module-sdk.md`. Remaining: templates beyond the scaffold, doc site.
9. **Distributions** — export/import exists; no signed/verified bundles.
10. **Agent packs/workflows/skill packs** — agent-pack and skill-pack kinds
    exist (`module-sdk.md`); no packaged workflow definitions.
11. **Gateway non-text producers** — poll/card/draft verbs are deliverable
    but nothing produces them yet (no real trigger); media is produced only
    from screenshot artifacts.

## Phases

- **P1 Audit** — done across generations; this doc is the inventory.
- **P2 Hardening** — done (~2,900 tests green, clippy clean).
- **P3 Modular core** — subsystem boundaries: Provider, MemoryProvider,
  Tool, GatewayPlatform traits exist; stage pipeline gives ContextEngine +
  Orchestrator seams.
- **P4 Deep extension foundation** — **largely done**: 14 hook points,
  priority ordering, Pass-decline stage composition, process + WASM plugin
  tiers, managed transforms.
- **P5 Real core modification** — stage handlers can wrap or replace every
  named step; remaining proof is per-scenario evidence (Tests C–L below).
- **P6 Module management** — **mostly done**: update/rollback/pin/deps/
  registry via `vikram registry` + `plugins update` + source tracking.
- **P7 Subsystem replacement** — config-selectable memory/context/
  provider/orchestrator; prove Test F/G/H.
- **P8 SDK** — **done (v1)**: `vikram module`, templates, validation.
- **P9 Agent ecosystem** — agent packs, workflow packs, skill packs.
- **P10 Distributions** — export/import/validate/rollback.
- **P11 Validation** — Tests A–L with evidence.

## Non-negotiable test scenarios (tracking)

A default works · B additive plugin · C deep core modification ·
D middleware transform · E memory extension · F memory replacement ·
G interface replacement · H mixed architecture · I rollback ·
J failure isolation · K security · L distribution portability.

Current evidence: A/B/J/K partially proven by plugin/hook/wasm/store
tests; I partially proven (plugins update rollback, transforms --force
handoffs); C/D/E demonstrated by stage-handler + hook-rule tests;
F/G/H/L not yet demonstrable end-to-end.

## Extension architecture decision (Phase 4+)

Plugin execution methods, by trust level:

1. **In-process declarative hooks** (existing) — manifest-declared hooks at
   named points; permission-gated; cannot execute arbitrary code beyond
   declared `process`/`network` grants. Covers most customization.
2. **Process plugins** — separate executable + JSON-RPC/stdio protocol
   (same shape as MCP). Full capability isolation: plugin gets only the
   host-provided service API; permissions enforced host-side. This is the
   untrusted/extreme-power path — a process plugin can do anything the
   granted capabilities allow because the host brokers every call.
   Process plugins can also act as stage handlers.
3. **WASM plugins (implemented)** — `.wasm` modules run in-process through
   wasmi under a fuel budget, no WASI, JSON-over-memory ABI v2 with
   `vikram.log` + `vikram.read_resource` host imports. Same permission
   gate as process plugins; tighter isolation than a subprocess (no fs,
   no env, no network unless the host import is added later — currently
   none exists) at the cost of a sandboxed compute model. See
   `plugins/wasm.rs`.
4. **Native Rust modules** — compiled crates wired at build/distribution
   time (`vikram module` packs that vendor a crate the runtime loads via
   a versioned `Module` trait). Trusted only; requires rebuild. Honest:
   no dlopen hot-reload — activation is restart or managed rebuild.

Managed patches: tracked file-overlay mechanism — `transforms` applied by
`vikram transforms apply` to declared paths; conflicts refuse unless
`--force`, which records the displaced owner so the handoff stays
reversible. Never silent; always diffable.

Stage-level modularity: the 10 `StagePoint`s are named ordered pipeline
steps; a plugin (process, WASM, or native `register_stage`) can wrap,
mutate-and-pass, or resolve each step. Ordering is by registration
priority; `Pass` decline composes handlers into a chain.
