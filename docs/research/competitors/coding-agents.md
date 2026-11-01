# Coding & development agents — competitive comparison and gap analysis

Spec Part 2B (survey) + Part 3 (diagnostics). Source-level evidence, file:line citations, per-project
detail and confidence grades live in
[`../implementations/coding-agent-internals.md`](../implementations/coding-agent-internals.md).
All repos cloned and read 2026-10-31. Claims below are **[H]** (code read directly) unless marked.

Scope: 7 engines — Codex CLI (Rust), Goose (Rust), OpenCode (TS/Effect), Gemini CLI (TS),
OpenHands (TS UI + Python SDK), Aider (Python), Claude Code (closed; public evidence only).

---

## A. Head-to-head: how the loops are actually built

| Dimension | Codex | Goose | OpenCode | Gemini CLI | OpenHands SDK | Aider | Claude Code |
|---|---|---|---|---|---|---|---|
| Loop shape | 3 nested `loop`s in one 3 612-line file (`run_turn` 763 LOC `:177-939`, `run_sampling_request` 105, `try_run_sampling_request` 869) | ordered pipeline of 17 pluggable `Operation`s + 1 `Inference` step; `StateMachine::run` 16 LOC (`machine.rs:241-256`) | 3 nested `while(true)` + inbox promotion (`runner/llm.ts:61-218`, `:237`) | recursive generator, `boundedTurns` decremented (`client.ts:990-1061`) | `while` over `Agent.step`, `max_iteration_per_run=500` | `while message:` + reflection counter, `max_reflections=3` | **ASSUMPTION**: single tool loop |
| Exit condition | no follow-up ∧ no pending input ∧ stop-hook doesn't block (`turn.rs:750-798`) | nothing applicable ⇒ `Yielded`; `Cancelled` | `needsContinuation`; step cap ⇒ synthetic prompt + `toolChoice:"none"`; 2× output-limit ⇒ fail (`llm.ts:249-271,336-366`) | no pending calls; `MaxSessionTurns`; `LoopDetected`; `AfterAgent` stop | iteration cap + `StuckDetector` (nudge once, then stuck) | no more edit blocks / 3 reflections | `complete-task`-like signal unknown |
| Loop LOC (core) | ~1 737 | 363 + 6 373 steps | 457 + 1 246 compaction | 1 357 + 542 + 1 018 scheduler | 3 262 local + 2 026 remote | ~700 | n/a |
| Codebase understanding | **shell `grep`/`rg` inside sandbox**; no read/grep tools, no index, no embeddings | agentic via MCP extensions; no index | `grep`/`glob`/`read` tools; no index; tree-sitter only for TUI highlighting | agentic tools; 141 K LOC context engine; no embeddings | agentic tools; tree-sitter only for **shell** semantics in `security/` | **tree-sitter def/ref graph + PageRank + binary-search token fit** (`repomap.py:470-572,669-691`) | `@file`/mentions, Grep/Glob tools (public); **ASSUMPTION** no index |
| Tool schema validation | typed `ToolSpec` + `ToolPayload`, `matches_kind`, trusted vs external registration + collision errors (`registry.rs:516,567,609`) | `rmcp::model::Tool` from `ToolBase`; **duplicate tool name = hard error** (`machine.rs:98-110`); 1 068-LOC provider schema repair | Effect `Schema` in separate `@opencode/schema` package | 7-state scheduler + `tool-modifier` arg rewriting | pydantic per tool | none (text fences) | published `.d.ts` contract, 13 440 lines |
| Parallelism | per-call `tokio::spawn` + `AbortOnDropHandle` (`tools/parallel.rs:272`) | `SubagentOperation::with_max_concurrency(GOOSE_MAX_CONCURRENT_SUBAGENTS, default 5)` | subagent `background:true` + `Job` service | parallel scheduler (`scheduler_parallel.test.ts`) | `agent/parallel_executor.py` (357 LOC) | serial | background subagents/teammates (public types) |
| Sandbox boundary | **OS**: Seatbelt `.sbpl` `(deny default)`, bwrap `--unshare-user --unshare-net` + Landlock, Windows MXC; approval = pure fn of policy×fs-policy | **none** — approvals + policy + OSV malware check on extensions | none (permission rules only) | per-OS `sandbox/` + TOML policy tiers + `sandboxPolicyManager` | container/remote workspace; tree-sitter shell semantics | none | `--restricted`, sandbox refs **[public/ASSUMPTION]** |
| Session persistence | JSONL rollout crate 18.5 K LOC: writer locks, reverse scanner, rolling compression, SQLite index | SQLite + session manager + migrations + HTML/MD export | **SQLite event-sourced** (`event-log.ts`, `durable-event-manifest.ts`) + stale-row settlement at boot | chat history + `context/` managers | event store per conversation, remote replay on reconnect | git only | resume/plan restore bugs in changelog **[public]** |
| Hooks | own crate; 10 events incl. `permission_request`, `model_request`, `compact`, `interrupt`; hooks can be MCP | `hooks/` + `StopHookOperation`, `SteerOperation` | `PluginHooks` namespaced dotted events, awaited before every step | registry/planner/runner/aggregator/translator + **`BeforeToolSelection`** (rewrite advertised tool list) | `POST /api/hooks` server-side | none | the entire engine is a hook pipeline; 4 first-party mods are plugins |
| MCP | `rmcp` (git pin) 42 K LOC client crate + resource tools | everything *is* MCP: `ExtensionConfig::{Stdio,Builtin,StreamableHttp}` | client + oauth + stdio + socket | client + manager + compliance transport | `sdk/mcp` | none | plugins can ship MCP w/ `headersHelper` **[public]** |
| Model independence | OpenAI Responses API assumed by the loop (`provider.capabilities()`, `RemoteCompactionSupport::V2`); Bedrock/AWS crates | **11 engines + 53 declarative JSON providers**; `manages_own_context()` changes the loop; llama.cpp/MLX local with tool-grammar parsing | provider-agnostic AI client, catalog + variants (`providerID/modelID#variant`) | Gemini-first + LiteRT local + Vertex/Code Assist + model router/fallback | LiteLLM-style + any OpenAI-compat + ACP guests | any litellm model | Anthropic-first; Bedrock/Vertex/Foundry/gateway **[public]** |
| Durable background work | `cloud-tasks`, `ext/queue`, `SleepHandler`, `wait_agent` | `scheduler/` + `recipe/` + `moim` | **`schedule` tool: cron/interval/at/delay + `if`/`until` shell-or-code gates + pause/resume**; `session/background.ts` feeds `experimental.idle` | `agent-scheduler`, a2a-server | `automation/`, cloud scheduled tasks **[public]** | none | background tasks + cron + cloud scheduled tasks **[public]** |
| Subagents | `thread_spawn.rs` fork modes, depth limit, roles "may reduce, never replace" (`agent/role.rs:3`), 2 generations coexisting | `ops_subagent.rs` (565) as a step, concurrency-capped | child session, fresh context, model override, background notify | `agentLoader` + `codebase-investigator` + **A2A client and server** | `sdk/subagent` | none | `agent.register/spawn`, `teammate`, `subagent_type`, `CLAUDE_CODE_SUBAGENT_EFFORT` **[public]** |
| Git/worktree | dedicated `worktree/` crate (1 062 LOC), TUI-managed worktrees, `ext/git-attribution` | worktree via extensions | **git-tree snapshots** for revert (`session/revert.ts:32-49`) | git via tools | `sdk/git` | auto-commit + `Co-authored-by` trailer | file checkpointing, `/diff` as a mod **[public]** |

