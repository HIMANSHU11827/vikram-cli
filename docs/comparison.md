# VIKRAM vs. other agent frameworks — a parity table with evidence

Status: 2026-11-07. Scope: VIKRAM (this repo), Hermes, Claude Code, Codex CLI, OpenCode, Goose,
Antigravity, Agent Zero.

This document exists because comparison tables are usually invented. Everything below is either
**verified by reading source that is present in this repo**, **documented by a research note that is
present in this repo** (which itself cites upstream `file:line`), or marked **unverified**. Nothing
is upgraded to a claim because it would look good.

---

## 1. Evidence legend

| Mark | Meaning |
|---|---|
| **✓** | Verified here: I read the source in this repo. The command that reproduces the finding is given in §5 (VIKRAM) or in the cell's note (Hermes / Codex CLI, whose source is vendored). |
| **◐** | Documented only: a research note in `docs/research/` asserts it with an upstream citation. I did **not** re-verify it against upstream source, and it is not a VIKRAM claim. |
| **absent (checked)** | I looked at the vendored source / crate list and found nothing. This is a statement about the vendored snapshot, not a promise about the upstream project. |
| **unverified** | No evidence in this repo. Treated as *unknown*, never as *missing*. |

Two frameworks the request names — **Antigravity** and **Agent Zero** — have **no capability evidence**
in this repository. `grep -ril "agent.zero" . --exclude-dir=target` matches only this document.
`grep -ril "antigravity" . --exclude-dir=target` matches only incidental mentions inside two vendored
*other* projects: `inspect_ai/CHANGELOG.md` (a line about "Antigravity's `call_mcp_tool`" being
bridged) and `openhands2/.../desktop-electron/references/guide.md` (its `.app` bundle name). Neither
is source for Antigravity, so every cell for both frameworks is `unverified`, and no other statement
about them is made here.

What *is* vendored, and therefore checkable:

| Project | Where | What it is |
|---|---|---|
| Hermes | `research/hermes-platforms/` | 27 gateway plugin directories (Python), 162 `.py` files, 70 164 lines |
| Codex CLI | `codex/` | 125 top-level directories under `codex/codex-rs/` (117 crates), 5 702 `.rs` files, 2 220 355 lines |
| Codex / Goose / OpenCode / Claude Code / Gemini CLI / OpenHands / Aider research | `docs/research/competitors/`, `docs/research/implementations/`, `docs/research/skills/` | source-level competitor notes with citations |
| OpenHands | `openhands2/` | vendored, not one of the seven frameworks in scope |

The Hermes subset is the **gateway plugins only** — 27 platform adapters, no agent core, no tests
(`find research/hermes-platforms -name 'test_*.py' | wc -l` → `0`). Hermes cells outside the gateway
dimension are therefore `unverified` rather than "no".

---

## 2. Feature / parity table

