# Harness Redesign — why Vikram feels basic, and what to change

Date: 2026-11-22
Scope: the agent harness only — turn loop, context assembly, compaction, tool execution, policy/trust, approvals, memory, orchestration, providers.
Not in scope: gateway/platform adapters, TUI visual design, evolution engines beyond their entry points.

Baseline: working tree at `HEAD c8835ba` **plus uncommitted worker edits** (`git status` shows `vikram-policy/src/lib.rs +168`, `vikram-runtime/src/session.rs +54`, `orchestrator.rs` untracked, voice changes). Every line number below refers to that working tree, not to `c8835ba` in isolation. Workers were editing while I read — during this session `turn.rs`, `cli/main.rs` and `tools/tests.rs` also picked up uncommitted edits — so trust the symbol name over the line number and re-grep before quoting a line in a commit message.

---

## 0. Method and evidence tiers

Every claim is tagged with what I actually did:

- **[read]** — I opened the file and read the cited lines.
- **[counted]** — a mechanical count I ran myself; command shape given so it is reproducible.
- **[repro]** — I reproduced the mechanism in an isolated program under `/tmp/opencode/probe`, not in this repo. Observed, not inferred.
- **[knowledge]** — from knowledge of a closed or non-vendored harness. No source in this repo. A hypothesis to test, not a citation.
- **[unverified]** — did not check. Stated as unknown rather than guessed.

I read at source level: `crates/vikram-runtime` (84 files, 67,974 lines — [counted] `find … | wc -l`), `crates/vikram-policy` (1,745), `crates/vikram-contracts`, `crates/vikram-store` (hit-list), `crates/vikram-cli` (surface), `crates/vikram-tui` (render/approval surface), `crates/vikram-sandbox` (all 467), `crates/vikram-bench`, `crates/vikram-mcp`, `crates/vikram-evolution` (module docs + engine surface).

I did **not** read: `vikram-gateway` (80,900 lines), `vikram-evolution` internals beyond `lib.rs` and the wired paths, `vikram-daemon` beyond `triggers.rs`, `vikram-mcp` beyond `client.rs` signatures. Sections touching those are marked PARTIAL.

Comparative sources: vendored trees in this repo — `codex/codex-rs` (Rust, the internal Codex, materially larger than the public CLI), `inspect_ai/` (Python eval + agent framework), `openhands2/` (TypeScript UI shell), `gitui/` (Rust git TUI). Source-read with citations. Claude Code / OpenCode / Amp / Manus are **[knowledge]** only.

I did not run `cargo test` — other agents own the tree. Two claims below are backed by **[repro]** instead: I copied the exact code shape into `/tmp` and ran it with `rustc`.

---

## 1. Architecture As Read

### 1.1 The turn loop

`run_turn` (`turn.rs:179`) is a real, defensively written loop. Shape, in order:

- Turn claim taken **before** the task spawns, so two submits cannot both see an idle session (`turn.rs:~150`, comment explains the race).
- Ten stage points, each with a plugin hook and possibly a replacing native module: `MemoryRecall`, `ContextBuild`, `ProviderRequest`, `ToolDispatch`, `ResultCommit`, `UserInput`, `ResponseCommit`, `TurnEnd`, `SessionLifecycle`, `AgentLifecycle` (`pipeline.rs`; dispatch sites at `turn.rs:449`, `:469`, `:575`, `:918`). More structure than most harnesses expose.
- `TurnGuard` releases the turn claim on unwind, so a panicking turn cannot strand `is_busy` (`turn.rs:~190`).
- Streaming with a silence watchdog: `stream_silence_secs`, and the error tells the operator the exact `vikram config set` command to raise it (`turn.rs:649`, `:672`; config at `config.rs:54`).
- Step budget: `max_steps` default 24 (`config.rs:498`), read at `turn.rs:211`. **It is not a wall.** At `steps_used >= max_steps` the loop emits one warning and keeps going (`turn.rs:901-912`, comment: "A budget, not a wall"). The config field name promises a cap the code does not enforce.
- Context-limit rejection is recovered by dropping the oldest messages and retrying, up to 2 times (`turn.rs:773-793`).
- `RepeatGuard` (`turn.rs:1199`) counts repeated identical failures and nudges the turn at streak lengths of 3.
- End of turn: assistant text/reasoning/tool calls are committed as **one** assistant message before dispatch, with an explicit comment about why (`turn.rs:~860-895`); if the model asked for no calls, the loop breaks.

Tool dispatch is the strongest single piece: `dispatch_tool_calls` (`turn.rs:2717`) groups calls into **conflict lanes** by resource key and `join_all`s the lanes, so independent calls run concurrently and conflicting ones serialize (`turn.rs:2735-2760`). Results are committed as `Trust::External` messages (`commit_tool_results`, `turn.rs:2768`; `Message::tool_result` at `message.rs:144-153` sets `Trust::External`).

### 1.2 Context assembly

`assemble_context` (`turn.rs:2540`), in order, every step:

1. skill catalogue (metadata only; bodies loaded on demand) — `turn.rs:2547-2555`
2. project instructions from the context-file ladder (`project_context`, `runtime.rs:1489`)
3. turn voice: `PREAMBLE` (`turn.rs:75-105`) or a bot identity block (`bot_preamble`, `turn.rs:~108`)
4. retrieved memory (inserted at index 0) — `turn.rs:2594`
5. dialectic supplement — `turn.rs:2600`
6. code-index ranked definitions/references for the request, with grep fallback (`turn.rs:~2610-2650`)
7. plugin-injected context

Every layer is labelled and priced into `ContextMetered` (`turn.rs:499-540`), which the TUI shows. Token budget is resolved per model from a limits table with a measured-usage anchor (`context_engine.rs:75-310`), and `active_context_tokens` trusts the provider's reported number after the first response (`compaction.rs:168-184`). This is good design and is not the problem.

The problem is the content of layer 3. `PREAMBLE` is 30 lines of persona: "a friend and family member… never sugarcoat… write one or two short sentences of progress" (`turn.rs:75-105`). It contains **no** work contract: no plan discipline, no "read before you edit", no "run the test", no statement that tool output is untrusted data, no definition of done. Compare Codex's base instructions (§5.1).

AGENTS.md discovery is deliberately shallow: "Nearest AGENTS.md: workspace root, then any nested. **Just root + one level for now.**" (`context.rs:399`). Codex scopes instructions per directory tree with precedence rules (§5.1).

### 1.3 Compaction

Policy: trigger at 72% of usable, keep last 12, never fold fewer than 6, tool-output ceiling 6,000 (`compaction.rs:36-39`; defaults mirrored in `config.rs:417-433`). Runs before **every** request inside `prepare_context` (`runtime.rs:1575-1640`): fold once, then prune oversized tool results.

`compact()` (`compaction.rs:254`) is careful in the ways that break other harnesses:

- never splits a tool call from its result (`safe_split`, `compaction.rs:198`; `pairing_is_consistent`, `:228`)
- refuses a fold that frees less than a tenth of the window (`made_progress`, `:245`)
- summary ceiling = usable/12, clamped 400–6000 (`:317`)
- nothing is destroyed: the journal replays the full transcript (`replay_transcript`, `:733`)
- the pre-fold state is persisted (`session_compaction` table, `vikram-store/src/lib.rs:4494`)

And then it gives the summary to a **keyword scrape**. `FoldNote::collect` (`compaction.rs:490-560`) buckets old messages by substring markers:

- decisions: `"we will"`, `"i will"`, `"decided"`, `"decision:"`, `"chose"`, `"going with"`, `"the fix is"`, `"plan:"`, `"approach:"` (`:511-515`)
- pending: `"todo"`, `"still need"`, `"next:"`, `"remaining"`, `"not yet"`, `"later"`, `"to do"` (`:516-520`)
- errors: `"error"`, `"failed"`, `"panic"`, `"denied"`, `"timeout"`, `"cannot"`, `"unable"` (`:521-523`)
- each kept line is clipped to 300 chars (`condense`, `:691`)

No model is asked to write the summary. `Message::text(Role::System, summary, Trust::Host)` (`compaction.rs:285`). A decision phrased without one of those markers never survives; the *reasoning* that connected decisions never survives at all. The transcript stays valid and the thread of intent is gone.

### 1.4 Tool layer

- 89 builtin registrations in `with_builtins` (`tools/core.rs:207-305`, [counted] `awk` over that range), plus MCP tools registered dynamically. `ToolSpec { name, description, schema: Value }` (`provider.rs`): a one-line description and a raw JSON schema. `fs.read`'s schema declares `{"path": {"type":"string"}}` with no per-field description (`tools/fs.rs:16-23`).
- Output ceiling: `MAX_TOOL_OUTPUT = 20_000` **bytes**, cut **head-only**, `"… [output truncated]"` appended (`tools/core.rs:12`, `:384-396`).
- After the tool returns, `turn.rs:2361` calls `inner.spool_output(...)` (`runtime.rs:1902`), which digests head+tail to `tool_output_ceiling` (default 6,000 chars, `config.rs:425`) and writes the original to a per-session spool with a handle that `output.read` can fetch.
- **Order matters and is wrong.** The tool has already truncated to 20,000 bytes *before* the spool runs, so the spool stores at most 20,000 bytes and `output.read` can never return what `truncated()` cut. The full-text guarantee holds only for results between 6,000 and 20,000 bytes.
- `fs.read` takes **only** `path` — no `offset`/`limit`, no line numbers — reads the whole file and cuts at 20,000 bytes head-only (`tools/fs.rs:11-23`, `:36-47`). Any file whose first ~500 lines exceed 20 KB cannot be fully read by the model; the rest is reachable only through `search.text` or a shell `sed`.
- `fs.edit` is exact-string replacement, requires uniqueness, and returns `"edited X: N -> M bytes"` (`tools/fs.rs:207-300`). The model never sees the resulting diff, the human never sees it either, and whitespace drift causes a hard failure ("old_string not found") rather than a tolerant match.