**Two findings that reframe the category.**
1. **Nobody indexes code.** Across 2.2 M (codex) + 272 K (goose) + 80 K (opencode) + 141 K (gemini) LOC
there is **zero** embedding/vector retrieval for source. tree-sitter is used only to parse *shell
commands for safety* (codex, OpenHands) or for TUI highlighting (opencode). Aider's PageRank repo-map
is the only static-ranking design in the field — and aider has been **dormant since 2026-05-22**.
Agentic grep won on capability, so the "code intelligence" gap is open, not closed. **[H]**
2. **The leaders are converging on being harnesses, not engines.** OpenHands shipped an ACP agent
(4 782 LOC) that is 3× the size of its own agent (1 700 LOC) and its UI now drives Claude Code / Codex /
Gemini / OpenCode. Goose and OpenCode both expose ACP. Codex, Gemini CLI, OpenCode are separately
installable *as* ACP servers. **A portable wire protocol for coding agents exists and is spreading**
(`agentclientprotocol.com`). VIKRAM must implement ACP as a *client* (to host others) and a *server*
(to be hosted), or be excluded from the orchestration tier. **[H]** for the code sizes, **[M]** for the trend.

---

## B. Part 3 diagnostics, per question, with evidence

### B.1 What is tightly coupled?
- **Codex: the loop to the OpenAI Responses stream.** `run_sampling_request` iterates
  `ResponseEvent`s and knows `response.created`, `output_item.done`, incomplete-response usage,
  `use_responses_lite` interrupt frames, sticky-routing `ModelClientSession` (`turn.rs:1835-1939`,
  `:2899-2990`). Provider *capabilities* are enums consulted inside the loop
  (`provider.capabilities().remote_compaction`, `:247`) — a second provider seam inside control flow. **[H]**
