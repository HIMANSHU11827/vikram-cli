# Vikram → Jarvis-class Upgrade — Design (2026-11-22)

Approved direction: user selected parallel sub-agent tracks with a harness-engineering
worker, CLI-first, following `docs/jarvis-class.md`. GUI comes later.

## User direction (quotes preserved)

- "We are making a Turbo AI Assistant like Freedom like Jarvis ... use multiple AI at
  an time start working now ... He need to be intelligent autonomous level full"
- "use mpsil sub agent dive work, dijoint work work you become aorchter ok and work"
  → orchestrator + multiple sub-agents on disjoint work
- Approach selection: "use sub agent give thm task ok they will work parrely for
  you ok we are making ... hud, arc rector on know ok becye its cli based later we
  wilkl add ui ok but cli will have a toz ok" → HUD/arc-reactor in the CLI now;
  GUI deferred
- "use harness engring sub agent to upgrade our harneseer ok and also see javrius
  class work ok etc do and contu wokrign know convetr our vikram like javris"
  → harness-engineering sub-agent; follow jarvis-class.md; proceed now

Unparsed user fragments (open, non-blocking): "Jarvis Timo different MDFI",
"cli will have a toz".

## Goal

Convert Vikram from a CLI agent runtime into a JARVIS-class assistant as defined in
`docs/jarvis-class.md`: voice-first, remembers the user, consistent personality,
acts with judgement, proactive, self-verifying — with a multi-agent orchestrator at
the core and a Jarvis-flavor console in the TUI.

## Chosen architecture — two parallel tracks, disjoint file ownership

**Step 0 (gate, main agent): land the uncommitted Phase D voice WIP.**
16 modified files + untracked `crates/vikram-voice/` + `crates/vikram-cli/src/voice.rs`
sit on master (HEAD 196f731). Verify (`cargo fmt --check`, `clippy`, `test --workspace`),
fix trivial issues, commit. No parallel work starts until the tree is clean — parallel
agents on a dirty tree would thrash. Nothing is pushed; commits stay local.

**Track 1 — Harness engineering (kernel).** Owner: `vikram-runtime`, `vikram-policy`,
`vikram-store` context assembly.

- Orchestrator MVP per research (12 sources, ranked decisions):
  1. `spawn_worker(brief)` supervisor-via-tools — one orchestrator whose extra tool
     spawns workers; no worker-to-worker channels in v1
  2. Typed delegation briefs: objective, output format, tool allowlist, file scope,
     task boundary
  3. Kernel-enforced disjoint file ownership: worker-owned globs denied at the
     policy/effect machine, not by prompt
  4. Monotonic trust narrowing: child capability set = parent minus restrictions;
     denied actions cannot be re-requested via a sibling
  5. Artifact-store handoffs: workers write results to memory/filesystem, return
     references — orchestrator merges references, not transcripts
  6. Partial-result handling: `max_turns` → marked-partial + resumable; one retry,
     then reassign or degrade — never abort the team for one worker failure
  7. Effort scaling in the orchestrator prompt: 1 worker simple / 3–5 default /
     10+ only when justified
  8. Role-based model routing: `model` + `effort` on worker-role specs (default:
     inherit); cheap models for read-only roles
- JARVIS Phase A: durable presence session identity
- JARVIS Phase B: liveness awake/dozing/asleep + `/sleep` `/wake`
- Relationship model (jarvis-class.md §6 #2): first-class rows — people, projects,
  preferences, mood — feeding context assembly before every turn
- Semantic verification (CONTINUE.md P0): feature-level acceptance so "tests pass"
  stops meaning "feature missing"

**Track 2 — Voice + Jarvis console.** Owner: `vikram-voice`, `vikram-cli` voice path,
`vikram-tui`.

- Finish Phase D push-to-talk (uncommitted WIP): ASR in, TTS out, interruptible;
  Voxtype is installed on the machine (VAD verified on today) — reuse it as the
  input path where practical
- TUI arc-reactor state glyph per jarvis-os plan: ● idle muted · ◐ thinking accent
  pulse · ▲ approval urgent · ✓ done
- Team activity HUD in the TUI console: live workers, briefs, results
- JARVIS Phase C support surface: what the console shows when daemon triggers fire

**Track 3 — Daemon triggers (Phase C).** Owner: `vikram-daemon`.

- Extend from cron to event-driven triggers (file change, calendar proximity,
  inbox arrival, long-job completion) with a silence policy: when NOT to speak

**Integration gate (main agent):** full `cargo test --workspace` green; TUI shows a
live orchestrator team; voice round-trip works; docs updated (CONTINUE.md, AGENTS.md
per doc-check findings).

## Sequencing rationale (jarvis-class.md §6)

Voice + relationship are what make it feel like JARVIS; proactivity without them is
noise; policy is already built. Orchestrator depth is a named P0 in CONTINUE.md.
The hard half (memory, policy, tools, gateway, evolution) is already built — this
design fills the human-facing half and the multi-agent core.

## Out of scope now (deferred per user: "later we will add ui")

- jarvis-os Quickshell plugin build (plan exists at ~/Desktop/jarvis-os)
- vikram-ui React PWA wiring
- WASM prompts component, plugin marketplace signing, Windows/Termux isolation

## Honesty markers

- "3,387 tests passing at local HEAD `5a12d42`, clippy clean" are CONTINUE.md claims —
  verified live at Step 0 before being trusted
- Nyaya audit (Nov 08): CI historically red, `fmt --check` had 23 diff sites,
  e2e autonomy unproven; REFINEMENT batch 34 fixed several of these
- Doc-check sub-agent reports (3 running) adjudicate remaining md claims; updates
  land as part of integration, not before

## Risks

- WIP voice work may not compile/test clean → fix or revert before Track 1 starts
- Sub-agent merge conflicts → disjoint crate ownership, enforced at dispatch time
- Orchestrator + phases A/B are both in `vikram-runtime` → same worker owns both,
  sequenced internally (orchestrator first)