| Dimension | VIKRAM | Hermes | Claude Code | Codex CLI | OpenCode | Goose | Antigravity | Agent Zero |
|---|---|---|---|---|---|---|---|---|
| **Platforms (chat gateways)** | ✓ 28 adapter modules in `crates/vikram-gateway/src/platforms/` + registry, pairing, access control, routing, delivery ledger | ✓ 27 platform dirs, each with `adapter.py` + `plugin.yaml` | unverified | absent (checked): no messaging-adapter crate among the 125; `slack`/`notion` appear only as plugin ids and MCP server names in tests | unverified | unverified | unverified | unverified |
| **Media (in/out)** | ✓ 19 of 28 gateway modules implement `send_media` for attachments/media; `Block::Image` + `ImageSource::{Url,Data}` in contracts; per-adapter media types and filenames | ✓ 26 of 27 adapters reference media/attachments | unverified | ✓ `attachment-store` crate; image handling present | unverified | unverified | unverified | unverified |
| **MCP** | ✓ client + server + discovery + visibility (`crates/vikram-mcp`, 5 modules; JSON-RPC 2.0 over stdio) | unverified (one `mcp` mention in `discord/adapter.py`; no MCP client/server in the subset) | ◐ plugins may ship MCP | ✓ `rmcp-client` crate + `rmcp` workspace pin | ◐ client, oauth, stdio, socket | ◐ MCP-first: `ExtensionConfig::{Stdio,Builtin,StreamableHttp}` | unverified | unverified |
| **Skills / plugins** | ✓ `vikram-skills` (15 modules: `SKILL.md` + frontmatter, progressive load, bundles, install, history, trust) and 9 plugin tools in the runtime | ✓ `plugin.yaml` manifest per platform adapter (27); no agent-skill system in the subset | ◐ skills, plugins, hooks | ✓ `skills`, `plugin`, `plugin-host`, `hooks` crates | ◐ `PluginHooks` namespaced events | ◐ extensions + recipes | unverified | unverified |
| **Sandboxing** | ✓ `vikram-sandbox`: bubblewrap (`bwrap`), macOS `sandbox-exec`/Seatbelt profile, Windows Job objects; policy floor independent of the mechanism | unverified | ◐ `--restricted`, sandbox refs marked public/ASSUMPTION | ✓ `sandboxing`, `bwrap`, `linux-sandbox`, `windows-sandbox-rs`, `mxc-sandbox` crates | ◐ none (permission rules only) | ◐ none (approvals + policy) | unverified | unverified |
| **Subagents / teams** | ✓ `agents.rs`, `teams.rs`, `hive_chat.rs`; tools `team.create/list/delegate`, `hive.create/delegate`; delegation and hive token budgets in `ContextConfig` | unverified | ◐ `agent.register/spawn`, `teammate`, subagent types | ✓ subagent references in 417 files; `agent-roles`, `runtime-agent-instance` | ◐ child session, background jobs | ◐ `SubagentOperation`, concurrency cap | unverified | unverified |
| **Persistence / memory** | ✓ SQLite journal (`vikram-store`, WAL, fork/duplicate/export, interrupted-session recovery) + `memory.rs` FTS5 hybrid recall with MMR rerank + `memory_files` | unverified (sqlite mentioned in 5 adapter files; no store crate in the subset) | ◐ resume/plan restore | ✓ `rollout`, `thread-store`, `memories`, `message-history` crates | ◐ SQLite event-sourced | ◐ SQLite session manager + migrations | unverified | unverified |
| **Browser / computer** | ✓ 12 browser tools over CDP (`browser/{cdp,dom,capture,discovery}.rs`) + Linux computer use via `xdotool`/`scrot`/`wmctrl` (`computer.rs`) | unverified | unverified | ◐ config surface only (`app-server-protocol` `browser_use_config`); no driver crate found | unverified | unverified | unverified | unverified |
| **TUI / CLI** | ✓ ratatui + crossterm TUI (`vikram-tui`), clap CLI (`vikram-cli`), background daemon (`vikram-daemon`) | unverified (gateway adapters only) | ◐ CLI/TUI product, closed source | ✓ `tui`, `cli`, `exec` crates | ◐ TUI | ◐ CLI | unverified | unverified |
| **ACP** | ✓ client **and** server, JSON-RPC 2.0 on stdio (`AcpClient`, `AcpServer` in `vikram-acp`) | unverified | unverified | absent (checked): no `agent-client-protocol`/`acp` match in `codex-rs` | ◐ ACP server | ◐ exposes ACP | unverified | unverified |
| **Providers** | ✓ one OpenAI-compatible `/chat/completions` seam; `base_url` + `model` + `api_key_env` from config. No per-vendor adapters — any compatible endpoint works, none is special-cased | unverified | ◐ Anthropic-first + Bedrock/Vertex/gateway | ✓ `model-provider` with Amazon Bedrock, GCP, Ollama, LM Studio alongside OpenAI | ◐ provider-agnostic catalog + variants | ◐ 11 engines + 53 declarative providers | unverified | unverified |
| **Licensing** | ✓ `MIT OR Apache-2.0` (`Cargo.toml` `[workspace.package] license`, `LICENSE-MIT`, `LICENSE-APACHE`) | unverified (no LICENSE in the vendored subset; `plugin.yaml` says `author: NousResearch`) | unverified | ✓ Apache-2.0 (`codex/LICENSE`) | unverified | unverified | unverified | unverified |