- **Codex: turn state to a 20-object parameter bag.** `run_turn` needs `Session`, `TurnContext`,
  `StepContext`, `TurnDiffTracker`, `ModelClientSession`, `ContextManager`, 3 `CancellationToken`s,
  `McpStartupRequirements`, `ExtensionData`; `session/mod.rs` is 5 842 LOC. **[H]**
- **Gemini CLI: loop to `Config`.** `this.config.getMaxSessionTurns()`,
  `.getContextManagementConfig().enabled`, `.hasLatestFlashGAAccess()`, `.resetTurn()`,
  `hookStateMap` — the generator reads the god-config directly at ~10 points in `client.ts`. **[H]**
- **Goose: legacy `crates/goose/src/agents`** — `services.rs` 2 980 + `reply_parts.rs` 1 500 +
  `mcp_client.rs` 1 442 still own most behaviour; the clean `goose-agent` crate is only 2 654 LOC.
  Mid-extraction coupling. **[H]** for sizes, **[M]** for intent.
- **OpenCode: `Session`↔`Bus`↔`Database`↔`Plugin`.** The runner node declares 16 deps
  (`llm.ts:438-457`) and correctness depends on `plugins.awaitActivation` being re-run at 4 points;
  the inbox is SQL so the loop cannot be tested without a DB (their tests spin a real store). **[H]**
- **Universal:** history-shape is the deepest coupling in every codebase. Compaction, resume, tool
  dispatch, permissions and the TUI all read one concrete message/conversation type, which is why
  each project grew its own 1-2 K LOC history module (codex `context_manager/history.rs` 1 502,
  opencode `history.ts`+`compaction.ts`, gemini `context/*`, goose `session/*`). **[H]**

### B.2 What is replaceable (already, in someone else's hands)?
- **Model providers — Goose proves it.** 53 JSON files declare base_url/env-var/model costs/context
  limit/pricing against ~11 engines (`declarative/definitions/aigateway.json`); `manages_own_context()`
  then *deletes* compaction from the pipeline (`state_machine_builder.rs:70,99-113`). A provider is
  data + one capability bit. **[H]**
- **Capabilities — everything in Goose/OpenCode/Codex that speaks MCP** (`ExtensionConfig::Builtin`
  vs `Stdio` vs `StreamableHttp`; Codex `register_external*`). **[H]**
- **The whole engine, from OpenHands' point of view** — ACP subprocess, `agent_kind` per backend,
  model switching relayed (`src/constants/acp-providers.ts`, `use-switch-acp-model.ts`). **[H]**
- **Built-in features, in Claude Code** — diff pane, telemetry, AGENTS.md precedence, org policy
  isolation are mods under the same plugin API third parties use. That is the strongest possible
  replaceability signal and the clearest architectural lesson. **[H]** (public published source)
- **Not replaceable anywhere:** the compaction policy, the permission evaluator, or the tool registry
  are swappable at runtime by configuration. Codex's `ToolExposure` gates *visibility*, not
  implementation; Goose's `extra_operations` is the only true step-injection seam. **[H]**