### 1.5 Command execution — a defect worth the whole audit

All three shell-ish paths (`shell.run`, `git`, `verify`) funnel through `wait_with_timeout` (`verification.rs:536`). It:

1. spawns with `Stdio::piped()` for both streams (`:547-551`)
2. polls `try_wait()` every 25 ms until exit **or** deadline (`:553-567`)
3. only **after** the child exits does it spawn reader threads to drain stdout/stderr (`:568-600`, comment: "they must not be joined, or a grandchild holding the pipe would block this call exactly like `wait_with_output` did")

Nobody reads the pipes while the child runs. A child that writes more than the pipe buffer blocks on `write(2)`, never exits, and is killed at the deadline. **[repro]**: I copied this exact ordering into `/tmp/opencode/probe/wt.rs` and ran it with `rustc`; a child writing 70,000 bytes to stdout:

```
4096: ok bytes=4096
60000: ok bytes=60000
65000: ok bytes=65000
70000: TIMEOUT
200000: TIMEOUT
```

So on this machine any command producing ≳64 KiB on stdout+stderr — `cargo build` with warnings, `cargo test`, `git log`, `git diff`, a large `cat` — is reported to the model as `command timed out after N ms` with **no output at all**, after burning the whole timeout. `git`'s timeout is 30 s (`tools/git.rs:10`). The user experiences the agent as unable to run its own build.

### 1.6 Policy, trust, approvals

`vikram-policy/src/lib.rs` is a real permission engine, not a stub:

- deny-first; no rule means no permission (`lib.rs:1-9`)
- a `MandatoryFloor` evaluated *before* rules, which no later rule or substituted evaluator can widen (`lib.rs:576-585`)
- `evaluate_at_depth` so a delegated session cannot escape `max_delegation_depth` (`lib.rs:576`; caller `turn.rs:2096`)
- resource patterns are tiny by design: exact, `prefix/**`, `cmd *`, `*` (`lib.rs:23-58`)
- command segments are split on `&&`, `||`, `;`, newline, pipes and redirections, including `$(`/`((`, so `cargo test && rm -rf /` is evaluated as two requests (`lib.rs:307-340`, used at `:287`)

`PermissionMode` has seven postures including `Plan` (read-only refusal) and `AllowList` (`vikram-contracts/src/policy.rs:101-118`); `resolve` enforces them *under* the evaluator, and `Deny` is final in every mode (`policy.rs:131-230`).

Origin/trust is set at the call site: `Origin::Model → Trust::Model`, `Origin::Operator → Trust::User` (`turn.rs:50-63`), and `run_tool_once` overwrites the tool's own `originating_trust` with it (`turn.rs:2061`). The documented weak spot "Trust::Model defaults in permission builders" is **refuted for production code**: the only builder that defaults to `Trust::Model` is `gated()` inside `#[cfg(test)] mod tests` (`vikram-contracts/src/policy.rs:446`, `:472`). Every production tool passes its trust explicitly. Do not spend a wave on this.

Approvals are a proper oneshot: insert into `inner.approvals`, emit `ApprovalRequested`, race the answer against cancellation, remove on either path, journal the decision (`turn.rs:2134-2210`, `:3120`).

But the prompt the human sees carries **no preview of the effect**:

```rust
pub struct ApprovalPrompt {
    pub approval: ApprovalId, pub session: SessionId,
    pub verb: String, pub resource: String, pub reason: String,
    pub amendment: Option<String>,
}
```
(`types.rs:273-280`)

For `fs.write`/`fs.edit` that is a path. For `shell.run` that is the command string (admittedly the important part). For an MCP tool it is `server.tool`. There is no diff, no content preview, no "what will change". The operator approves blind and learns to either trust everything or read every file afterwards.

MCP trust is a separate hole: every MCP tool is gated as `ActionKind::Execute`, `SideEffect::LocalMutation`, resource `server.tool` (`tools/mcp.rs:33-38`). A remote server's `send_email` or `create_invoice` is indistinguishable from a local file mutation, and in `AcceptEdits`/`Smart` postures a `LocalMutation` inside the workspace can auto-approve. MCP tools are also never classified by their own annotations: `grep -rn annotation vikram-mcp/src` finds nothing, so `readOnlyHint`/`destructiveHint` returned by `tools/list` are discarded before the tool is built.

### 1.7 Memory