Reading the table: VIKRAM's row is dense because this is VIKRAM's repository. **A dense VIKRAM row
is not a claim of superiority** — it is the one column that could be checked line by line. The
`◐` column for Goose, OpenCode and Claude Code is stronger evidence than `unverified`, and the
Codex CLI column is nearly as well evidenced as VIKRAM's because its source is vendored too.

---

## 3. Notes on the evidenced third-party columns

### Hermes (`research/hermes-platforms/`)

* 27 adapters against VIKRAM's 28 modules. The difference is explained, not inferred: VIKRAM also
  ships `dingtalk_stream` as its own module, and splits the shared pieces (registry, pairing,
  access, routing, delivery) out of the adapters into `vikram-gateway/src/*.rs`.
* The adapters are richer than a name list: `telegram/` alone carries `dashboard_card.py`,
  `inline_picker.py`, `telegram_topic_routing.py`, `update_admission.py`, and the port target is
  named in `docs/gateway-port-plan.md`.
* No MCP client/server, no agent loop, no tests in the vendored subset — those rows are `unverified`
  rather than "absent", because the subset is not the whole project.

### Codex CLI (`codex/`)

Verifiable by crate name alone (117 crates with a `Cargo.toml`, 125 top-level directories, under
`codex/codex-rs/`): `tui`, `cli`, `exec`,
`rmcp-client`, `skills`, `plugin`, `plugin-host`, `hooks`, `sandboxing`, `bwrap`, `linux-sandbox`,
`windows-sandbox-rs`, `mxc-sandbox`, `model-provider` (with `amazon_bedrock`, `gcp`, `ollama`,
`lmstudio`), `rollout`, `thread-store`, `memories`, `worktree`, `agent-roles`, `app-server`.
Apache-2.0.

### Claude Code, OpenCode, Goose

Evidence is `docs/research/competitors/coding-agents.md` and
`docs/research/implementations/coding-agent-internals.md`, both of which cite upstream `file:line`
and mark assumptions as assumptions. Claude Code is closed source, so its row is public-evidence
only. Treat all `◐` cells as "someone read the upstream source and wrote down where"; they are not
VIKRAM measurements.

---

## 4. Benchmark methodology (`vikram-bench`), and what it does not claim

`crates/vikram-bench` is an in-process harness. It measures VIKRAM's own code paths on the machine
it runs on and prints a table; it measures **nothing** about any other framework, and **no
cross-framework performance number is claimed anywhere in this document**.

* **Timing**: `std::time::Instant` around every individual iteration. No sampling profiler, no
  wall-clock of a shelled-out process, no estimates.
* **Shape**: 100 untimed warm-up iterations, then 5 timed runs × 1000 iterations per operation.
  Median and p95 are nearest-rank over all 5 000 per-iteration samples, so one slow iteration shows
  up in the p95 instead of being averaged away.
* **Floor**: a per-iteration `Instant` pair costs roughly 20–40 ns; rows below a microsecond are
  upper bounds, not exact costs. This is printed in the harness header rather than silently
  subtracted.
* **Excluded on purpose** (needs the network, a credential, or a TTY): model completions,
  `web.search`, real gateway delivery, MCP handshakes, browser automation, cloud providers,
  terminal rendering, and process spawn / sandbox set-up. Every gateway and tool path that *is*
  measured runs over a transport that refuses to send anything.