### B.3 What is configurable?
Best→worst: **Codex** (10-layer config + a second, non-widenable *requirements* stack, MDM, cloud
bundles, per-directory layers, `features` flags, `--config` overrides, execpolicy DSL with
machine-proposed amendments) > **Goose** (`Config::global().get_param::<…>("GOOSE_*")`, TOML/YAML
providers, per-model limits, `stop_hook_block_cap`, `compaction_threshold`, `tool_call_cutoff`) >
**Gemini CLI** (tiered policy TOML w/ fractional priorities, context-management config, model
mappings, hooks, MCP, skills) > **OpenCode** (permission rulesets, `keep` headroom, background
summaries, agent `steps`) > **Claude Code** (extensive `CLAUDE_CODE_*` env surface — breadth **[public]**)
> **Aider** (flags; no layering). **[H]**

### B.4 What is extensible?
Five distinct seams exist in the wild: (1) **MCP tools** (everyone); (2) **hooks on loop events**
(codex `hooks/` crate, gemini `hooks/`, claude mods, OpenHands `POST /api/hooks`, opencode
`PluginHooks`); (3) **skills as markdown+assets** (codex `skills/parser.rs`, gemini `skillLoader`,
opencode `skill.ts` with `Directory|Url|Embedded` sources, claude bundled skills **[public]**,
OpenHands `sdk/skills`); (4) **step/operation injection** (goose `extra_operations` only);
(5) **declarative workflows** (goose `recipe/`, opencode `schedule`+`automation`, OpenHands
`sdk/automation`).
The **richest** and most instructive is Claude Code's: plugins can add *nouns* to the engine
(`interface EngineInterface { topo: Topo }` declaration merging) and register UI elements into a
cross-surface element table (terminal/desktop/vscode/mobile from one source). **[H]** (published `.d.ts`).

### B.5 What is hardcoded?
- Codex `TOOL_CALL` namespaces, `MemoryConsolidation` special cases and `SessionSource::Internal`
  branches *inside* `run_turn` (`turn.rs:759-768`), `PROTECTED_METADATA_PATH_NAMES`, per-provider
  capability enums, `"spawn_agent"|"send_message"|"followup_task"` string literals in
  `router.rs:70-77`, and `GUARDIAN_MAX_ROOT_MESSAGE_TOKENS`. **[H]**
- Goose's 17-step order is Rust code, not data (`state_machine_builder.rs:72-121`) — the ordering is
  the architecture and it is not user-configurable; defaults `5` subagents, `stop_hook_block_cap`,
  `max_turns` inline. **[H]**
- OpenCode: `CONTINUE_AFTER_INCOMPLETE_STREAM` / `CONTINUE_AFTER_OUTPUT_LIMIT` prompt strings,
  `SUMMARY_OUTPUT_FLOOR = 32_000`, `15 %`/`16 k` reserve, `2×keep ≥30 k ≤25 %` tail rule, the "2 retries
  then fail" limit, and the exact 4-message `SessionEvent` orderings. **[H]**
- Gemini CLI: `TOOL_CALL_LOOP_THRESHOLD=5`, `CONTENT_LOOP_THRESHOLD=10`, `MAX_HISTORY_LENGTH=5000`,
  `LLM_CONFIDENCE_THRESHOLD=0.9`, `MAX_TURNS`, tier bases 1..5 for policy priority. **[H]**
- Aider: `max_reflections=3`, `map_tokens=1024`. **[H]**
- Everywhere: the system/instruction prompt text and the tool-set names. **[H]**

### B.6 What is inefficient?
1. **Codex re-captures `StepContext` (fresh MCP discovery, environment resolution, plugin/skill
   injection) on *every* sampling iteration** (`turn.rs:526-562`) and re-clones the full history
   (`sess.clone_history()`, `:589`) — under a 205-crate build whose rollout crate alone is 18.5 K LOC. **[H]**
2. **Byte-heuristic token counting**: codex `approx_token_count` / `approx_bytes_for_tokens`
   (`history.rs:71-76`); aider estimates tokens by `sample_tokens/len*len_text` (`repomap.py:92-101`).
   Both then over-provision compaction because they cannot know the real count. **[H]**
3. **Compaction is implemented 2-4 times per product** (codex inline / remote V1 / remote V2 /
   async speculative; opencode auto / overflow / manual / background-precomputed; gemini
   chat-compression + context-compression + distillation + masking) — each variant re-reads history
   and re-tokenizes. **[H]**
4. **OpenCode queries SQLite for message rows inside the loop** (last-3 `json_extract` for the
   output-limit check, `llm.ts:338-353`) and runs `store.context(sessionID)` over the whole history
   at boot settlement (`:412`). **[H]**
