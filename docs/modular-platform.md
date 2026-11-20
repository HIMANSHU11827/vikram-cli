# Modular Platform Roadmap

Goal: evolve Vikram into a complete, open, deeply modular, core-modifiable
AI-agent platform — defaults provided, never mandatory; plugins modify
defaults in place; complete subsystem replacement is optional.

## What already exists (verified in tree)

- **Plugins**: manifest + permissions (deny-first) + integrity (sha256) +
  version constraints + install/audit/exec (`crates/vikram-runtime/src/plugins/`).
  Workspace plugins are listed but never executable; hooks load only from
  installed plugins.
- **Hooks**: 5 points — `turn_start`, `context_build`, `model_request`,
  `tool_call`, `tool_result` (`hooks.rs`, wired in `turn.rs`). Declarative +
  process hooks, permission-gated, payload-capped, panic-isolated.
- **Replacement seams (partial)**: `Provider` trait (provider.rs),
  `MemoryProvider` trait + `BuiltInMemoryProvider` (memory_provider.rs),
  `Tool` trait (tools/core.rs), `GatewayPlatform`/`PlatformAdapter` traits.
- **Module catalog**: `vikram modules` — bundled hook-module catalog with
  install/confirm.
- **Events**: `RuntimeEvent` bus exists for UI/host consumption.
- **MCP**: client + server, plugin-adjacent tool surface.
- **Skills**: discovery/loader/trust/disabled-state, fail-closed.

## Gaps vs the vision

1. **Hook coverage** — 5 of ~20 needed points (no turn_end, user_input,
   session lifecycle, memory ops, delegation, agent lifecycle,
   response delivery, task completion).
2. **Middleware ordering** — hooks have no priority/dependency ordering;
   no transform/short-circuit semantics beyond current rewrite rules.
3. **Typed event bus for plugins** — RuntimeEvent is host-facing, not a
   plugin subscription surface.
4. **Policy providers** — plugins can't register execution/model/tool/
   memory/context policies; permissions are static manifests only.
5. **Service providers / overrides** — no registry for alternative
   implementations of runtime services; no reversible managed patches.
6. **Subsystem replacement** — Provider/MemoryProvider traits exist but
   selection is not user-configurable end-to-end (no config key, no
   provider discovery, no fallback chain UI).
7. **Module manager depth** — install/enable/disable exist; no
   update/rollback/pin/version-constraints/dependency resolution/registry.
8. **SDK** — no scaffolding/templates/validation for module authors.
9. **Distributions** — no export/import of a customized installation.
10. **Agent packs/workflows/skill packs** — agents/bots/hives exist but no
    packaged, installable agent definitions.

## Phases

- **P1 Audit** — done across 10 generations; this doc is the inventory.
- **P2 Hardening** — done (2,850+ tests green, all P0–P3 findings fixed).
- **P3 Modular core** — subsystem boundaries: Provider, MemoryProvider,
  Tool, GatewayPlatform traits exist; add ContextEngine + Orchestrator
  seams where coupling is proven.
- **P4 Deep extension foundation** — expand HookPoint coverage, add
  priority-ordered middleware semantics, plugin event subscription,
  policy-provider registry. *(in progress)*
- **P5 Real core modification** — wire new hook points into turn loop,
  memory, context, delegation, session lifecycle; prove Test C/D/E.
- **P6 Module management** — update/rollback/pin/deps/registry.
- **P7 Subsystem replacement** — config-selectable memory/context/
  provider/orchestrator; prove Test F/G/H.
- **P8 SDK** — `vikram module init`, templates, docs, validation.
- **P9 Agent ecosystem** — agent packs, workflow packs, skill packs.
- **P10 Distributions** — export/import/validate/rollback.
- **P11 Validation** — Tests A–L with evidence.

## Non-negotiable test scenarios (tracking)

A default works · B additive plugin · C deep core modification ·
D middleware transform · E memory extension · F memory replacement ·
G interface replacement · H mixed architecture · I rollback ·
J failure isolation · K security · L distribution portability.

Current evidence: B/J/K partially proven by plugin/hook tests;
C/D/E/F/G/H/I/L not yet demonstrable.

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
3. **Native Rust modules** — compiled crates wired at build/distribution
   time (`vikram module` packs that vendor a crate the runtime loads via
   a versioned `Module` trait). Trusted only; requires rebuild. Honest:
   no dlopen hot-reload — activation is restart or managed rebuild.
4. **WASM** — deferred; needs wasmtime + component model design. Noted as
   future isolation tier for untrusted compute.

Managed patches: tracked file-overlay mechanism — a plugin may ship
`patches/` applied to a declared allowlist of extension files at install,
reverted on disable/uninstall. Never silent; always diffable.

Stage-level modularity: meaningful processing stages (context build,
memory recall/consolidate, tool dispatch, provider request) become
named ordered pipeline steps a plugin can prepend/append/replace via
the hook+priority system — not one trait per line of code.
