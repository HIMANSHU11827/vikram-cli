# VIKRAM testing strategy

Status: proposal v1 (2026-10-31), grounded in `docs/research/benchmarks/agent-benchmark-landscape.md`.
Principle: **everything below the model boundary is deterministic and belongs in CI; everything
above it is stochastic and belongs in a logged eval harness with statistics, not assertions.**
VIKRAM rejects mock *production* providers; a scripted fake provider exists ONLY as a test
fixture (see "Fake provider containment").

## 1. Test layers

| Layer | Scope | Determinism | Where it runs |
|---|---|---|---|
| L0 | Unit: parsers, context compaction, IPC framing, permission policy eval | Full | every PR, `cargo nextest` |
| L1 | Agent loop vs scripted fake provider | Full | every PR |
| L2 | Golden/snapshot: tool transcripts, TUI frames, session JSONL | Full | every PR, `insta` |
| L3 | Property: IPC codec, edit-applier, diff/patch round-trips, state machine | Full (seeded) | every PR, `proptest` |
| L4 | Integration: real `vikram` binary over IPC/pty in a temp sandbox dir | Full (fake provider) | every PR, Docker + bare linux |
| L5 | Chaos/recovery: SIGKILL mid-turn, network cut, child-process death, resume | Full (fake provider) | every PR (fast subset), nightly (full) |
| L6 | Resource budget: idle/active RSS, CPU-s-per-task, fd count, regression benchmarks | Full | nightly + on-demand; criterion + dhat |
| L7 | TUI: `TestBackend` buffer asserts + synthetic crossterm events | Full | every PR |
| L8 | Model-backed evals: real providers × task suite | None (statistical) | nightly/weekly + pre-release, never blocks PRs |

## 2. Deterministic core: the scripted provider as TEST fixture

- Trait boundary: `Provider::complete(RequestStream) -> EventStream`. Production impls:
  `vikram-providers/*`. Test impl: `vikram-testfx::ScriptedProvider` — replays a recorded
  `.vikramtape` file (JSONL of provider events with inter-event delays, optionally virtual).
- **Containment rules (enforced, not cultural):**
  1. `ScriptedProvider` lives in `crates/testfx`, declared only in `[dev-dependencies]` and
     under feature `test-fx`; `#![cfg(not(test))]` compile-fail guard in `main.rs` rejects the
     feature in release builds (`required-features` + a CI check `cargo tree -e features
     --release | grep -q test-fx && exit 1`).
  2. No `dyn Provider` registry keyed by a runtime "mock" string in production code; the
     provider list is a `match` over a closed enum that has no Mock arm.
  3. `.vikramtape` fixtures are recordings of *real* sessions (redacted) so L1–L5 pin today's
     model behavior without calling it — these are fixtures, not simulations of intelligence.