5. **Gemini's LLM-assisted loop check** costs an extra model call at up to every 15th interval with a
   0.9 confidence gate — a token tax for a heuristic that is deterministic 95 % of the time. **[H]**
6. **Scale, generally**: 2.22 M LOC (codex) / 272 K (goose) / 141 K (gemini core) / 173 K (OpenHands SDK)
   for a loop that is conceptually 100 lines. `session/mod.rs` 5 842 and `ops_toolcalling.rs` 1 198 are
   where the maintenance cost actually lives. aider does the same job in 13 K. **[H]**

### B.7 What is insecure?
- **Goose has no OS sandbox.** Enforcement is approval prompts + policy + an **OSV** lookup on
  extension deps (`extension_malware_check.rs`). A user-approved `rm -rf` or a malicious stdio
  extension runs unsandboxed. **[H]** for absence in crates read; **[M]** for "nowhere".
- **Approval-vs-rewrite ordering is handled correctly by the two best engines** (verified, so VIKRAM
  must match it, not merely claim it): codex fires `PreToolUse` hooks which may rewrite the invocation
  and then **recomputes the guardian `serving_assessment` from the rewritten call**
  (`tools/registry.rs:855-886`, `apply_pre_tool_use` → `serving_assessment.authorization.permissions`
  refresh, `requires_review = true` on failure) *before* native review and the orchestrator's approval
  step; gemini runs `BeforeTool` (which can swap args via `tool.build(modifiedInput)`,
  `scheduler/hook-utils.ts:81-89`) and only *then* `checkPolicy(toolCall)`
  (`scheduler/scheduler.ts:644-680`, marked "1. Hook Check … 2. Policy & Security"). The engines
  without that ordering discipline (opencode: `tool.ts:121 beforeExecute` hook fires inside
  `executeTool`, permissions resolved separately by the caller at `subagent.ts:71`) are the ones where
  the ordering must be re-derived, not assumed. **[H]**
- **The real remaining insecurity is content-level, not call-level.** Gemini's taint model is the only
  one in the group, and it is a *command-shape proxy*: `hasTaintedOrBuildFileRisk()` = untrusted flags
  in the command (`findUntrustedFlags`, e.g. security-disabling git/npm switches) ∨ (build/test command
  ∧ modified build files) (`tools/shell.ts:306-313`), used to upgrade `ALLOW`→`ASK_USER`/`DENY`
  (`scheduler.ts:685-695`) and — the good part — to **refuse persisting an "always allow" amendment**
  (`getPolicyUpdateOptions` returns `undefined`, `shell.ts:316-330`). But **no engine carries a trust
  label from an observation (file read, web fetch, MCP/skill text, hook output) into the next model
  request**, so poisoned repo content still flows into a "clean" ALLOW. Codex has `ToolExposure::hidden`
  and a `permission_request` hook event; nobody else models read-grants except codex's
  `windows_sandbox_read_grants` (`core/src/windows_sandbox_read_grants.rs`). **[H]** for the code, **[M]**
  for the gap.
- **Prompt-injection surface is structurally unhandled everywhere**: skill bodies, MCP tool
  descriptions, hook output (`hooks/output_spill.rs`), plugin-install as a *model-facing tool*
  (`request_plugin_install` — the model can ask the user to install a plugin), and goose's
  `extension_malware_check` is reputation, not containment. **[H]**
- **Gemini policy integrity is partial**: `policy/integrity.ts` + `stable-stringify.ts` exist, but
  fractional priorities (`4 + 950/1000`) invite precedence surprises, and a **client-initiated** call
  (i.e. anything launched by a slash command) downgrades `ASK_USER` straight to `ALLOW` and returns,
  skipping the prompt entirely unless `additional_permissions` is set (`scheduler/policy.ts:76-89`) — a
  UI-origin privilege asymmetry; genuinely non-interactive `ASK_USER` at least hard-throws (`:94-103`).
  **[H]**
- **OpenCode keeps tool-produced images as inline base64 data URIs in history** (resized, with
  `[N images omitted: …]` notes, `tool.ts:92-117`), so a resumed session re-uploads pixel payloads on
  every subsequent request; OpenHands' ACP path inherits whatever file grants the guest CLI has
  (`acp_file_credentials.py`, 539 LOC). **[H]**
