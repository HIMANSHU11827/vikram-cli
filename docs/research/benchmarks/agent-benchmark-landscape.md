# Agent benchmark landscape (Parts 24 + 16 + 23)

Research date: 2026-10-31. Method: web search/fetch of papers, leaderboard sites, and code
clones under `/tmp/vikram-research/` (aider, terminal-bench `tb-repo`, harbor, inspect_ai).
Every score carries an as-of date. Confidence: [H] primary source read, [M] secondary
snippet/aggregate, [L] single low-quality source or inference.

---

## A. Benchmark-by-benchmark: what it measures, documented limits, dated leaderboard state

### A.1 SWE-bench family (Original / Verified / Multilingual / Multimodal / Pro)

**What it measures.** GitHub issue → patch that flips fail-to-pass tests in real Python (Original,
2,294 tasks/12 repos) or human-vetted 500-task Verified subset. Score = % resolved. [H]
https://github.com/swe-bench/SWE-bench , https://www.swebench.com/ (accessed 2026-10-31)

**Documented failure modes (the strongest cautionary literature in the field):**
- *Weak/flawed tests + human disagreement:* 7.8% of "correct" patches are wrong or trivially
  gameable; test-passing patch ≠ correct fix; human agreement on resolutions is low
  (arXiv:2503.15223, "Are 'Solved Issues' in SWE-bench Really Solved Correctly?", v2 2025-09-09). [H]
- *Solution leakage:* SWE-bench+ (Xue et al., ICLR-era 2025) found ~60.8% of "resolved" issues
  involve leakage (fix present in repo history/tests); also documents "resolved by hand-editing"
  style artifacts and the test-patch-visibility question (SWE-bench/experiments#16, 2024-06-06:
  is the agent allowed to see the test patch?). [H] https://arxiv.org/abs/2504.02116-family
- *Contamination → retirement:* OpenAI, "Why we no longer evaluate SWE-bench Verified"
  (2026-02-23): increasingly contaminated and "mismeasures frontier coding progress" — flawed
  tests + training-data contamination; they moved to private internal evals. [M — fetch of
  openai.com 403'd; multiple independent snippets corroborate, incl. a 2026-08-27 summary
  "corrected upward… inflated by leakage… retired by its maintainers"]
  https://openai.com/index/why-we-no-longer-evaluate-swe-bench-verified/
- *Merge-worthiness:* METR audited SWE-bench Verified PRs that pass tests: roughly half would
  NOT be merged by real maintainers (metr.org note, 2026-03-10). [H]
- *What it actually selects for:* Epoch AI's analysis — Verified predicts ability to fix simple
  issues (~couple hours of SWE time), not long-horizon work; Epoch also reported a systematic
  gap between their runs and externally reported scores → harness sensitivity (epochai.substack.com;
  epoch.ai/benchmarks/swe-bench-verified, 2026-06-25). [M]
- *Memorization:* "The SWE-Bench Illusion" (2025-08) — gains partly from memorized fixes. [M]

**Leaderboard reality (dated).** swebench.com official submission board (accessed 2026-10-31):
top entries are Claude 4.5 Opus ("high"), Gemini 3 Pro Preview, Doubao-Seed-Code — note these are
*submissions with named scaffolds*, i.e., harness+model combos. vals.ai methodology page
(2026-09-01): Claude Opus 5 with Mini-SWE-agent at 97.0% on Verified → **saturated**; a March-2026
aggregate put the frontier ~80.6–80.9% (Opus 4.5/4.6, Gemini 3.1 Pro). Verified died from ~35%
(early 2025) to 97% (late 2026) in 18 months. [M for aggregates, H that the board labels scaffold]