- What L1 tests: turn accounting, tool-call dispatch, retry/backoff (against a deterministic
  failing HTTP `wiremock` server — infra failure is deterministic even when responses aren't),
  max-token / max-turn cutoffs, permission gate (allow/deny/ask state machine), cancellation,
  context-window budgeting, background job orchestration.

## 3. Snapshot & property testing

- `insta` for: rendered TUI frames (ratatui `Buffer` → text), session file format, tool result
  truncation, error payloads. Snapshots committed; review via `cargo insta test --check`.
- `proptest` for: IPC codec (arbitrary frames must never panic the reader; fuzz corpus also fed
  through `cargo fuzz` in nightly), edit-application (apply→revert identity), path/permission
  matching (no glob can escape sandbox root — express as safety property, 10k cases).
- ratatui official pattern (docs.rs ratatui `TestBackend`): construct
  `Terminal::new(TestBackend::new(80,24))`, draw, assert `buffer.area(...)`, feed
  `Event::Key/Event::Paste` directly into the event loop's receiver — no pty needed for L7;
  one `xterm.js`-free pty smoke test in L4 covers real terminal encoding.

## 4. Integration over IPC (L4) and chaos (L5)

- Spawn the actual binary with `tokio::process`, `VikramHome` in a `tempdir`; drive it as a real
  client (JSON-RPC/IPC), assert protocol conformance transcripts (`insta` on message sequence).
- L5 matrix (all with ScriptedProvider paused mid-tool): kill -9 runtime → resume → session
  rehydrates, no duplicated side effects (idempotency ledger asserted); kill child shell →
  supervisor reports job death, agent notified; drop network via `tc netem`/blocked mock port →
  retry policy + user-visible degraded mode; OOM one child → contained. Each scenario is a
  deterministic state-machine test: the "chaos" is injected, not random.
- Background-task reliability metric: launch N sleep/writing jobs, SIGTERM runtime, resume,
  assert job ledger completion count == expected (feeds the pass^k-style metric in L8).

## 5. Resource measurement (L6) — runtime only, model excluded

- Sampler crate `vikram-metrics` (bin): reads `procfs` for self + descendants (RSS, utime/stime,
  fd count, context switches) at 100 ms; on macOS uses `libproc` via `sysinfo`; Termux/aarch64:
  procfs path, CI-labeled "android-shell" run on a hosted aarch64 linux container as proxy
  (documented deviation — Termux's `/proc` is the same kernel interface; selinux differences
  untested in CI, called out in reports).
- Budget assertions in CI (not benchmarks — thresholds): idle runtime < X MB RSS, < Y% 1-core
  (config: `metrics/budgets.toml`, regressions fail the job); `dhat` max-heap assertions for
  hot paths (session load, context compaction); `cargo bench` (criterion) tracks per-turn
  harness CPU time with .min-breakpoint baselines stored per-arch; `tokio-console` +
  `tracing→opentelemetry` exported per eval run (task starvation / busy tasks) — human-reviewed
  nightly artifact, not gate.
- Wall-clock, tokens, cost, interventions, regressions per task are emitted into the run
  manifest (below) for both L4 (fake) and L8 (real) runs — same schema, so L8 is "L4 with a
  live model".

## 6. Model-backed eval harness (L8) — internal design

- **Harness-first discipline** (from research §B): tasks pin (prompt file, tool allowlist,
  max_turns, token budget, retry policy, sampling params) in a versioned `evals/tasks/*.toml`
  (Harbor-format-compatible so we can also run upstream suites). The runtime under test is
  registered as a Harbor/Terminal-Bench *adapter* (`vikram` entry in installed agents) so TB
  2.1/3.0 numbers are publicly comparable.
- Every run writes `run-<git>-<model>-<ts>.eval.jsonl`: full transcript, tool events, permission
  decisions, token/cost, wall clock, restart count, human-intervention count (approve/ask
  responses in non-interactive mode = human sim), attempted-unauthorized-action count
  (permission gate denials are first-class log events), crash/recovery events, RSS peak.
- Statistics: k=8 repeats/task; report pass@1 mean ± 95% CI **and pass^8**; never post best-of-k.
  NOP (empty agent) and oracle (scripted solution) baselines run for every suite → task leakage
  visible. Regression gate for evals: new runtime version must not drop suite mean >2 pts at
  equal token budget (budget-matched comparison à la HAL; a cheaper win counts, an expensive win
  logs "cost-normalized: neutral").
- External-task honesty: internal task sets are the primary signal; public boards (TB, SWE-L,
  τ²) reported only with date, model endpoint string, and our published manifest.

## 7. Fake vs mock: terminology lock

Production: no mocks. Tests: `ScriptedProvider` (fixture replay), `wiremock` (HTTP transport
failures), `null`/`oracle` agents borrowed conceptually from harbor/TB (they are benchmark
baselines, and we run both as *fixtures* in L4 and as *baselines* in L8).

## 8. CI matrix

```
PR gate (must be green, <15 min):
  os: ubuntu-22.04 x86_64 | ubuntu-24.04 aarch64 (ARM runner) | macos-14 aarch64 | windows-2022 x86_64
  steps: cargo nextest run --workspace --features test-fx | cargo insta test --check
         | clippy -D warnings | fmt --check | budget smoke (idle RSS < bound on ubuntu x86_64)
Cross-builds (PR, compile-only):
  cross: aarch64-unknown-linux-gnu/musl, x86_64-unknown-linux-musl
  cargo-zigbuild: aarch64-linux-android (Termux artifact — unit tests for it run in the
                  aarch64-linux container, android runtime smoke is nightly-manual label)
Nightly:
  L5 full chaos matrix, criterion --save-baseline (per arch), dhat asserts, tokio-console +
  OTel artifact capture, L8 model suite (2 pinned models × task suite, k=8), Harbor TB run
Release:
  L8 extended (all supported models/providers), external suite registration, report gen with
  dates + CIs + cost, published alongside manifests
```

Flakiness control: `nextest` retries **disabled** on PRs (flakes are bugs; quarantine label
`flaky-quarantine` excludes from PR gate, must be fixed or deleted within 14 days); tokio virtual
time (`start_paused = true`) so no sleep-based waits; no wall-clock assertions below 100 ms with
>3σ margins; network egress forbidden except allowlisted provider endpoints on L8; seeds logged
and re-runnable (`RUST_SEED` env consumed by proptest + chaos schedules).

## 9. What "green" will NOT claim

L8 never gates merges (model drift ≠ code regression). Public score comparisons only at fixed
model + fixed sampling + published manifest. Runtime resource numbers always state platform and
whether inference was excluded. Intervention/violation counters count *attempts*, not outcomes.