- **OpenCode persists tool base64 images into history** (`tool.ts:92-120` normalizeImages) and
  OpenHands' ACP path inherits whatever the guest CLI's file grants are
  (`acp_file_credentials.py`); Codex has an explicit `windows_sandbox_read_grants` module, nobody
  else models read-grants at all. **[H]**

### B.8 What is unnecessarily complex?
- **Codex: 205 crates and coexisting generations** — `guardian` + `guardian-v2` + `guardian-v3`,
  `multi_agents` + `multi_agents_v2`, `compact_remote_history` + `compact_remote_v2*` + `compact.rs`,
  `realtime_*` voice inside `core/src`, `code-mode` (embedded V8!) *and* tool calls *and* shell,
  three nested loops in one file, 5 842-line `session/mod.rs`. The runtime is a product surface, not a
  kernel. **[H]**
- **Gemini CLI: 4 parallel context subsystems** (`context/` graph+pipeline+processors+profiles,
  `chatCompressionService`, `contextCompressionService`, `toolDistillationService`,
  `toolOutputMaskingService`, `memoryContextManager`) plus a 26-file hook pipeline plus a 27-file
  policy dir for a terminal agent. **[H]**
- **OpenCode: Effect-TS DI graph** (16 deps for one runner node, `makeLocationNode`, `FiberMap`,
  `Effect.uninterruptibleMask` inside the loop) — real correctness benefits, but the loop is
  unreadable without Effect expertise, and inbox-as-SQL makes a 30-line concept into 457 lines. **[H]**
- **OpenHands: three runtimes for one product** (canvas UI, agent-server, SDK) plus an ACP path that
  mostly duplicates the native path. **[H]**
- **Goose: two agent codebases** (`crates/goose/src/agents` legacy god-objects + `crates/goose-agent`
  clean kernel) — the good part is 2 654 LOC; the rest is what it is escaping. **[H]**

---

## C. Gap analysis → what VIKRAM should build (and what it must not copy)

**C.1 Copy these, structurally.**
1. **Goose's `Operation`/`Effect` kernel** (`goose-agent/src/machine.rs`, 363 LOC) as VIKRAM's
   `vikram-runtime`: ordered steps, each `-> NotApplicable | Applied(effects)`, effects applied by an
   injected `EffectHandler`, session reloaded by id each iteration, cancellation *completes* by
   synthesizing error tool-responses. This is the only design in the field that is simultaneously a
   loop, a resume mechanism, and an extension seam — and it is 363 lines, not 1 737. It also compiles
   to wasm via `MaybeSend`.
2. **Provider-as-data** (goose 53 JSONs + ~11 engines) *plus* **provider capability flags that
   compose the loop** (`manages_own_context()` removing compaction). VIKRAM's model independence must
   cover API+local with the tool-call grammar layer goose actually ships
   (`native_tool_parsing.rs`, `tool_emulation.rs`, `prompt_template.rs`, llama.cpp metal/cuda/vulkan + MLX).
3. **OpenCode's durable semantics**: inbox rows with `input` vs `steer` delivery classes, compaction
   as a queueable control item rather than a loop side effect, boot-time `settleStaleCompactions` /
   `settleStaleToolCalls`, `{action, resource, effect}` permission rules with deny-first evaluation, the
   **`Unattended` ruleset** for headless work, and the `schedule` tool (cron/interval/at + `if`/`until`
   shell-or-code gates + pause/resume). Together these are the reference for "durable background tasks".
4. **Codex's security kernel**: `(deny default)` seatbelt `.sbpl` + bwrap `--unshare-net` + Landlock +
   Windows backend as one `sandboxing` crate; `default_exec_approval_requirement` as a *pure function*
   of (policy × filesystem policy); a separate network approval lane; execpolicy as a small prefix-DSL
   whose denials return a machine-proposed amendment; 10-layer config with a **non-widenable
   requirements tier**; `arg0` single-binary dispatch; `worktree` crate for isolated parallel attempts.
5. **Two-tier tool registry**: codex's trusted-vs-external registration + collision tracking +
   `ToolExposure` (visibility is policy, not existence); goose's duplicate-tool hard error.
6. **Claude Code's dogfood rule** (public evidence): first-party features ship as plugins on the same
   hook API — `engine.create` as a fold where a plugin can add a noun. VIKRAM's built-ins must be
   built on `vikram-ext` from day one, or the API will calcify around private internals.