* **Honesty checks in the loop**: each iteration verifies it did the real work (config parsed the
  seeded file, 250 sessions listed, 250 journal records replayed, ≥2 chunks produced with content
  retained, 1 webhook event with 2 attachments parsed, the tool returned its argument). A silent
  no-op fails the run; the binary exits non-zero on any error.

### One measured run (this host, not a portable claim)

Linux x86_64, rustc 1.99.1, 8 cores, release profile, 2026-11-07.
Raw output: `cargo run -p vikram-bench --release`.

| operation | iterations | median | p95 | unit |
|---|---:|---:|---:|---|
| `config.load` | 5000 | 3.26 us | 4.18 us | per load (fs read + JSON parse) |
| `config.parse_json` | 5000 | 1.28 us | 1.57 us | per parse (no file read) |
| `store.session.insert` | 5000 | 7.70 us | 13.88 us | per session insert |
| `store.journal.append` | 5000 | 10.38 us | 20.38 us | per journaled message record |
| `store.list_sessions` | 5000 | 197.13 us | 274.10 us | per list of 250 active sessions |
| `store.replay_messages` | 5000 | 283.29 us | 404.70 us | per replay of 250 journal records |
| `gateway.truncate_message` | 5000 | 0.26 ms | 0.37 ms | per 64 KiB chunking (max_len=4096) |
| `gateway.webhook_parse` | 5000 | 2.32 us | 2.45 us | per webhook parse (1 event, 2 attachments) |
| `contracts.message.to_json` | 5000 | 260 ns | 310 ns | per `Message` serialisation |
| `contracts.message.from_json` | 5000 | 841 ns | 904 ns | per `Message` parse |
| `runtime.tool_dispatch` | 5000 | 151 ns | 194 ns | per registry lookup + `Tool::run` |
| `codeindex.ranked_context` | 5000 | 59.55 us | 68.26 us | per query over a 60-file / 1200-symbol index |
| `memory.recall` | 5000 | 660.52 us | 736.53 us | per hybrid recall over 200 memories |

Run-to-run spread on this host was up to ~30 % between two consecutive runs for the slower rows
(the machine was building other crates at the time), so read these as orders of magnitude and
re-run before quoting them. The shape of the table — what is microseconds and what is milliseconds —
was stable across runs.

Two things the numbers say out loud, including where they are unflattering:

* `gateway.truncate_message` began at ~5 ms for 64 KiB because the greedy loop re-rendered and
  re-measured the growing frame for every appended line. It now tracks each frame's rendered length
  incrementally (`helpers.rs`, with a differential test against the old implementation), so the same
  input costs ~0.26 ms and scales linearly; a 16→256 KiB probe measured 105 µs → 1.19 ms.
* `memory.recall` at ~0.7 ms per turn is now the largest fixed cost in the measured set — it fuses
  a raw `LIKE` sweep, FTS5 BM25, recency/importance and an MMR rerank in one call.

Numbers on other hardware, other toolchains, or debug builds will differ; the harness prints the
build profile and warns when it is not release.

---

## 5. How to verify the VIKRAM numbers in this document

Run from the repository root. Each command prints the value quoted above.