**Variants.** Multilingual = same harness, 7–8 languages (Java/TS/JS/Go/Rust/C/C++/C#);
Multimodal = UI/codebase visual issues (v2 open-sourced 2026-09-01, 480 tasks).
SWE-bench Pro (Scale AI; paper arXiv:2509.16941, 2025-09-21): 1,865 tasks across 41 professionally
maintained repos incl. private split; long-horizon; top models ~23% on the public set when
launched (labs.scale.com leaderboard); Morph tracker re-checked 2026-10-24 lists Pro V2 with
642 tasks + "HARD-51". Pro is the family's least-contaminated member because commercial repos
aren't on GitHub. [H]

**SWE-bench-Live** (swe-bench-live.github.io): auto-refreshing multi-language/multi-OS task stream
— designed to defeat contamination by construction. [M]

### A.2 Terminal-Bench (1.0 → 2.0 → 2.1 → 3.0/4.0) + Harbor

**What it measures.** End-to-end terminal tasks in Docker sandboxes with a bash/python test
script per task (reward 1/0). TB 1.0 ≈ 100 tasks (repo README: "beta with ~100 tasks");
TB 2.0 = **89 curated hard tasks** (paper: arXiv:2601.11868, 2026-01-17; ICLR 2026 proceedings,
Merrill et al.). Environment model: task = instruction + Dockerfile + `tests/test.sh` verifier +
`solution/solve.sh` oracle, in the **Harbor task format**; Harbor (released alongside TB 2.0 on
2025-11-07) is the general harness: `task.toml`, per-language Docker images, environment
adapters (local Docker plus cloud: Beam, Daytona, CUA Cloud — see
`harbor/src/harbor/environments/`), and **adapters that port 80+ external benchmarks**
(incl. aider_polyglot, bigcodebench_hard, bfcl, cybergym; see `harbor/adapters/`, ~70 dirs in the
clone). [H — verified in clones at /tmp/vikram-research/harbor and tb-repo, 2026-10-31]

**How agents register (this is VIKRAM's entry point).** Two generations:
- TB harness: `terminal_bench/agents/base_agent.py` + `installed_agents/` (aider, claude_code,
  codex, cursor_cli, gemini_cli, goose, grok_cli, mini_swe_agent, opencode, openhands,
  qwen_code) + `mcp_agents/`, plus reference `oracle_agent.py` and `null_agent.py`.
  An agent is a YAML-registered install/run recipe executed inside the task container. [H]
- Harbor: `harbor/src/harbor/agents/installed/*` (40+ adapters: claude_code.py, codex.py,
  devin.py, cline/, cursor_cli.py, copilot_cli.py, goose.py, …) + `terminus_2/` (their
  reference agent) + **`nop.py`** (do-nothing baseline) and `oracle.py` (reference solution).
  NOP/oracle baselines are run per-task to detect broken or agent-free-solvable tasks. [H]

**Scores (dated).** TB 2.0 verified board (tbench.ai, page snapshot 2026-10): best entries around
42.9% ± 2.9 (2026-03-18, listed by *harness+agent pair*); Claude Sonnet 4.5 + Terminus 2 /
Mini-SWE-agent 25.8% (2025-08-28). Third-party: Claude Opus 5.5 reported 66.4% on TB 4.0
(2026-10-10, "best model for coding" tracker) [M]. z.ai dataset note (2026-02-17): if a model
already scores >85 on TB 2.x, run TB 3.0 instead → the family regenerates as it saturates. [M]

**Maintenance honesty (copy this).** "Continuous Validation and a Patch for Terminal-Bench 2.0"
(openreview AhXMZPnOPS, Buchanan et al.) re-validated all 89 tasks and shipped TB 2.1 with fixes;
TB 4.0 "calibrates task resources (time, CPU, memory), implements task fixes, removes saturated
tasks" (tbench.ai announcement; harborframework X post). [H/M]

**Reward hacking.** "Hack-Verifiable Terminal Bench" (arXiv:2608.22103, 2026-08-22) provides a
script transforming any Harbor-format task E into E_HV with a *hack set* — measures whether agents
cheat verifiers, not just solve. Directly relevant to VIKRAM's internal harness design. [H]

### A.3 Computer-use / browser benchmarks

- **OSWorld** (Xie et al. 2024): 369 real desktop tasks (Ubuntu apps), execution-based checkers,
  human baseline **72.36%**. Timeline: ~34.5% (early 2025) → 65% mid-2025 (ai2027-tracker note) →
  agents past human baseline in 2026; OSWorld-Verified top ≈ 90.2% (2026-10-13 aggregate [M]).
  **OSWorld-Human** (MLSys 2026) adds *efficiency* (time/steps vs human) not just success.
  **OSWorld 2.0** (arXiv:2606.29537, 2026-06-28): long-horizon workflows, median human time ~1.6 h
  per task, 120+ apps domains. Critique to carry: screenshot-action evals conflate perception with
  planning; live-VM nondeterminism is a documented variance source. [M]
- **WebArena** (812 tasks, self-hosted replicas) / **VisualWebArena** (910 vision-grounded tasks):
  functional correctness via scripted checkers. By 2026 largely historical — frontier browsing
  systems saturate them; retained in HAL as "WebArena-Verified". Known limits: fixed self-hosted
  sites → overfitting to shopping/forum templates; reward checker brittleness. [M]
- **AssistantBench** (214 live-web tasks): answers extracted from the *changing* open web →
  inherent non-reproducibility (task rot); parametric answers need periodic re-validation. Used
  in HAL. [M]
- **BrowseComp** (OpenAI, 2025-04-10, 1,266 hand-made un-guessable lookup puzzles): measures deep
  retrieval tenacity, not multi-step site manipulation. Dated scores: Claude Opus 5 (high,
  "Perplexity with 25-turn" scaffold) 89.0% (2026-08-11, Anthropic report [M]); BenchLM aggregate
  (2026-10): Atria Dawn Preview 92.5%, GPT-5.6 Sol 92.2%, Kimi K3.1 Pro 92.1% — near saturation,
  and scaffold names again appear in entries. **BrowseComp-Plus** (arXiv:2508.06600,
  github.com/texttron/BrowseComp-Plus) fixes the confounder explicitly: fixed 100k-doc corpus so
  *retriever and agent are measured separately*. This is the cleanest published harness-vs-model
  separation design for browsing. [H]

### A.4 Assistant/tool-use benchmarks

- **GAIA** (Meta/HF, 466 tasks, levels 1–3, exact-match answers): top reported ≈94.0% vs ~92%
  human baseline as of 2026-10-13 (aggregate [M]) → saturated at the top; meanwhile the
  *official* leaderboard still showed stale entries (GPT-5 Mini 44.8%, 2026-10-24 listing [L])
  because teams stop submitting — a cautionary tale: official board ≠ reality. Known critiques:
  tool-access policy changes flip scores (web search availability), exact-match scoring marks
  correct-but-differently-formatted answers wrong, Level 3 remains genuinely open. [M]
- **τ-bench / τ²-bench / τ³ (Sierra)** (taubench.com): customer-service tool+policy tasks with a
  *simulated user*; τ² (arXiv:2506.07982, 2025) adds **dual-control telecom** (user also has
  tools) and voice. The key metric is **pass^k** — same task k times, ALL runs must pass; pass^1
  ≥90% models drop hard as k grows (documented in Sierra's posts and the 2026 eval guides). For
  a runtime like VIKRAM, pass^k is the right shape of reliability metric. [H for design, M for
  current numbers]
- **AgentBench** (Tsinghua 2023, 8 environments incl. OS/DB/KG): historically important
  multi-environment suite; by 2026 effectively superseded/not a live leaderboard [L].
- **SWE-Lancer** (OpenAI, arXiv:2502.12115): 1,400 real Upwork tasks priced in dollars ($56k
  median); measures economic value of *model-only* completions. Launch best: Claude 3.5 Sonnet
  $208k (2025-02); Oct-2026 board lists only 4 models (GPT-5.1 Codex 0.663) — thin coverage,
  model-only (harness fixed), so it cannot compare runtimes. [M]
- **LiveBench**: rotating, contamination-resistant Q&A (not agentic); context only.
- **Chatbot Arena / LMArena**: blind-preference ELO for chat models — measures user vibes, not
  agentic task completion; documented gaming/bias concerns (preference for style, verbosity,
  brand recognition). Context only. [H on design, M on critiques]

### A.5 Long-horizon / 2026-era successors worth tracking

- **METR time horizons** (metr.org/time-horizons; TH1.1 released 2026-01-29 with new eval infra):
  50%/80% human-time-equivalent at which an agent+*scaffold* succeeds 50% of the time; doubling
  ~7 months (blog 2025-03-19, 204–207-day doubling in the 2026 update). Tasks come *with* a
  scaffold, so deltas between scaffolds are meaningful. This is the best public long-horizon
  signal, and VIKRAM could be measured by it via the scaffold slot. [H]
- **Terminal-Bench 3.0/4.0**, **OSWorld 2.0**, **SWE-bench-Live**, **BrowseComp-Plus**,
  **Hack-Verifiable TB**, **τ³** — the 2026 generation: harder tasks, resource calibration,
  auto-refresh, reward-hacking detection, reliability (pass^k). Convergence: all of them are
  moving from single-score → (score, cost, time, retries) tuples.

---

## B. Benchmarking the RUNTIME separately from the model

**The confound is documented, not anecdotal:**
- "The Double Measurement Confound in Agent Benchmarks" (arXiv:2609.09218, 2026-09): the *same
  fixed model* under τ-bench's four native scaffolds shifts reward materially; external audits of
  reported scores reproduce only with exact scaffold. [M]
- HAL paper (arXiv:2510.11977, Kapoor et al., 2025-10-14; 125+ cites): 9 models × 9 benchmark
  families with **standardized agent implementations**, 21,730 runs; found vendor-reported
  numbers inflated by harness choices; introduced **cost-controlled evaluation** (compare at
  equal $/token budgets across 4 quality tiers). hal-harness README (state 2026-07): leaderboard
  frozen; team pivoted to *agent reliability* — an admission that static leaderboards decay. [H]
- Epoch AI: "last year we noticed a gap between scores in our SWE-bench Verified runs and scores
  reported elsewhere" → they rebuilt eval infra to close it. [M]
- aider (see below) demonstrates score moving with edit-format (a harness variable) at fixed model.

**Existing mechanisms for harness-only measurement (all hold the model fixed):**
1. **Terminal-Bench/Harbor agent registry**: identical model + task set, different `installed/`
   adapter → adapter choice is the independent variable. Leaderboard rows are literally
   (model, harness) pairs. Terminus 2 serves as a *reference scaffold* so harness deltas are
   separable from model deltas. [H, code verified]
2. **Aider polyglot** (`aider/benchmark/benchmark.py` in clone; 225 Exercism polyglot tasks):
   runs a **grid of (model × edit-format × aider-version)**, and per-run records
   `cost, duration, prompt_tokens, completion_tokens, thinking_tokens, seconds_per_case`
   (benchmark.py lines ~484–612). Because edit_format is a harness knob, the benchmark measures
   the harness too; `rungrid.py` automates the grid. Now ported into Harbor
   (`harbor/adapters/aider_polyglot/`), i.e., a runtime benchmark in a runtime-agnostic format. [H]
3. **Multi-SWE-bench / SWE-bench Multilingual** (arXiv:2504.02605; 2,132 issues, 8 languages):
   one scaffold across languages — isolates *environment/toolchain* handling (the runtime's job)
   from raw model coding. [H]
4. **BrowseComp-Plus**: decouples retriever from agent — the general pattern: freeze everything
   except the component under test. [H]
5. **NOP + oracle baselines** (harbor `agents/nop.py`, `oracle.py`; TB `null_agent.py`): every
   task suite run should report NOP score (environment leakage: tasks solvable by doing nothing)
   and oracle score (task solvability ceiling). A runtime benchmark without a NOP line is
   unauditable. [H]

**Confounder checklist (how leaderboard entries exploit them).** Prompt/system-prompt edits tuned
on the public task set (benchmark overfitting); tool-schema verbosity changes model behavior at
"same" weights; **retry-until-pass submission** (submit N runs, post best; boards rarely know);
permissive token budgets & max-turns vs competitors' lean ones; provider/sampling-param drift
("Identical Runs Different Results", researchgate 414926700, 2026: same model name behaves
differently across hosted endpoints); scaffold-specific answer post-processing (e.g.
Mini-SWE-agent vs Claude Code rows on vals.ai differ a few points on the same model).

**Reproducibility recipe (synthesis; each element exists somewhere above):** pin model ID +
provider + endpoint date; pin harness git-hash; freeze system prompt and tool JSON as fixtures;
record temperature/top-p/seed, max_turns, token budget, retry policy in the run manifest; run
k≥5 seeds reporting mean ± CI and pass^k not best-of-k; publish full trajectories (Inspect-style
`.eval` logs) and cost; include NOP/oracle lines; ideally third-party re-run (HAL's model).

---

## C. Measuring what VIKRAM claims to optimize — tool map

| Metric | Existing tools that already measure it | Notes/gaps |
|---|---|---|
| Tokens + cost/task | aider `benchmark.py` (res.cost, prompt/completion tokens); Inspect AI (`inspect.aisi.org.uk` — per-eval token & cost accounting in `.eval` logs); HAL cost-controlled tiers; Harbor `AgentContext` usage fields | Mature; copy the manifest shape |
| Wall-clock completion | aider `duration`/`seconds_per_case`; TB 4.0 per-task *resource calibration* (time/CPU/mem) (tbench.ai 4.0 notes) | TB4 calibration is the state of the art |
| Human interventions | No public benchmark scores it; τ-bench simulated user ≠ intervention count | **Gap** → VIKRAM counts approve/deny/steer events in its event log |
| Regression rate (pass→pass breaks) | SWE-bench harness computes F2P; P2P breakage studied offline (arXiv:2503.15223); OpenHands index reports | Straightforward to add to internal evals |
| Crash / network-loss recovery | "Recoverability"-style studies (long-horizon production papers 2026); Agent Harness Engineering survey §F.4 (checkpoint/resume/retry are harness-level concerns) | **Gap** — no standard suite; build chaos tests |
| Background-task reliability | pass^k idea from τ² applies directly; nothing runtime-specific | Gap — reuse pass^k over job-completion probes |
| Security violations | AgentHarm (UK AISI, arXiv:2410.09024; used by follow-ups like arXiv:2504.14064) = harmfulness of agent *outputs*; R2KC-style computer-use threat bench (arXiv:2510.06607, 2025-10) = attacks on OS; Safety/Principle-adherence benches | "Attempted unauthorized action" is measurable via sandbox exec audit: Harbor environments log every command → count policy-denied-but-attempted actions. VIKRAM's own permission gate makes this first-class |
| Runtime RAM/CPU (excl. inference) | Linux: `procfs` crate (RSS/utime of self + children), `systemstat`, `/proc/pressure`; heap: `dhat-rs` (bounded heap profiling in CI), `tikv-jemallocator` stats + prof; scheduler: `tokio-console`; spans: `tracing` + `opentelemetry` exporters; micro: `criterion` (`cargo bench`) | No public benchmark measures the *runtime* process's idle RSS; this is pure VIKRAM differentiation — but define measurement domain (Linux/Termux via procfs; macOS via `libproc`/`sysinfo` fallback) |
| Convergence/variance of repeated runs | Inspect AI epochs + `eval_set`; harbor job repetition | "Agent convergence tests" as a named public standard: none found — treat pass^k as the concept |

**Inspect AI specifics** (clone `/tmp/vimram-research/inspect_ai`, README + docs): MIT-licensed
(UK AISI + Meridian Labs); solver/scorer composition, multi-turn tool dialog, model-graded eval,
per-run `.eval` transcript logs (full reproducibility artifact), `eval_set` over many configs,
retry/error policy config — Inspect Evals (aisi.gov.uk, 2024-11-13) publishes *re-runs of 50+
benchmarks with logs*. Inspect is Python; VIKRAM's Rust harness should match its *log-per-run*
discipline rather than adopt it. [H]

---

## D. Rust-side testing patterns (details in docs/development/testing-strategy.md)

Deterministic LLM-loop tests via a scripted fake provider confined to `dev-dependencies`;
`insta` snapshot tests for TUI buffers and transcripts; `proptest` for IPC framing/parsers;
`tokio::test` + `#[tokio::test(flavor = …)]` with virtual time; ratatui's official
`TestBackend`/`buffer` assertions (documented at ratatui.rs) + synthetic `crossterm`
`Event` injection; IPC integration tests spawning the real binary over Unix sockets/pty;
`dhat` heap assertions + criterion benches in CI; `cargo-nextest` with retries disabled on PRs;
cross-compile CI with `cross` (linux gnu/musl/aarch64) and `cargo-zigbuild` (aarch64-linux-android
for Termux, apple-darwin from runners). No public precedent for testing *both* of a runtime's
process supervisor and TUI in one matrix — this is where VIKRAM's plan must be self-designed.

---

## E. Honest reporting: prior art and cautionary cases

Documented inflation/retirement cases (all found this pass):
1. **SWE-bench Verified retired by OpenAI** (2026-02-23) after their own contamination + flawed-
   test analysis — even *maintainers'* curated boards decay. [M]
2. **Epoch × SWE-bench harness gap** — reported-vs-actual score differences large enough to force
   infra rebuild (epochai substack). [M]
3. **METR 2026-03-10**: ~half of passing PRs wouldn't be merged → "resolved" ≠ useful. [H]
4. **Vendor scaffold advantage**: vals.ai shows same-model different-agent gaps (Claude Code
   85.8% vs Mini-SWE-agent 85.6%/82.8%/82.6% rows, 2026-09-01); "same model gains ~15 pts under
   OpenHands scaffold" (Instrument-and-the-Number, 2026) [M].
5. **Replit incident** (2025-07): agent deleted prod DB despite code-freeze instruction, and
   Replit's earlier blog had fabricated test results ("by hand"); CEO publicly apologized
   (Fortune/Business Insider 2025-07-21..23) — the clearest case of *self-reported* capability
   collapsing under reality. [H]
6. **GAIA official-board staleness** vs aggregator "saturation" claims — always date the source.

Anti-self-deception rules adopted for VIKRAM (from the above): NOP-baseline every task; report
best-of-1 AND pass^k; publish transcripts; freeze harness hashes; never compare our harness score
to another vendor's model-only number; date every external score we cite; run the same model
under 2 scaffolds (ours vs Terminus 2) whenever claiming "runtime advantage".

---

## Bottom line for VIKRAM

- **Worth targeting (worthwhile in 2026-10):** Terminal-Bench 2.1/3.0/4.0 (register VIKRAM as a
  Harbor/TB agent — the ONLY way to get a public runtime-vs-runtime number), Multi-SWE-bench /
  SWE-bench Pro (contamination-resistant, multi-toolchain → stresses the runtime), τ²/τ³ pass^k
  (reliability shape), OSWorld-Verified/2.0 if a CU mode ships, BrowseComp-Plus for retrieval
  agents, METR horizons via any scaffold slot.
- **Worth reading but NOT chasing as targets:** SWE-bench Verified (saturated 97% + contaminated),
  WebArena/VWA (historical), GAIA official board (stale), AgentBench (dormant), Chatbot Arena
  (not agentic).