**C.2 Attack these gaps.**
- **G1 Code intelligence.** Nobody indexes. Agentic grep is the accepted answer and its cost is token
  and latency per turn (B.6). aider's repo-map is 5 months dormant and only ranks *definitions*, not
  impact. Build `vikram-codeindex`: tree-sitter def/ref/call graph + impact analysis + optional
  embeddings, exposed as (a) ranked context injection and (b) a *tool* the model can query — and make
  it an optional step in the sense goose makes compaction optional (index absent ⇒ degrade to grep,
  never fail).
- **G2 Insecurity that is unmodelled, not unsolved.** No project carries a trust label with content
  (B.7). VIKRAM's runtime-enforced security should add: taint/trust on every observation, an
  authority check *after* hook input rewriting (close codex's TOCTOU ordering), capability-scoped
  read grants as the default (only codex has `windows_sandbox_read_grants`; nobody has a POSIX
  equivalent), and sandbox-optional-but-policy-first for hosts like goose's users.
- **G3 ACP is the orchestration rail and it is unclaimed in Rust.** Implement
  `vikram-acp` as *both* server (be hostable, like codex/gemini/opencode) and client (host Claude
  Code/Codex/Gemini like OpenHands). Cheap, differentiating, and it turns VIKRAM's model-independence
  into agent-independence.
- **G4 The verification loop is the weakest part of every engine.** Evidence: codex has `TestSyncHandler`
  and `turn_diff_tracker.rs`; gemini has `complete-task` + plan mode; opencode has `file-diff`+`todo`;
  goose has `DoctorOperation`/`ExitOnErrorOperation`. **None** has a first-class
  edit→build→test→re-repair loop with a budget, a failure taxonomy, or "did the diff actually change
  behaviour" gate — that lives in prompts and in the model's goodwill. VIKRAM should make
  `VerificationOperation` a *step* (goose-shaped) with per-attempt worktree isolation (codex-shaped)
  and durable scheduling (opencode-shaped). This is the single biggest capability gap in the category.
- **G5 Loop-level cost accounting is heuristic everywhere** (byte approximations, re-cloned histories,
  2-4 compaction implementations). A deterministic token meter at the provider boundary + one
  compaction strategy + a byte/token property test is enough to beat all seven.

**C.3 Do not copy.**
- Codex's shape: 205 crates, three nested loops in one 3 612-line file, 5 842-line session module,
  coexisting v2/v3 generations of the same subsystem, V8 *and* shell *and* tools.
- Gemini's 4-subsystem context stack and fractional-priority policy tiers.
- OpenCode's Effect-TS DI depth (keep its *semantics*, in plain Rust traits).
- OpenHands' three-repo split where the UI cannot run without a server.
- aider's 3-reflection termination — it is an artifact of no state machine, not a design.

**C.4 Best structural references, ranked for a Rust runtime.**
1. **`block/goose` `crates/goose-agent` (2 654 LOC) + `state_machine_builder.rs`** — the kernel to
   imitate, near-verbatim: trait shapes, effect/command pattern, cancellation finalization,
   provider-capability-driven step composition, wasm-safe bounds.
2. **`openai/codex` `sandboxing/`, `execpolicy/`, `config/`, `rollout/`, `arg0`, `worktree`** —
   the security, policy-DSL, layered-config-with-admin-floor, append-only-session-with-index, and
   single-binary seams. Imitate the *crate boundaries*, not `core/src`.
3. **`sst/opencode` `packages/core/src/session/`** — the durable-state semantics: inbox/steer,
   compaction-as-data, stale-row settlement, permission triple + `Unattended`, `schedule` tool,
   git-tree snapshots for revert. Read it as a spec, re-implement in Rust.
4. `Aider-AI/aider/repomap.py` — the retrieval algorithm (tree-sitter graph → weighted PageRank →
   rank redistribution to definitions → binary-search token fit → on-disk tag cache).
5. `google-gemini/gemini-cli` — the policy-engine and hook-event *catalogue* (not its code), plus
   `BeforeToolSelection` and explicit loop detection.
6. `anthropics/claude-code/mods/*` + `mods/types/claude-code.d.ts` — the extension-API surface to
   aim at, and the proof that first-party features can live on it.