`hybrid_recall` (`memory.rs:385`) fuses four signals: FTS5 BM25 (`vikram-store/src/lib.rs:4827` creates `session_fts`; the memory table's FTS path is `memory.rs:836 fts_or_like_scores`), token overlap, recency × importance decay, optional local cosine; then MMR rerank at λ=0.7 (`memory.rs:1-24` module docs). Scope expansion is written only by runtime code (`bind_session_scopes`), so naming another session's id cannot widen recall.

This is the best-built subsystem in the crate. The weak side is *writing*: memory exists only if the model called `memory.save`, and the learning pipeline stages proposals that await human approval (`learning.rs:913-960`), so "learning" is inert until someone approves it.

### 1.8 Durability and resume

Journal-first: every message, tool result, policy decision and approval is journaled (`inner.journal(...)`, e.g. `turn.rs:2099`, `:2200`), and the transcript is a projection (`replay_transcript`, `compaction.rs:733`; `project_from_journal`, `:855`).

Crash recovery landed: `list_interrupted_sessions` + `recover_interrupted_session` (`vikram-store/src/lib.rs:1329`) append synthetic failed results for pending tool calls and journal a `recovery` record; surfaced as `sessions recover` (`vikram-cli/src/main.rs:1199-1226`).

**This repairs the transcript; it does not resume the turn.** The model is not re-invoked at the point of interruption with intent intact — the user re-prompts. The documented claim "execution-resume still missing" is therefore true in the strong sense and false in the weak sense. State it precisely in `CONTINUE.md`.

### 1.9 Providers

`anthropic.rs` (1,810) and `openai.rs` (2,099) both stream SSE with tool-call delta assembly, interleaved tool blocks, thinking blocks surfaced as reasoning (not text), and retries: `MAX_ATTEMPTS = 3`, `MAX_RETRY_WAIT = 10s`, `Retry-After` honoured, transport failures retried only before any byte was sent (`openai.rs:117-138`, `:328-341`; Anthropic uses the same policy, `anthropic.rs:111-180`).

**No prompt caching.** `grep -c cache_control anthropic.rs openai.rs` → `0 0`. Every step re-sends the full system prefix plus 89 tool schemas at full price with no cache breakpoints. On a long session that is the difference between a harness you leave open all day and one you close to save money.

### 1.10 Orchestration

`orchestrator.rs` (580 lines) is **untracked but already wired in the working tree**: `git status` shows `??` for the file, while `git diff` shows `lib.rs` gaining `pub mod orchestrator;` + re-exports, `runtime.rs` gaining `RuntimeInner.workers: WorkerRegistry`, and `tools/core.rs` registering `SpawnWorker`. So the module and its tool exist and compile against in-flight policy/session changes — but nothing is committed, which is why §6 treats it as in-flight rather than done. It defines `WorkerBrief { objective, output_format, tool_allowlist, file_scope, boundary, model, effort }` (`orchestrator.rs:56-90`), `WorkerRegistry` with `mark_partial`/`complete`/`degrade`/`attempt_failed` (`:164-250`), and a `spawn_worker` tool (`:252`). In-flight `vikram-policy` adds `CapabilityScope::narrowed(parent, tools, files)` and `session.rs` gains `capability_scope`, `worker_max_turns`, `effort`.

Today's only working delegation is bot-based: `hive.delegate` (`tools/hive_roster.rs:161-246`) → `spawn_agent` (`runtime.rs:4422`), bounded by `max_delegation_depth`.

### 1.11 Everything else, briefly

- **Daemon**: trigger kinds with a persistence-backed quiet-hours window and a dedupe window (`vikram-daemon/src/triggers.rs:33-200`; landed `317c6b5`, `6e6b7e7`).
- **MCP**: stdio client that lists tools/resources/prompts (`vikram-mcp/src/client.rs:71-140`); only **tools** become model-visible. Resources and prompts are collected and never surfaced (`grep` for `all_resources` in `vikram-runtime/src` finds nothing).
- **Skills**: metadata catalogue injected, bodies loaded on demand via `skill.load`/`skill.read` (`turn.rs:2547-2555`) — correct progressive disclosure.
- **Bench**: `personality.rs` is deterministic, offline, and explicitly does not fail the run (`vikram-bench/src/personality.rs:1-14`). There is no task-level pass/fail benchmark of "can the harness finish a job".
- **Sandbox**: real bwrap/seatbelt wrapping (workspace rw, `/` ro, `--unshare-net`, `--clearenv` + allowlist) with honest degradation notices (`vikram-sandbox/src/lib.rs:222-295`), enabled by default with network off (`config.rs:137-145`). Landlock and Windows Job Objects are documented stubs that say so (`lib.rs:237-247`).

---

## 2. Why It Feels Basic

The harness is not thin. 67,974 lines in the runtime alone, ten stage points, lane-parallel tool dispatch, deny-first policy with a mandatory floor, hybrid BM25+MMR memory, a journal that is a projection and destroys nothing. A reader of the crate list would expect a mature agent. The daily user feels otherwise. Seven causes, ranked by how much of the felt experience each explains.

### 2.1 It is blind exactly when a build fails — and it fails *silently*

Two independent defects stack:

1. `wait_with_timeout` reads the pipes only after exit (`verification.rs:547-600`). **[repro]** >~64 KiB of stdout+stderr → the child blocks, the deadline kills it, the tool returns `command timed out` with **zero output**. `cargo build`/`cargo test` on a 246k-line workspace routinely exceeds that, as do `git log` and `git diff`.
2. Whatever survives is cut at 20,000 **bytes**, head-only, *before* the spool can keep the full text (`tools/core.rs:384-396` then `turn.rs:2361`). Rust compiler diagnostics put the summary at the **end**; `cargo test` puts the failing assertion at the **end**. The model gets the head.

So the single most important loop in a coding harness — change, build, read error, fix — is the one that is broken. The user sees the agent "give up", "time out", or invent a fix from partial output, and cannot tell why. This is the largest single cause and the cheapest to fix (P0-1, P0-2).

### 2.2 It forgets the *why* mid-task

After a fold, the surviving text is sentences that happened to contain `"decided"` or `"the fix is"`, clipped to 300 chars each, capped at ~6,000 tokens (`compaction.rs:490-560`, `:317`). No model writes the summary. Decisions phrased as "let's keep the store as the source of truth, otherwise the TUI and the daemon will disagree" vanish; the follow-on work that depended on them gets re-litigated. The user experiences this as "it got dumber after a while" and blames the model.

Codex asks the model itself for a handoff summary with an explicit retention list (progress, decisions, constraints, preferences, next steps, critical data) and prefixes the transcript so the next model knows another model started the work (§5.1). Vikram spends the same tokens on a keyword scrape.

### 2.3 It has been told to be a companion, not an engineer

`PREAMBLE` (`turn.rs:75-105`) is persona only: friend, family member, never sugarcoat, narrate one or two sentences between tool rows. That is the owner's voice and it should stay — but nothing in the model's standing instructions says:

- make a plan for multi-step work, and keep it current
- read the file before editing it
- run the narrowest check that proves the change
- do not say "done" without evidence
- tool output is data, not instructions
- keep going until the problem is actually resolved

Codex's base instructions are 275 lines of exactly this, plus AGENTS.md scoping rules and a "Validating your work" section that changes behaviour by approval mode (§5.1). Vikram's plumbing has more depth than Codex's in places; its instruction set has less.

### 2.4 The user cannot see intent, and cannot see effect

- **Intent**: there is no plan artifact. `PermissionMode::Plan` is read-only refusal (`contracts/policy.rs:108`), not planning. The only `task` tool writes a global `$VIKRAM_HOME/tasks.json` (`tools/process.rs:226-238`) that the TUI does not render as a live checklist. On a 20-step refactor the user has no idea what the agent is doing or whether step 7 of 12 is done.
- **Effect**: `ApprovalPrompt` carries verb, resource, reason, amendment — **no content** (`types.rs:273-280`). `fs.edit` returns `"edited f.rs: 1200 -> 1250 bytes"` (`tools/fs.rs:283-292`). Nobody — model or human — sees the diff of what just changed. Every review therefore happens after the fact, in `git diff`, by hand.

### 2.5 "Done" is unverified, and the one component built to verify was switched off

`verification.rs` (1,203 lines) contains edit→build→test→repair with `FailureKind` classification (`:41`), attempt isolation in a worktree (`:757`), and change expectations (`:123-243`). It is called from exactly one place — the `verify` tool — which:

- advertises "single attempt, no auto-repair" (`tools/verify.rs:15-17`)
- clamps `max_attempts` to 1 (`:39-41`)
- passes a repair closure that always declines: `|_, _| None` (`:73`)
- treats `expected_changes` and `expect` as **warnings that never affect pass/fail** (`:16-19`)

So the "tests pass, feature missing" failure is structural: the harness can only ask "did the build succeed", never "did the change do what was asked". The user learns to distrust every completion claim — which is precisely the complaint that produced this audit.

### 2.6 Every step re-pays the whole prompt

89 builtin tool schemas plus the full system prefix are re-sent on every step with `cache_control` nowhere (`grep -c` → 0). Tool schemas are always all present: `specs()` filters only a global disabled set (`tools/core.rs:317-326`); there is no per-mode or per-session gating, so even `Plan` mode ships write tools in its schema. Long sessions get slow and expensive, the model has no tool to ask "how much room is left", and the meter is a TUI event only (`ContextMetered`, `turn.rs:530`).

### 2.7 The agent cannot run its own project out of the box

Default sandbox: enabled, network off, `/` bound read-only, workspace bound read-write, `--clearenv` (`config.rs:137-145`; `vikram-sandbox/src/lib.rs:262-295`). **[read-inferred, unverified by execution]** `$CARGO_HOME` is therefore read-only and the network is gone, so a `cargo build` that needs the registry or a package-cache lock fails inside the jail; there is no "the sandbox denied it, escalate with approval" path the way Codex has (`SandboxErr::Denied` → retry with approval, `codex-rs/core/src/exec.rs:828-832`). Owner can test this in one command:

```
vikram chat "run cargo build and show me the last 20 lines"
```

If it reports a timeout, a read-only filesystem, or silent failure, 2.7 is confirmed and P0-1/P1-4 are the fixes.

### Symptom → cause map

| What the owner feels | Actual cause | Fix wave |
|---|---|---|
| "It can't even build the project" | pipe deadlock + head-only cut (§2.1) | P0-1, P0-2 |
| "It forgets what we decided" | keyword-scrape compaction (§2.2) | P1-1 |
| "It wanders / doesn't plan" | no plan tool; persona-only prompt (§2.3, §2.4) | P0-3 (contract), P1-2 (plan) |
| "I have to re-read every edit" | no diff in approval or tool result (§2.4) | P0-4 |
| "It says done and it isn't" | verify loop stubbed; expectations advisory (§2.5) | P0-5 |
| "It gets slow/expensive late in a session" | no prompt cache; 89 tools always (§2.6) | P0-6, P1-10 |
| "I can't tell what it cost" | no cost/context surface (§2.6) | P1-3 |
| "Sandbox breaks normal dev" | network/ro-home by default, no escalation (§2.7) | P1-4 |

---

## 3. Gap Analysis

Severity is judged by one question: does this change what the single daily user gets, this week? Not "is it missing from a checklist".

| # | Gap | Evidence | World-class practice | Cost (effort) |
|---|---|---|---|---|
| G1 | Pipe drained only after exit → >64 KiB output becomes "timed out", 0 bytes | `verification.rs:547-600`; **[repro]** 70 KB → TIMEOUT | drain concurrently (`tokio::process` + `read_to_end` in `join!`), or inherit a pty | S |
| G2 | Truncation is byte-head-only and runs before the spool | `tools/core.rs:12,384`; `turn.rs:2361`; `runtime.rs:1902` | token-budgeted middle truncation, head+tail, header stating original size (§5.1) | S |
| G3 | Compaction summary is a keyword scrape, no model call | `compaction.rs:490-560` | model-written handoff summary + "another model started this" prefix (§5.1) | M |
| G4 | Approval carries no preview; edits return byte counts | `types.rs:273-280`; `tools/fs.rs:284` | diff/command preview rendered per call (`ToolCallView`, §5.2) | M |
| G5 | Standing instructions are persona only | `turn.rs:75-105` | explicit work contract + AGENTS.md scope rules (§5.1) | S |
| G6 | No plan artifact the user can watch | `contracts/policy.rs:108`; `tools/process.rs:226` | `update_plan` with rendered checklist (§5.1) | S |
| G7 | Verify loop wired to a declining closure; expectations advisory | `tools/verify.rs:15-19,39-41,73` | expectations that can fail; repair loop actually run | M |
| G8 | No prompt caching on either provider | `grep -c cache_control` = 0/0 | cache breakpoints on system+tools (§5.5) | S |
| G9 | No context/cost visibility for the model or the user | `ContextMetered` is UI-only (`turn.rs:530`); no cost field anywhere | status line + `/cost` + a budget tool | S |
| G10 | 89 schemas always sent; no per-mode gating | `tools/core.rs:207-326` | tool gating by mode/session; terse schemas with parameter docs | M |
| G11 | MCP tools all classified `LocalMutation`/`Execute` | `tools/mcp.rs:33-38` | use server annotations; classify per tool; ask by default | S |
| G12 | MCP resources/prompts collected, never reachable | `vikram-mcp/src/client.rs:124-142`; no runtime user | expose as `mcp.read_resource` + prompt commands (§5.5) | M |
| G13 | Turn resume: repaired, not resumed | `vikram-store/src/lib.rs:1329` | checkpoint at step boundary, replay and continue (§5.1) | L |
| G14 | Orchestrator uncommitted; delegation path untested end to end | `orchestrator.rs` untracked; `lib.rs`/`runtime.rs`/`tools/core.rs` wiring in working tree | spawn/fan-out with typed briefs + artifact results (already written) | S (tests) |
| G15 | `max_steps` name promises a cap the code does not enforce | `config.rs:498`; `turn.rs:901` | either enforce, or rename to `step_notice` and document it | S |
| G16 | AGENTS.md only root + one level | `context.rs:399` | per-directory scope with precedence (§5.1) | S |
| G17 | `fs.edit` exact-match only; whitespace drift = failure | `tools/fs.rs:252-270` | multi-pass tolerant match: exact → trim-end → trim-both (§5.1) | S |
| G17b | `fs.read` cannot page; no offset/limit, no line numbers, head-only cut | `tools/fs.rs:11-23,36-47`; `tools/core.rs:384` | line-ranged read with numbered output (§5.5) | S |
| G18 | No refusal / `stop_reason` handling | `turn.rs:766-800` has size-recovery only | inspect `stop_reason`, retry refusals bounded, handle truncation (§5.2) | S |
| G19 | Silent error discards hide failed state writes | [counted] non-test: 317 `let _ =`, 332 `.ok()`, 343 `unwrap_or_default()`, 295 `.unwrap()` | audit the ones on durable writes only; leave config fallbacks | M (PARTIAL) |
| G20 | Memory written only when the model remembers to; learning gated behind approvals | `memory.rs:385` (read side good); `learning.rs:913-960` | post-turn consolidation pass with dedupe/decay | L |
| G21 | Bench cannot answer "is the harness better at work" | `vikram-bench/src/personality.rs:1-14` | task-level pass/fail evals (§5.2) | L |
| G22 | Landlock / Windows sandbox are honest stubs | `vikram-sandbox/src/lib.rs:237-247` | fine for Linux bwrap user; document | skip |

**Not gaps** — documented weak spots that are already handled, verified, do not re-litigate: Trust::Model defaults (§1.6, refuted), interrupted-session repair (§1.8, landed), deny-first policy with a floor (§1.6), parallel lane dispatch (§1.1), hybrid memory recall (§1.7), progressive skill loading (§1.11), trigger dedupe/quiet hours (§1.11).

---

## 4. Redesign Waves

Rules for this plan: one user, one machine, no multi-tenant, no cloud control plane, no "agent OS". Every item names files, an approach, an acceptance test that can actually be run, and an effort estimate. Items are ordered so that the first block changes what the owner feels **this week**.

### P0 — the week's block (feel-fixing, low blast radius)

#### P0-1 Drain command pipes while the child runs
- **Files**: `crates/vikram-runtime/src/verification.rs` (`wait_with_timeout` :536-611), callers `tools/shell.rs:41`, `tools/git.rs:31`, `tools/verify.rs:216`.
- **Approach**: spawn with `tokio::process::Command`, take `child.stdout`/`child.stderr`, and `tokio::join!` the two `read_to_end` futures with `child.wait()` inside a `tokio::select!` against the deadline and the cancel token. Keep `process_group(0)` + `kill_tree` unchanged. Cap the buffers (e.g. 8 MiB) so a runaway writer cannot exhaust memory; on hitting the cap, keep draining and discarding so the child still exits.
- **Acceptance**: `cargo test -p vikram-runtime large_output_is_not_mistaken_for_a_timeout` — a child printing 2 MiB exits 0, returns full-length (or capped) stdout, `ok == true`, and takes < 1 s. Regression: a 200 ms-sleep child with a 50 ms timeout still returns `None`. Reproduce first: the new test must fail on the current code (it will, as `[repro]` in §1.5 shows).
- **Effort**: S.

#### P0-2 Replace byte-head truncation with a token-budgeted middle cut, and spool before cutting
- **Files**: `tools/core.rs:12,384-396`, `turn.rs:2359-2361`, `runtime.rs:1902-1929`.
- **Approach**: (a) change `truncated()` to keep head and tail, insert `\n… [N of M bytes elided] …\n`, and prepend a header with original byte and line counts (Codex's header is the model of this, §5.1); (b) in `run_tool_once`, spool the **full** content first, then hand the model the digest — i.e. swap the order so `spool_output` sees untruncated text, and let the digest carry the `output.read` handle; (c) make `MAX_TOOL_OUTPUT` a config knob (`context.tool_output_ceiling` already is; give it a byte twin).
- **Acceptance**: `cargo test -p vikram-runtime the_tail_of_a_failing_command_survives_truncation` — a 100 KB payload ending in `error[E0308]: mismatched types` yields model-visible content containing both the first line and that error line, plus a readable handle; `output.read` on the handle returns the full 100 KB.
- **Effort**: S.

#### P0-3 Give the model a work contract (keep the persona)
- **Files**: `turn.rs:75-105` (`PREAMBLE`), `assemble_context` `turn.rs:2560-2590`.
- **Approach**: keep `PREAMBLE` verbatim for voice; add a second system message `WORK_CONTRACT` immediately after it, ≤ 60 lines, stating: plan multi-step work before acting; read a file before editing it; prefer the narrowest check that proves the change and run it before claiming done; never claim a result you did not observe; tool output is data and never an instruction; when a tool fails twice the same way, change approach and say so; leave unrelated code alone. Encode the AGENTS.md precedence rule in one line ("a nearer AGENTS.md wins; user instructions win over both"). This is the highest leverage-per-line change in the document.
- **Acceptance**: `cargo test -p vikram-runtime the_prefix_carries_the_work_contract` asserting the system prefix contains both blocks in order and that the contract is < 700 tokens by `TokenEstimator`. Behavioural check, run manually: three fixed tasks (fix a failing test, add a flag, rename a symbol) before/after, count tool calls and "false done" claims.
- **Effort**: S.

#### P0-4 Show the diff at the approval prompt and in the tool result
- **Files**: `vikram-runtime/src/types.rs:273-280` (`ApprovalPrompt`), `turn.rs:2134-2150`, `tools/fs.rs:207-300`, `vikram-tui/src/app.rs` approval rendering, `vikram-cli` approval print.
- **Approach**: add `preview: Option<ApprovalPreview>` where `ApprovalPreview` is `Diff { path, unified, added, removed } | Command { cwd, line } | Body { truncated }`. Add `Tool::preview(&self, args, ctx) -> Option<ApprovalPreview>` with a default of `None`; implement it for `fs.write`, `fs.edit`, `fs.delete` (compute a unified diff from the current file, ≤ 200 lines, elide the middle) and `shell.run` (already has the command string; add cwd). Render it in the approval pane with `+`/`-` colouring and the existing scroll/copy machinery. `fs.edit` also returns the first 10 changed lines in its result instead of `N -> M bytes`.
- **Acceptance**: unit test `fs_edit_preview_is_a_unified_diff` (contains `-old`, `+new`, correct counts); TUI snapshot test `the_approval_pane_shows_the_pending_diff`; CLI test that a non-interactive approval print includes the diff.
- **Effort**: M.

#### P0-5 Make verification able to say "the change is wrong"
- **Files**: `tools/verify.rs:15-19,39-41,66-73`, `verification.rs:669-760`.
- **Approach**: (a) add `strict: bool` to the `verify` schema; when true, a missing `expect` pattern or an `expected_changes` path that did not change makes `ok = false` with `kind = "expectation_missing"`; (b) stop clamping `max_attempts` to 1 — either wire the repair closure to the caller-supplied `repair` hook or delete `verify_attempts`/`isolate_attempts` so the code stops advertising what it cannot do. Prefer wiring: run attempt 1, classify, and if `retryable`, hand the classified failure back into the turn as a `Notice` + tool result rather than looping inside the tool.
- **Acceptance**: `cargo test -p vikram-runtime strict_verify_fails_when_the_expected_symbol_is_missing` — write a temp workspace whose test passes, expect a symbol that is absent, assert `ok == false`, `kind == "expectation_missing"`, and that the model-visible content names the pattern. Second test: non-strict mode keeps the old behaviour.
- **Effort**: M.

#### P0-6 Cache the prompt on the wire
- **Files**: `anthropic.rs:600-640` (request body), `turn.rs:2540` (prefix assembly ordering).
- **Approach**: mark the last system block and the tool list with `cache_control: {"type": "ephemeral"}` (2 breakpoints; Anthropic allows 4). Order matters — put the stable parts (contract, project instructions, tool schemas) before the volatile ones (code-index context, memory) and place the breakpoint after the stable block. For OpenAI this is a no-op: automatic caching kicks in above ~1k tokens; document that.
- **Acceptance**: `cargo test -p vikram-runtime anthropic_requests_mark_the_stable_prefix_as_cacheable` asserting the serialized body has exactly two `cache_control` markers at the expected positions; manual: run a 20-step session and compare `usage.cache_read_input_tokens` before/after (the provider reports it; surface it in P1-1).
- **Effort**: S.

#### P0-7 Classify MCP tools instead of trusting them uniformly
- **Files**: `tools/mcp.rs:33-38`, `vikram-mcp/src/client.rs` (store `annotations` from `tools/list`).
- **Approach**: honour MCP `readOnlyHint`/`destructiveHint` when present; map read-only to `ActionKind::Read`/`SideEffect::None`, destructive to `SideEffect::Destructive`, unknown to `SideEffect::ExternalChange` (never `LocalMutation`, which is what `Smart`/`AcceptEdits` auto-approve). Keep the resource as `server.tool` so the prompt is legible.
- **Acceptance**: `cargo test -p vikram-runtime an_unknown_mcp_tool_requires_approval_in_smart_mode` — a fake registry with one unannotated and one read-only tool; assert the first yields `RequireApproval` under `Smart` and the second does not.
- **Effort**: S.

**Why not compaction in P0?** It is the second-largest felt cause but needs an async summarisation call inside a sync path (`prepare_context` is `&self`, `runtime.rs:1575`). Doing it properly is P1-1 and it must not be rushed into the same week as the pipe fix.

### P1 — next block (structure that pays back daily)

#### P1-1 Model-written compaction with a deterministic fallback
- **Files**: `runtime.rs:1575-1640` (`prepare_context`), `compaction.rs:254-330`, `turn.rs:449-500` (where the fold is triggered today), `store` `session_compaction` (`vikram-store/src/lib.rs:4494`).
- **Approach**: move the decision out of `prepare_context` into the async turn loop: before building the request, if `should_compact`, issue a **non-streaming** provider call with the old messages and a fixed summarisation prompt (progress, decisions, constraints/preferences, next steps, critical data/paths, open failures), cap output at the `summary_ceiling` budget, and on any error fall back to today's `FoldNote`. Store the summary as a `session_compaction` row and inject it as the first message with Codex's "another model started this, here is its summary" prefix (`prompts/templates/compact/summary_prefix.md`). Keep `pairing_is_consistent` and `replay_transcript` untouched — they are correct.
- **Acceptance**: `cargo test -p vikram-runtime compaction_asks_the_model_and_falls_back_on_error` — a scripted provider asserts (a) the summarisation request contains the retention list, (b) the folded transcript starts with the summary prefix, (c) a provider error still produces a valid fold via `FoldNote`, (d) `pairing_is_consistent` holds in both cases.
- **Effort**: M.

#### P1-2 A live plan the user can watch
- **Files**: new `crates/vikram-runtime/src/tools/plan.rs`, `session.rs` (plan state), `types.rs` (`RuntimeEvent::PlanUpdated`), `vikram-tui/src/app.rs` + `ui.rs` (a plan panel), `vkram-cli` (print on update).
- **Approach**: `update_plan` tool taking an ordered `steps: [{title, status: pending|active|done}]`, plus one `explanation` on revision. Session-scoped (not the global `tasks.json`). The tool returns "Plan updated" and emits an event; the TUI renders it beside the transcript and marks the active step; the model is told (in the work contract) to create a plan for ≥3-step work and to mark steps as it goes. This is the closest mechanical analogue of Codex's `update_plan` (`codex-rs/core/src/tools/handlers/plan.rs`, instructions at `prompts/src/update_plan_instructions.rs`).
- **Acceptance**: `cargo test -p vikram-runtime update_plan_replaces_the_session_plan_and_emits_an_event`; TUI snapshot with 3 steps and the middle one active; a plan survives a session reload (journal replay).
- **Effort**: M (S if the TUI panel is plain text).

#### P1-3 Context and cost become visible
- **Files**: `types.rs` (`ContextMeter` already exists), `vikram-tui/src/app.rs` status line, `vikram-cli/src/chat.rs` (a `/cost` and `/context` command), `anthropic.rs`/`openai.rs` (carry `cache_read_input_tokens`), new tool `context.status` or extend `output.read`.
- **Approach**: surface `used/usable`, the fold count, and cumulative cost per session; add one model-facing tool that answers "tokens remaining" so the agent can plan instead of being surprised. Codex's model has no such tool either, but its model *is told* the budget in the environment context — pick one and be consistent.
- **Acceptance**: `cargo test -p vikram-cli cost_command_reports_reported_usage_not_an_estimate` — with a fake provider reporting 1,234 input / 567 output, `/cost` prints those numbers; a `/context` render test asserts the meter parts sum to `used`.
- **Effort**: S.

#### P1-4 Sandbox that lets a build succeed, and escalation when it does not
- **Files**: `vikram-sandbox/src/lib.rs:262-295`, `config.rs:127-145`, `verification.rs`/`tools/shell.rs`.
- **Approach**: bind `$CARGO_HOME`, `$RUSTUP_HOME`, and `~/.cache` read-write (or a dedicated `$VIKRAM_HOME/sandbox/{cargo,cache}`), keep network off by default, and add detection: if a command fails with a sandbox-shaped error (EPERM/ENETUNREACH/"Read-only file system"), raise a `RequireApproval { amendment: "run outside the sandbox" }` instead of returning a dead end. Codex models this as `SandboxErr::Denied` → escalate (`codex-rs/core/src/exec.rs:828-832`).
- **Acceptance**: `cargo test -p vikram-sandbox cargo_home_is_writable_inside_the_jail` (skip cleanly when bwrap is absent) and `cargo test -p vikram-runtime a_sandbox_denial_is_offered_for_escalation`.
- **Effort**: M.

#### P1-5 Land the orchestrator
- **Files**: `orchestrator.rs` (untracked), `lib.rs`, `tools/core.rs` (register `spawn_worker`), `vikram-policy` `CapabilityScope` (in flight).
- **Approach**: register the module and tool, wire `capability_scope` into `run_tool_once` next to the depth check (`turn.rs:2096`), and add one `team` tool that spawns N workers from N briefs and waits. Keep results as store artifacts (already designed) so the parent context does not carry child transcripts.
- **Acceptance**: `cargo test -p vikram-runtime a_worker_cannot_escape_its_brief` — a worker briefed with `tool_allowlist: ["fs.read"]` calling `shell.run` is denied by policy, and the denial names the envelope; `cargo test -p vikram-runtime a_failed_worker_is_marked_partial_not_fatal`.
- **Effort**: S to register, M to make the acceptance tests pass.

#### P1-6 Stop ignoring the model's stop reason
- **Files**: `turn.rs:700-800`, provider event mapping.
- **Approach**: carry `stop_reason` through `ProviderEvent::Finished`; on refusal/content-filter retry at most twice with a nudge, then end the turn with an honest `TurnState::Failed`; on max-tokens truncation, commit the partial answer and continue the turn once rather than silently cutting. inspect_ai does exactly this (`agent/_react.py:283-305`: `model_length` → overflow recovery, `content_filter` → 3 attempts).
- **Acceptance**: `cargo test -p vikram-runtime a_refusal_is_retried_twice_then_reported` and `a_truncated_answer_is_continued_once`.
- **Effort**: S.

#### P1-7 AGENTS.md scope ladder and tolerant edits
- **Files**: `context.rs:386-410`, `tools/fs.rs:207-300`.
- **Approach**: load AGENTS.md from every directory from the workspace root down to the file's directory, nearest-wins, and inject only the ones that apply to the touched paths (cache per turn). For edits, adopt Codex's `seek_sequence` strategy: exact → ignore trailing whitespace → ignore both, and prefer an EOF-anchored match when the pattern ends the file (`codex-rs/apply-patch/src/seek_sequence.rs:1-40`); return the diff (P0-4) either way.
- **Acceptance**: `cargo test -p vikram-runtime nested_agents_md_wins_for_files_beneath_it`; `cargo test -p vikram-runtime edit_tolerates_trailing_whitespace_drift` plus `edit_still_refuses_ambiguous_matches`.
- **Effort**: S.

#### P1-8 A read that can page
- **Files**: `tools/fs.rs:11-47`.
- **Approach**: add optional `offset` (1-based line) and `limit` (lines, default 2,000) to `fs.read`; return numbered lines (`   1\tuse std…`) and a trailing `[showing lines 1-2000 of 12,431; call again with offset=2001]`. Apply the 20,000-byte ceiling only *after* paging, so a call with a limit always returns complete lines.
- **Acceptance**: `cargo test -p vikram-runtime read_pages_a_large_file_without_losing_the_tail` — a synthetic 12,000-line file: `offset=11000, limit=500` returns lines 11000-11500, numbered, plus the "call again" hint; a default read of the same file names the next offset instead of a bare elision marker.
- **Effort**: S.

#### P1-9 True turn resume
- **Files**: `turn.rs` (step checkpoint), `vikram-store/src/lib.rs:1329`, `vikram-cli/src/main.rs:1199-1226`.
- **Approach**: journal a `TurnCheckpoint { turn, step, request_text, steps_used, last_tool_batch }` after each committed tool result; on `sessions recover`, if the last checkpoint belongs to an unfinished turn, offer "continue this turn" — re-enter the loop at the next step with the transcript as persisted, `steps_used` restored, and the model told one line: "resuming after an interruption at step N". This is what makes a crashed laptop non-catastrophic; it is also the only item here that needs the journal to carry turn identity (it almost does: `TurnId` is generated per turn).
- **Acceptance**: `cargo test -p vikram-store checkpoint_survives_a_crash_and_resumes_at_the_next_step` using an injected journal; integration: kill the process between two tool calls, run `vikram sessions recover`, assert the next provider request contains exactly one extra system line and the full prior tool results.
- **Effort**: L.

#### P1-10 Gate the tool surface
- **Files**: `tools/core.rs:317-326` (`specs`), `runtime.rs:1246` (`tool_schema_tokens`), `turn.rs:2540` (where the schema set is chosen), `config.rs` (per-posture overrides).
- **Approach**: three cheap cuts. (a) **By posture**: `Plan` excludes every write/execute tool from the schema; `Unattended` excludes the interactive ones (`ask_user`, `message.*`) that cannot succeed. (b) **By relevance**: ship a core set always (fs, search, shell, git, verify, memory, plan) and put browser/lsp/codeindex/hive/message/plugin tools behind one `tool.find`/`tool.enable` meta-tool that adds them to the schema for the rest of the session, with the catalogue's one-line descriptions still in the system prompt so the model knows what exists. (c) **Terse specs**: every schema property gets a one-line `description`; every tool description states when *not* to use it. Target: cut the always-on schema from 89 tools to ~25.
- **Acceptance**: `cargo test -p vikram-runtime plan_mode_schema_has_no_mutating_tools`; a `tool_schema_tokens` assertion that the always-on set is under a stated budget (measure the current number first and record it in the test as the baseline); `cargo test -p vikram-runtime tool_find_enables_a_tool_for_the_session` proving a disabled tool is callable after `tool.find` and refused before.
- **Effort**: M.

### P2 — later (real, not urgent)

| Item | Files | Acceptance | Effort |
|---|---|---|---|
| Memory consolidation pass: post-turn dedupe, decay, "what this project taught us" artifact | `memory.rs`, `consolidation.rs` | duplicate note merged; ancient low-importance entry demoted not deleted | L |
| Task-level eval bench: 10 fixed repo tasks with pass/fail, run before/after harness changes | `vikram-bench/src` new module | `vikram bench tasks` prints pass/fail per task, non-zero exit on regression | L |
| Second-model review tier (Codex Guardian analogue), opt-in, reviews high-risk commands only | new `guardian.rs`, `turn.rs` gate | a dangerous command is refused with a reason from the reviewer; off by default | L |
| MCP resources/prompts reachable from the CLI | `vikram-mcp/src/client.rs:124-142`, new tool | `@server:resource` resolves; prompt appears as a slash command | M |
| Evolution durability for `self_*` engines | `vikram-evolution/src/lib.rs:63-68` | state survives restart; dashboard shows it | L |
| TUI input/mailbox model borrowed from gitui | `vikram-tui/src/app.rs` | only if input latency is measurable; otherwise skip | M |

**Explicit non-goals** (YAGNI, one user): multi-tenant auth, remote sandbox fleets, cloud task queues, a plugin marketplace, a web UI, team analytics. `vikram-gateway` already carries 80,900 lines of that shape; none of it makes the daily loop better.

---

## 5. Comparative Harness Audit

How to read this section. "Better" means *better for one person running a coding agent on their own machine*, not better in the abstract. A cloud task queue is impressive and useless here. Each harness gets: what it is, the mechanics that actually exist in its tree (some have fewer than six — a UI client has no turn loop), then adopt/skip with a target in this repo.

Evidence tier is stated per harness. Three of the four vendored trees are real source; one (`gitui`) is not a harness at all and I say so rather than manufacture a comparison.

### 5.1 Codex — `codex/codex-rs` (source-read)

**What it is.** The internal Codex: 5,719 Rust files, crates for a cloud backend, an agent graph, a "guardian" LLM policy checker, code mode, multi-agent message boards. Much larger than the public CLI. I read the core loop, prompts, compaction, truncation, approvals/sandbox, subagents.

**Turn loop.** `run_turn` at `core/src/session/turn.rs:177`, driven by a step loop; there is no `max_turns` — the agent keeps going until the task resolves or the user stops it, which the prompt states explicitly ("keep going until the query is completely resolved… Only terminate your turn when you are sure that the problem is solved", `protocol/src/prompts/base_instructions/default.md:125`). Cancellation is a `CancellationToken` threaded through the turn (`turn.rs:183`). *Better than Vikram*: the behavioural contract. *Worse*: surface area; a single turn touches ~20 modules (guardian, code mode, plugin inventory, world state) that a single-user CLI does not need.

**Context assembly + compaction.** Auto-compaction is driven by a per-model token limit with a percent-of-window fallback (`core/src/session/context_window.rs:53-111`), and a post-turn threshold variant. The summary is **written by the model** with a retention list — progress and decisions, constraints and user preferences, what remains, critical data — plus a fixed prefix telling the next model that another model started the work (`prompts/templates/compact/prompt.md`; `summary_prefix.md`). Compaction writes a checkpoint with a window number and is resumable (`core/src/compact.rs:69-90`, `session/compact_resume_*` tests). *Better than Vikram*: the summary actually carries intent, and the next model is told to build on it. *Worse/irrelevant*: remote compaction, image budgeting, guardian checkpoints — cloud concerns.

**Tool registry + result handling.** Truncation is the part worth stealing. `utils/output-truncation/src/lib.rs:23-52`: token-budgeted, **middle** truncation (`truncate_middle_chars`), and the truncated text is prefixed with `"Warning: truncated output (original token count: N)\nTotal output lines: M"`. There is a +20% serialization allowance for headers (`:19-22`), and MCP results are truncated while preserving error status (`:74+`). Compare Vikram: head-only 20,000 bytes and `"… [output truncated]"` (`tools/core.rs:384-396`). *Adopt → P0-2.*

**Permission / sandbox / trust.** Two orthogonal axes, both explicit enums: `AskForApproval { UnlessTrusted, OnRequest, Granular, Never }` (`protocol/src/protocol.rs:1146-1169`) and `SandboxPolicy { DangerFullAccess, ReadOnly { network_access }, ExternalSandbox, WorkspaceWrite … }` (`:1232-1260`). The sandbox is real OS confinement (bwrap / seatbelt / landlock crates under `sandboxing/`, `linux-sandbox/`). Crucially: a command that fails *because of the sandbox* is classified (`is_likely_sandbox_denied`, `core/src/exec.rs:828`) and returned as `SandboxErr::Denied` so it can be escalated for approval instead of being reported as a normal failure. *Better than Vikram*: the escalation path, and OS confinement is the default rather than the exception. *Adopt → P1-4.* Vikram's sandbox is already comparable mechanically (`vikram-sandbox/src/lib.rs:262-295`); what is missing is the escalation.

**Sub-agent orchestration.** `spawn_agent` with per-child model and reasoning-effort overrides (`core/src/agent/child_config.rs:64-260`), role selection, separate developer instructions for children, an inter-agent message board, and `multi_agent_instructions.rs`. *Better than Vikram*: per-child model/effort routing (cheap workers, strong orchestrator). *Irrelevant*: the message-board org chart for one user. *Adopt → P1-5* (the `WorkerBrief { model, effort }` fields already exist in the untracked `orchestrator.rs`; wiring them is the work).

**Memory.** AGENTS.md discovery per directory with precedence rules (`base_instructions/default.md:17-27`), plus retained context and compaction checkpoints. No key/value memory store comparable to Vikram's `hybrid_recall`. *Vikram is ahead here* and should not regress it.

**Verdict: adopt 7, skip the rest.**

| Adopt | Target |
|---|---|
| Middle truncation + size header, token-budgeted | P0-2 |
| Work contract in base instructions (AGENTS.md scope, planning, validating) | P0-3 |
| `update_plan` rendered checklist | P1-2 |
| Sandbox denial → escalate with approval | P1-4 |
| Tolerant patch matching (`apply-patch/src/seek_sequence.rs:1-40`) + unified diff from patch | P1-7, P0-4 |
| Model-written compaction with "another model started this" prefix | P1-1 |
| Per-child model/effort in spawn | P1-5 |

Skip: guardian LLM policy gate (a second model call on every command — cost with no measured benefit for one user), code mode, agent graph, cloud tasks, message board, 5,719-file scale.

### 5.2 Inspect AI — `inspect_ai/` (source-read)

**What it is.** UK AISI's eval framework with an agent layer: 1,763 Python files, `src/inspect_ai/{agent,approval,review,tool,scorer,util}`. It is the only vendored tree that treats *verifying an answer* as a first-class abstraction.

**Agent loop.** `agent/_react.py:261-340`: a `while True` that (a) drains operator messages before each turn, (b) checkpoints at the turn boundary, (c) calls the model, (d) **branches on `stop_reason`**: `model_length` → overflow recovery via compaction or truncation, once (`:283-295`); `content_filter` → up to 3 attempts (`:296-305`), (e) executes tools, (f) detects the `submit` tool and routes the answer to the scorer. *Better than Vikram*: explicit stop-reason handling. Vikram recovers from context-limit errors by dropping messages (`turn.rs:773`) but has no refusal or truncation path. *Adopt → P1-6.*

**Approval.** `approval/_policy.py:23-46`: policies map **approvers** to **tools with argument globs** (`name(arg='value')` legacy form supported, argument values globbed). The approver receives the model message, the call, a **`ToolCallView`** (a custom rendering of the call for the human), and the history (`approval/_approver.py:9-33`). Decisions are `approve/reject/escalate` and are recorded (`_call.py`). *Better than Vikram*: the human decides with a rendered view of the effect, and policy can be argument-specific (`shell.run(command="git *")`). *Adopt → P0-4*; argument-glob policies are already possible in Vikram's `ResourcePattern::Prefix` (`vikram-policy/src/lib.rs:23-40`) so only the view is missing.

**Review.** `review/_review.py:1-20`: a third posture beyond allow/deny — reviewers inspect tool **results** and return `continue | terminate | escalate`. This is how you catch "the tool succeeded but the outcome is wrong" without a full second agent. *Adopt, narrowly, in P2* (guardian slot).

**Limits / checkpointing.** `util/_limit.py`: `TokenLimit` / message limits scoped with `LimitScope`, and the agent loop checkpoints so a limit exit is resumable (`cp.tick()`, `agent/_react.py:275`). *Adopt → P1-9* (turn checkpointing is the same idea).

**Handoff.** `agent/_handoff.py:18-50`: an agent becomes a tool; `input_filter`/`output_filter` shape what the child sees and returns; the default `content_only()` output filter **converts tool calls to text and strips system messages and reasoning** so the result is safe to read by another model. *Better than Vikram's* typed briefs in one respect: the return path is sanitised by construction. *Adopt → P1-5* (artifact-only results already achieve this; the filter idea matters if workers ever return prose).

**Scoring (the semantic-verification machine).** `scorer/_model.py:35+`: `model_graded_fact` / `model_graded_qa` with a grader role, a grader **panel** and majority reducer, and the rule that an unparseable grade withholds a vote rather than shrinking the denominator. This is the only real answer in any vendored tree to "tests pass, feature missing". *Verdict*: steal the **shape**, not the framework — one opt-in call at turn end that asks a grader "here is the request, here is the diff, here is the test result: did the change do what was asked? answer PASS/FAIL plus one sentence", with failures feeding `RepeatGuard` instead of ending the turn. Cost per turn is one cheap model call; make it opt-in and default off for chat, on for `verify --strict`. Target: P2 (and it composes with P0-5, which makes the deterministic half honest first).

**Targets, mapped:** P0-4 (`types.rs` + `tools/fs.rs` + TUI approval pane), P1-6 (`turn.rs` stop-reason handling + provider event), P1-9 (`turn.rs` checkpoint + `vikram-store`), P1-5 (`orchestrator.rs` result filter), P2 (grader call behind `verify --strict`).

**Verdict: adopt 4 narrow ideas, skip the framework.** Inspect's dataset/eval/scorer machinery, sample limits, and viewer are built for scoring thousands of model outputs, not for one person's afternoon. Its `ToolCallView` and `stop_reason` handling are the transferable parts.

### 5.3 OpenHands "agent-canvas" — `openhands2/` (source-read)

**What it is.** Not a runtime. `package.json` declares `"name": "@openhands/agent-canvas"`, `"description": "Agent Canvas UI for OpenHands - run AI coding agents with a visual interface"`, and the repo is React Router + HeroUI + Monaco + Electron. The agent itself is a **separate agent-server process** that is not vendored; this tree is the client for it (`src/api/agent-server-adapter.ts`, `agent-server-compatibility.ts`, ACP types under `src/types/agent-server/core/events/`).

So there is no turn loop, compaction, or policy to compare — and I will not pretend otherwise. What is worth reading:

- **Typed tool-call presentation.** `event-content-helpers/get-acp-tool-call-content.ts:1-40` switches on `tool_kind` (`execute | edit | read | fetch | other`) to pick the verb and the renderer, and strips redundant verbs emitted by servers ("Reading Read /path"). This is a presentation contract: the runtime tells the UI *what kind of thing happened*, and the UI decides how to draw it. Vikram's `RuntimeEvent::ToolStarted { summary, resource, arguments }` (`turn.rs:2160-2168`) carries only a string summary; a `tool_kind` field would let the TUI draw a diff card, a terminal card, or a read card from one event.
- **Confirmation-mode indicator.** `components/features/chat/confirmation-mode-enabled.tsx` renders a lock whenever the session requires confirmation. Vikram has seven postures and shows the mode only inside the status/approval path; a persistent indicator is a two-hour change with real trust value.
- Client-side event ordering/rendering of streamed events (conversation-events tree) is competent React work, not transferable to a Rust TUI.

**Targets, mapped:** `RuntimeEvent::ToolStarted` gains `tool_kind: ToolKind { Read, Edit, Execute, Fetch, Other }` (`types.rs`, emitted at `turn.rs:2160`) and `vikram-tui/src/app.rs` picks a card renderer from it; the posture indicator is a status-line field fed by `StateSnapshot.mode` (`types.rs:302`). Both are S.

**Verdict: adopt 2 presentation ideas, skip the stack.** Electron + Monaco + i18n + a separate server process is the opposite of a single-binary CLI. Do not port architecture from a UI client.

### 5.4 gitui — `gitui/` (source-read)

**What it is.** A Rust git TUI, 162 files. **It is not an LLM harness and is not a benchmark for one.** The only relevant mechanics:

- **Two-rate input polling with a pause state**: `src/input.rs:20-32` (`FAST_POLL_DURATION = 100ms`, `SLOW_POLL_DURATION = 10s`) plus `InputState::{Paused, Polling}` behind a `NotifiableMutex`, so a background operation can stop the poller from stealing keys.
- **One select over six channels**: `src/main.rs:285-320` selects across input, git notifications, app notifications, ticker, notify, spinner. Nothing in the UI blocks on a slow operation because slow work reports through its own channel.
- **Dirty-flag redraw**: `src/queue.rs:21-32` (`NeedsUpdate` bitflags: ALL/DIFF/COMMANDS/branches) means a git event redraws only the affected panes.

Vikram's TUI is event-driven from `RuntimeEvent`s with a spinner tick (`app.rs:965 tick`, `:1986-2018` frames), so the blocking failure mode gitui solves is not currently present. **Target, if ever needed:** `vikram-tui/src/app.rs` — replace unconditional transcript rebuild with a `NeedsUpdate` bitset keyed to the events that actually dirty a pane.

**Verdict: skip, except for the dirty-flag idea if profiling ever shows full-transcript redraws.** Borrowing a git TUI's architecture for an agent harness would be cargo-culting.

### 5.5 Claude Code (knowledge — no source in this repo)

**Mechanics as commonly documented/observed [knowledge]:** a visible todo list the model maintains; `Edit` as a diff-producing tool with an approval that shows the diff; `/cost`, `/context`, `/compact`, `/resume`, `/permissions`, `/mcp` slash surface; subagents (`Task`) with isolated context that return a report; hooks (`PreToolUse`, `PostToolUse`, `Notification`) for policy and side effects; `CLAUDE.md` hierarchy with imports; MCP servers with `@`-mentionable resources; permission modes including `plan` and `acceptEdits`.

**Better than Vikram**: the user can see intent (todo), see effect (diff), see cost, and interrupt the compact decision. **Worse/irrelevant**: closed source, subscription-coupled, undocumented internals, and its context management is opaque — Vikram's journal-projection is objectively better for auditability. **Unverified**: every specific behaviour claim; treat as a hypothesis to test against your own experience.

**Adopt analogues** (all already planned above, which is the point — the gap is not exotic): plan artifact (P1-2), diff at approval (P0-4), cost/context surface (P1-3), user-triggered + model-written compaction (P1-1), subagent reports (P1-5), hooks (Vikram already has a 10-point hook system — verify it is used, not just present), AGENTS.md ladder (P1-7). **Skip**: nothing structural; do not copy its permission UX wholesale (seven postures with a mandatory floor is already finer).

### 5.6 OpenCode (knowledge — no source in this repo)

**Mechanics [knowledge]:** a client/server architecture (a local server plus TUI clients, including a desktop and a web client), provider-agnostic model matrix, sessions with share links, agents declared in config/markdown with their own tool permissions, LSP integration, plugins, and a per-tool permission prompt model.

**Better than Vikram**: per-agent tool allowlists as a first-class config concept; attaching a second client to a running session; a broader provider story. **Worse**: younger codebase, thinner safety story (less of a mandatory-floor equivalent), and the share-link model assumes publishing sessions. **Adopt**: the per-agent allowlist idea is already arriving via `WorkerBrief.tool_allowlist` + `CapabilityScope::narrowed` (in flight, §6) — finish that rather than import the config format. **Skip**: the client/server split; Vikram already has gateway + daemon + ACP, and that is not what makes the daily loop feel basic.

### 5.7 Amp (knowledge — no source in this repo)

**Mechanics [knowledge]:** a deliberately small tool surface, agentic search over the repo rather than embeddings, a second "oracle" model for hard questions, thread sharing, generated commit messages, strong emphasis on speed.

**Better than Vikram**: tool-surface discipline. Vikram ships 89 builtin schemas on every request regardless of task; Amp's bet is that a small, sharp set plus good retrieval beats a wide shallow set. That bet matches the evidence in §2.6/G10. **Worse/irrelevant**: thread sharing (single user), and the closed model tie-in. **Adopt**: gate tools by posture and task (P1-10), and consider an oracle/second-opinion call for the guardian slot (P2) instead of a full review agent. **Skip**: nothing else.

### 5.8 Manus-class cloud agents (knowledge — no source in this repo)

**Mechanics [knowledge]:** a remote VM per task, browser + shell inside it, a persisted plan file (`todo.md`) that the agent rewrites as it works, a replayable "computer" view, long-horizon loops with the plan as the memory spine.

**Better than Vikram**: the **plan as a durable artifact in the workspace**. It survives compaction because it is a file, it is visible to the user without a TUI panel, and the agent can re-read it after a fold. That is a stronger version of P1-2 than a pure event. **Worse/irrelevant**: remote VM, browser-first, multi-tenant isolation, per-task cost. **Adopt**: write the plan to `.vikram/plan.md` in the workspace *and* emit the event (P1-2, amended — this also gives P1-1's summariser something stable to anchor on). **Skip**: the cloud.

### Verdict table

| Harness | Tier | What it is | Adopt | Skip | Single best idea |
|---|---|---|---|---|---|
| Codex | source-read | full internal agent + cloud | 7 | guardian, code mode, org chart | model-written handoff compaction + middle truncation |
| Inspect AI | source-read | eval framework with agents | 4 | datasets, scorers-as-framework, viewer | `ToolCallView` at approval; `stop_reason` branches |
| OpenHands canvas | source-read | UI client, no runtime | 2 | Electron/Monaco/server split | `tool_kind` on tool events |
| gitui | source-read | git TUI, not a harness | 1 (conditional) | everything else | six-channel select + dirty flags |
| Claude Code | knowledge | closed CLI agent | 6 analogues | nothing structural | visible todo + diff approval |
| OpenCode | knowledge | client/server agent | 1 (in flight) | client/server split | per-agent tool allowlists |
| Amp | knowledge | fast closed agent | 2 | thread sharing | small sharp tool surface |
| Manus-class | knowledge | cloud VM agent | 1 | the cloud | plan as a workspace file |

The pattern across all eight: **nobody beats Vikram on safety machinery, journal design, or memory retrieval. Everybody beats it on epistemics — what the model is told, what the human is shown, and what survives a fold.** That is where the waves are aimed.

---

## 6. Already Handled In Flight — do not double-pay

Other agents own this work right now. `git status` at the time of writing shows it uncommitted; treat it as claimed ground.

### Worker A — orchestrator, typed briefs, trust narrowing
- `crates/vikram-runtime/src/orchestrator.rs` (580 lines, untracked): `WorkerBrief { objective, output_format, tool_allowlist, file_scope, boundary, model, effort }` (`:56-90`), `WorkerRegistry` with `mark_partial`/`complete`/`degrade`/`attempt_failed` (`:164-250`), `SpawnWorker` tool (`:252`), and three stated invariants: child scope = intersection of parent envelope and brief; results cross as store artifacts, never transcripts; a failed worker is partial-and-resumable, not fatal (`:1-17`).
- `crates/vikram-policy/src/lib.rs` (+168 lines, uncommitted): `CapabilityScope { unrestricted, narrowed(parent, tools, files), allows_verb, contains_path }`.
- `crates/vikram-runtime/src/session.rs` (+54): `capability_scope()`, `set_capability_scope()`, `worker_max_turns()`, `effort()`.
- `lib.rs` / `runtime.rs` / `tools/core.rs` (+4/+4/+1): module export, `WorkerRegistry` on `RuntimeInner`, `SpawnWorker` registered.

**Consequence for this plan**: P1-5 ("land the orchestrator") is mostly *verification and tests*, not construction. Do not rewrite it. The open questions I could not answer without running tests: is the scope actually consulted in `run_tool_once` (the diff touches policy/session, not `turn.rs`), and do the acceptance tests for brief-escape exist? **[unverified]**

### Worker B — built-in voice
- Landed: `c88a152 feat: built-in mic capture via cpal`.
- In flight: `vikram-voice/src/asr.rs` (untracked), `session.rs`/`lib.rs` changes, `Cargo.toml`.
- Constraint recorded elsewhere: Voxtype stays optional; built-in capture is the default path. Nothing in the harness redesign depends on voice, so no overlap.

### Worker D — daemon triggers
- Landed: `317c6b5 feat: file-change daemon trigger`, `6e6b7e7 feat: trigger dedupe window with suppression logging`.
- `vikram-daemon/src/triggers.rs` (482 lines): `TriggerKind`, persistence-backed quiet hours (`QUIET_ENABLED/START/END`, `is_quiet` :242), dedupe window with suppression reason. Do not re-plan this; if anything, expose the suppression log in `vikram daemon status` — that is a P2 at most.

### Worker E — documentation corrections
- Landed: `c8835ba docs: fix stale claims, unify counts, stamp superseded audits`.
- Consequence: root docs are closer to true than they were, but I still verified every claim I reused against source (§0). Two claims in circulation did **not** survive verification and should be corrected where they appear:
  1. "Trust::Model defaults in permission builders" — refuted; the only default is a `#[cfg(test)]` helper (`vikram-contracts/src/policy.rs:446,472`).
  2. "execution-resume missing" — half true; crash *repair* landed (`vikram-store/src/lib.rs:1329`) but turn *resumption* did not (§1.8). Phrase it that way from now on.

### Already good — do not "fix"
Deny-first policy with a mandatory floor (`vikram-policy/src/lib.rs:576-585`), seven postures (`contracts/policy.rs:101-118`), lane-parallel tool dispatch (`turn.rs:2717`), hybrid BM25+MMR memory (`memory.rs:385`), journal-projection transcripts (`compaction.rs:733`), progressive skill loading (`turn.rs:2547`), provider retry with `Retry-After` (`openai.rs:117-138`), bwrap/seatbelt sandbox with honest degradation notices (`vikram-sandbox/src/lib.rs:222-295`), trigger dedupe and quiet hours (`daemon/triggers.rs`).

### Dormant, not broken — decide, don't build
- `vikram-acp` (2,068 lines): a real ACP client+server, `EXPERIMENTAL — no crate in the workspace depends on this yet` (`vikram-acp/src/lib.rs:1-14`). Keep it; do not invest until there is a host (Zed-like editor) to connect.
- `vikram-gateway` (80,900 lines) and the platform adapters: no daily-driver impact. Leave alone.

---

## 7. What I Could Not Verify (be suspicious here)

Marked so the next reader does not inherit false confidence.

1. **Behaviour, not mechanism.** I verified what the code does; I did not run a single turn against a model. Claims like "the model wanders because there is no plan tool" are mechanism plus inference. The three-task before/after check in P0-3 is the cheapest way to falsify them.
2. **The >64 KiB pipe deadlock** is reproduced in isolation (`/tmp/opencode/probe`, thresholds 65,000 ok / 70,000 timeout) but not exercised through `vikram` itself. Run one `vikram chat "run cargo build"` to confirm end-to-end. If the default sandbox blocks cargo first (§2.7), you will see that failure instead — which is itself informative.
3. **§2.7 sandbox inference** (`$CARGO_HOME` read-only, no network) is read from the bwrap argument construction (`vikram-sandbox/src/lib.rs:262-295`) plus defaults (`config.rs:137-145`). I did not execute a build inside the jail.
4. **Silent-discard counts** are crude: [counted] non-test `let _ =` 317, `.ok()` 332, `unwrap_or_default()` 343, `.unwrap()` 295, using `awk` that stops at the first `#[cfg(test)]` in each file and skips `*/tests/*`. Many are legitimate (config fallbacks, `pragma_update(…).ok()`). No per-site audit was done; G19 stays PARTIAL and should not be quoted as "317 bugs".
5. **Not read**: `vikram-gateway` (80.9k lines), `vikram-evolution` internals, `vikram-daemon` beyond `triggers.rs`, `vikram-mcp` beyond `client.rs` signatures, `mcp.rs` registry behaviour (22 lines of glue). Anything I say about their internals is from module docs, and module docs are claims.
6. **Codex citations** point into a vendored snapshot of an enormous moving project; line numbers will drift on the next vendor update. The mechanism descriptions and file paths are the durable part.
7. **Claude Code / OpenCode / Amp / Manus** are entirely **[knowledge]**; no source here, no test. If a claim about them matters to a decision, verify it before acting.
8. **`max_steps`** — I traced `steps_used >= max_steps` to a single comparison that only emits a notice (`turn.rs:901-912`). I did not find another enforcement site but did not exhaustively trace every branch; confirm before documenting it as advisory.

If any of the above is wrong, the fix is cheap: the doc is a plan, not a contract, and every P0 item has a test that will tell you the truth.

---

## If you only do three things

1. **P0-1 + P0-2** (drain pipes, tail-preserving truncation). One afternoon. Without it the agent cannot read its own build errors, and no prompt or plan fixes that.
2. **P0-3** (work contract). Sixty lines of text, appended after the persona. Biggest behavioural change per token in this document.
3. **P0-4 + P0-5** (diff at approval, strict verification). This is what turns "it says done" into "I can see what it did and whether it's right".

Everything else in P1 can wait for the following week without the owner noticing the delay.