```bash
# 16 workspace crates (includes vikram-bench)
ls -d crates/*/ | wc -l                                   # 16

# 222 Rust files, 171886 lines (this tree is edited concurrently — re-run for today's count)
find crates -name '*.rs' | wc -l                          # 222
find crates -name '*.rs' -exec cat {} + | wc -l           # 171886

# 2357 unit/integration test annotations across the workspace
grep -r "#\[test\]\|#\[tokio::test\]" crates/ --include=*.rs | wc -l   # 2357

# 28 gateway platform modules; 26 implement PlatformAdapter, 19 implement send_media
ls crates/vikram-gateway/src/platforms/*.rs | grep -v mod.rs | wc -l   # 28
grep -l "impl PlatformAdapter for" crates/vikram-gateway/src/platforms/*.rs | wc -l  # 26
grep -l "fn supports_media(&self) -> bool {" crates/vikram-gateway/src/platforms/*.rs \
  | xargs grep -l "true" | wc -l                                      # 19

# 71 built-in tool registrations
grep -c "registry.register(Box::new(" crates/vikram-runtime/src/tools/core.rs       # 71

# MCP: 5 modules (client, server, discovery, visibility, lib)
ls crates/vikram-mcp/src/*.rs | wc -l                     # 5

# Skills: 15 modules
ls crates/vikram-skills/src/*.rs | wc -l                  # 15

# Sandbox mechanisms
grep -n "bwrap\|sandbox-exec\|JobObjects" crates/vikram-sandbox/src/lib.rs

# ACP: client and server
grep -n "pub struct AcpClient\|pub struct AcpServer" crates/vikram-acp/src/lib.rs

# Provider seam: one OpenAI-compatible endpoint builder, no per-vendor adapters
grep -rn "chat/completions" crates/vikram-runtime/src/openai.rs

# Licence
grep -n "license" Cargo.toml                            # line 24: MIT OR Apache-2.0

# Hermes vendored subset: 27 platforms, 162 py files, 70164 lines, 0 tests
ls -d research/hermes-platforms/*/ | wc -l                # 27
find research/hermes-platforms -name '*.py' | wc -l       # 162
find research/hermes-platforms -name '*.py' -exec cat {} + | wc -l  # 70164
find research/hermes-platforms -name 'test_*.py' | wc -l  # 0

# Codex CLI vendored: 117 crates / 125 top-level dirs, 5702 rs files, 2220355 lines, Apache-2.0
find codex/codex-rs -mindepth 1 -maxdepth 1 -type d | wc -l        # 125
find codex/codex-rs -mindepth 2 -maxdepth 2 -name Cargo.toml | wc -l  # 117
find codex/codex-rs -name '*.rs' | wc -l                  # 5702
find codex/codex-rs -name '*.rs' -exec cat {} + | wc -l   # 2220355
head -3 codex/LICENSE                                     # Apache License 2.0

# Antigravity / Agent Zero: no capability evidence in this repo
grep -ril "antigravity" . --exclude-dir=target            # only inspect_ai + openhands2 mentions
grep -ril "agent.zero" . --exclude-dir=target             # only docs/comparison.md

# Reproduce the benchmark table
cargo run -p vikram-bench --release
```

---

## 6. Honest gaps in this comparison

* **Antigravity and Agent Zero**: no capability evidence in this repo — the only matches are
  incidental mentions of Antigravity inside two unrelated vendored projects (`inspect_ai`,
  `openhands2`), and Agent Zero is named nowhere but this file. Every cell for both says
  `unverified`; a filled-in row would be invention.
* **Claude Code** is closed source. Its `◐` cells rest on public evidence collected in
  `docs/research/`, not on source in this repo.
* **Nothing here is a performance comparison.** The only measured numbers are VIKRAM's, produced by
  `vikram-bench` on one host. Cross-framework latency, throughput or token-efficiency numbers are
  not claimed, not estimated, and not implied by the ordering of the table.
* **`unverified` ≠ absent.** Five of the seven other frameworks have no vendored source here, so this
  document can only say "no evidence", which is a statement about this repository.
* VIKRAM's own thin spots, recorded rather than hidden: one provider family (OpenAI-compatible
  only, no vendor SDKs), a young ACP implementation (2 tests), and eight gateway modules without
  outbound media (A2A artifacts, Buzz/Buzz CLI, Google Chat user OAuth, SimpleX XFTP, and
  IRC/ntfy/raft/SMS, which have no media path in the reference).


## 7. Where the remaining work is tracked

The ordered plan for closing the gaps in this table is [`docs/roadmap.md`](roadmap.md):
P0 covers inbound-media hardening, ledger coverage and provider conformance; P1 covers voice
notes, remaining cards/reactions/polls and the last media adapters; P2 covers extra providers,
evolution actuators and the performance gate. Nothing in this document claims a capability that
is not already in the repository.
