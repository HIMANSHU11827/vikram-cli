# Coding-agent internals: source-level findings

Spec Part 2B. Method: `git clone --depth 1` of real sources into `/tmp/ca-research/` (shared
`/tmp/vikram-research/` was being clobbered by parallel agents), then read/grep the actual loop,
dispatch and sandbox files. Access date for every repo: **2026-10-31**. Confidence: **[H]** code
read directly at the cited line, **[M]** inferred from code not fully read / docs of a cloned repo,
**[L]** secondary. Paths below are relative to each repo root.

| Repo | URL | HEAD (date) | License | Size |
|---|---|---|---|---|
| Codex CLI | https://github.com/openai/codex | `67f0994` 2026-10-31 | Apache-2.0 | 205 crates, 5 702 `.rs`, 2.22 M RS LOC (incl. tests/snapshots); `core/src` non-test **152 704** |
| Goose | https://github.com/block/goose | `b23d21c` 2026-10-31 | Apache-2.0, v1.57.0, rust 1.94.1 | 15 crates, 272 633 RS LOC (excl. tests) |
| OpenCode | https://github.com/sst/opencode | `bf544cd` 2026-10-31 | MIT | 36 packages, 4 794 `.ts` + 833 `.tsx`, 34 `.rs`; `packages/core/src` non-test **80 893** |
| Gemini CLI | https://github.com/google-gemini/gemini-cli | `6814e00` 2026-10-30 | Apache-2.0 | 6 packages, core non-test **141 507** TS LOC, 1 045 test files |
| OpenHands UI | https://github.com/All-Hands-AI/OpenHands | `b4a3f45` 2026-10-31 | MIT | `@openhands/agent-canvas` v1.34.0 — now a React/Electron **UI only** |
| OpenHands SDK | https://github.com/All-Hands-AI/agent-sdk | `98de8bf` 2026-10-31 | MIT | 4 py packages, **173 451** py LOC (non-test) |
| Aider | https://github.com/Aider-AI/aider | `5dc9490` **2026-05-22** | Apache-2.0 | 13 359 LOC (`aider/*.py`), 20 285 incl. coders/queries |
| Claude Code | https://github.com/anthropics/claude-code | `80c6959` 2026-10-31 | **proprietary** | closed engine; public `mods/` + `mods/types/claude-code.d.ts` (13 440 lines) |

GitHub traffic/open work (REST, 2026-10-31, rate-limited so only 3 retrieved):
codex 132 407 ★ / 25 808 open issues+PRs; gemini-cli 108 643 ★ / 766; aider 49 848 ★ / 1 934.
Others not retrieved — **[L]**.

---

## 1. OpenAI Codex CLI (`codex-rs`) — highest structural value

**Loop.** `core/src/session/turn.rs:163-176` documents the contract: each sampling request returns
either function calls or an assistant message; tool call → execute → feed back; assistant-only →
turn ends. `run_turn` (`:177`) is ~600 lines of preamble (pre-sampling compaction `:218`, MCP startup
requirements `:270`, step-context capture `:294`) then `loop {` at `:490`:
pending-input drain → `run_hooks_and_record_inputs` (`:503`, breaks on stop) → `capture_step_context`
(`:526`) → `run_sampling_request` (`:594`) → post-sampling token status (`:627`) →
`async_compaction::maybe_start` (`:639`, mid-turn vs post-turn phase) → rollover decision
`:691-708` → `continue` → else stop-hook (`:752`) → `break`.
Termination is therefore: *no follow-up needed AND no pending input AND stop-hook does not block*
(`:750-798`). Stop hooks can force continuation by injecting a prompt (`:769-786`) — the same
"stop hook can veto completion" pattern as Claude Code. A second loop inside
`run_sampling_request` (`:1835`) and a third in `try_run_sampling_request` (`:2899`,
`let outcome: CodexResult<SamplingRequestResult> = loop {`) consume the streaming response and drive
tool execution — **three nested loops in one 3 612-line file**. `[H]`

`ModelClientSession` is turn-scoped and reused across retries to keep sticky routing/WebSocket state
(`:481-487`). `[H]`

**Tool dispatch.** `core/src/tools/` is 24 modules: `router.rs:344-483` (`dispatch_tool_call_with_state`)
maps a `ToolCall` to a `RegisteredTool`; `registry.rs:428+` holds `ToolRegistry` with
`register_trusted` vs `register_external` (`:516`, `:567`) — external (plugin/MCP) tools take a
different, checked registration path and collisions are recorded (`record_collision` `:609`,
`ToolCollision` error surfaces in the loop at `:239`). Exposure is a 6-value enum
(`router.rs:42-47`): `direct`, `direct_model_only`, `deferred`, `deferred_model_only`,
`code_mode_only`, `hidden` — tools exist but are *not advertised* until conditions hold.
`ToolRuntime` trait (`sandboxing.rs`) + `orchestrator.rs:36` (`ToolOrchestrator`) own
approval → sandbox attempt → denial-retry. `parallel.rs:272` spawns each handler into
`AbortOnDropHandle` tasks; `executed_tool_calls.rs` (758 lines) records idempotency by output. `[H]`

**Built-in tool set is tiny and shell-centric.** `spec_plan.rs` registers: `exec_command` (`:1267`),
`write_stdin` (`:1268`), `apply_patch` (`:1306` region / `handlers/apply_patch.rs`), `plan` (`:1302`),
MCP resource list/read (`:1283-1289`), `request_permissions` (`:1366`), `new_context_window` (`:1372`),
`get_context_remaining` (`:1378`), `current_time`, `sleep`, plugin-discovery tools
(`:1341`, `request_plugin_install`), multi-agent `spawn_agent`/`send_message`/`wait_agent`
(`handlers/multi_agents*.rs`), `view_image`, and code-mode `execute`/`wait` (`:1095-1100`).
**There is no read/grep/glob/search tool in core** — the model greps through the sandboxed shell. `[H]`

**Codebase understanding: no index, no embeddings.** Tree-sitter appears only in
`shell-command/src/command_safety/powershell_tree_sitter.rs` and `bash.rs` — i.e. for *parsing the
command the model wants to run*, not the repository. Zero vector/embedding retrieval anywhere in
`core`. `[H]`

**Sandbox boundary.** Separate crate `codex-rs/sandboxing/src/`: macOS Seatbelt with real policy
files (`seatbelt_base_policy.sbpl` starts `(deny default)` and whitelists per-sysctl;
`seatbelt_network_policy.sbpl`, `seatbelt_preferences_policy.sbpl`,
`seatbelt_read_only_platform_defaults.sbpl`), Linux `bwrap.rs:88-90`
(`--unshare-user --unshare-net --ro-bind`) plus `landlock.rs` (`create_linux_sandbox_command_args_for_permission_profile`),
Windows `windows.rs` + `windows_mxc.rs` + `core/src/windows_sandbox.rs`. Policy DSL in
`execpolicy/` (`parser.rs:57` steinbock/serde-value based, `rule.rs:40 PrefixPattern`,
`PatternToken` alternation, `NetworkRule` with protocol enum, `amend.rs` = machine-suggested
rule widening returned to the user as `proposed_execpolicy_amendment`, `sandboxing.rs:228`).
Approval decision is a pure function of policy × filesystem policy
(`default_exec_approval_requirement`, `core/src/tools/sandboxing.rs:200-230`): `Never` → no approval,
`UnlessTrusted` → always ask, `OnRequest`/`Granular` → ask only when `FileSystemSandboxKind::Restricted`,
and `Granular` that disallows sandbox approval becomes **`Forbidden`** rather than silently prompting. `[H]`
Network has its own approval lane (`tools/network_approval.rs`, 1 415 lines) so sandbox retry and
network retry don't share state (`orchestrator.rs:1-5` comment is explicit about re-review on
escalation). `[H]`

**Config.** `config/src/loader/mod.rs:124-146` documents the layer stack, low→high: package, admin
(macOS MDM profiles), system `/etc/codex/config.toml`, cloud enterprise bundle, user
`$CODEX_HOME/config.toml`, `<name>.config.toml` profile, `$PWD/config.toml`, directory-tree
`.codex/config.toml`, repo-root `.codex/config.toml`, runtime (`--config`, UI model picker).
Untrusted directories are *loaded but disabled* (`:132-138`). A parallel **requirements** stack
(`:109-121`) carries admin-enforced constraints that config layers cannot widen, plus
`strict_config.rs`, `constraints.rs`, `permissions_toml.rs`, `project_trust.rs`. `[H]`

**Persistence.** Dedicated `rollout/` crate (18 522 LOC): append-only JSONL per thread
(`recorder.rs`), `writer_lock.rs` for concurrent appenders, `reverse_jsonl_scanner.rs` +
`seekable_reader.rs` for resume without loading whole files, `compression/` (materialize-on-append
rolling compression, `lib.rs:112`), `session_index.rs` + `state_db.rs` (SQLite index over rollouts),
`maintenance.rs`, `rollout_file_name.rs`. Resume also has `core/src/session/rollout_reconstruction.rs`
and `thread-store/src/local/rollout_migration*`. `[H]`

**Subagents.** `core/src/agent/` (control.rs 778, registry.rs 408, role.rs 425, history.rs 483,
child_config.rs 374) + `core/src/thread_spawn.rs:1-45` (`ThreadCreateRequest{task_name, fork_mode,
agent_type, model, reasoning_effort}`; `fork_mode: FullHistory` = native fork, `None` = fresh) with
`next_thread_spawn_depth` → depth-limited trees; roles may *reduce* capability but
"never replace the parent" (`agent/role.rs:3`). Two generations coexist:
`tools/handlers/multi_agents.rs` and `multi_agents_v2.rs` (`MultiAgentVersion` in protocol).
`agent-graph-store`, `agent-message-board-client`, `agent-identity` are separate crates. `[H]`

**Compaction.** `core/src/compact.rs` (928) + `session/async_compaction.rs` + `compact_remote_v2.rs`
(provider-side compaction, capability-gated by `provider.capabilities().remote_compaction`,
`turn.rs:247`) + `compact_token_budget.rs` + `compact_retained_tool_outputs.rs` +
`compact_model_fallback.rs` + `context_manager/history.rs` (1 502: `for_prompt` `:631`,
byte-approximate token accounting `:691`, output truncation policies `:597-600`,
`remove_first_item` + orphan-call normalization `:726-735`, `truncate_before` turn-aware cut
`:842-922`). Window rollover is a first-class tool (`new_context_window`). `[H]`

**Hooks / skills / plugins.** `hooks/` crate with `events/`: `pre_tool_use`, `post_tool_use`,
`user_prompt_submit`, `session_start`, `session_end`, `stop`, `compact`, `interrupt`,
`permission_request`, `model_request` — plus `engine/`, `registry.rs`, `schema.rs`, `mcp.rs`
(hooks can be MCP servers), `output_spill.rs` (large hook output to disk), `legacy_notify.rs`.
`skills/` crate: `loading.rs`, `parser.rs` (SKILL.md frontmatter), `mentions.rs`, `selection.rs`,
`assets/samples/`. Plugins: `plugin-host`, `core-plugins` (51 032 LOC), `core-plugin-common`,
`plugins/` in core, `config/src/plugin_edit.rs`, `marketplace_edit.rs`; plugin install is itself a
model-facing tool (`request_plugin_install`, `list_available_plugins_to_install`). `[H]`

**Single-binary dispatch.** `arg0/src/lib.rs`: `apply_patch`, the Linux sandbox helper, and the
exec/fs helpers are all the same executable selected by argv[0] (`CODEX_LINUX_SANDBOX_ARG0`,
`CODEX_ARG0_EXEC_HELPER_ARG1`, `APPLY_PATCH_ARG0`) with a resolved-path table handed to children as
`Arg0DispatchPaths`. `[H]`

**Git/worktree.** `codex-rs/worktree/` crate (1 062 LOC) used from
`tui/src/worktree_startup.rs`, `worktree_browser.rs`, `managed_worktree_tools.rs`,
`exec/src/lib.rs` — the TUI owns *managed worktrees* per session; `git-utils` crate +
`core/src/git_info.rs` (`rev-parse --abbrev-ref HEAD`, `git_diff_to_remote`) attribute commits
(`ext/git-attribution`). `[H]`

**Model/transport.** OpenAI Responses API is hardwired into the loop: `client.rs`,
`client_common.rs`, `responses_retry.rs`, `streaming events`, `provider.capabilities()` enum
(`RemoteCompactionSupport::V2`). Providers are `codex-backend-openapi-models`, `aws-auth`,
`cloud-client`, `login`; there is no generic third-party provider trait visible in `core`. **[M]**
`code-mode-runtime` embeds **V8** (`v8 = { features = ["v8_enable_sandbox"] }`, `deno_core_icudata`)
so the model can write JS cells that call tools programmatically (`tools/runtimes/`, `code_mode_host.rs`). `[H]`

**Cost of the growth.** `session/mod.rs` is 5 842 lines; `guardian*` (review agent), `realtime_*`
(voice), `ext/guardian-v2`, `ext/guardian-v3` (versioned-in-parallel) show features kept as
coexisting generations rather than replaced. 205 crates for one binary. `[H]`

---

## 2. Goose (`block/goose`) — the modularity reference

**Loop = ordered pipeline of pluggable Operations + one Inference step.**
`crates/goose-agent/src/machine.rs:59-63` `StateMachine{ steps: Vec<Step>, cancel, interrupted_step }`;
`Step::Operation | Step::Inference` (`:45-48`); `Operation` trait (`operation.rs:91-150`) requires
`run`, and optionally `inference_tools`, `prompt_parts`, `moim_parts`, `finalize_cancellation`,
`run_command`; `Inference: Operation` adds `applies(conversation)` + `prepare_session` + `infer`
(`:160-177`). `step()` (`:162`) walks the list, **first step whose precondition applies wins**, each
returning `OperationResult::{NotApplicable, Applied}` (`:203-216`); `run()` (`:241-256`) reloads the
session **by id from storage on every iteration**, applies effects through an `EffectHandler`, and
loops while `RunStatus::Continuing`. Termination = `Yielded` (nothing applicable) or `Cancelled`. `[H]`

Three properties make this the best Rust template: (1) **effects, not mutations** — steps return data
that a `runtime: EffectHandler` applies, so the loop has no DB or I/O dependency; (2) **context is
reloaded per step**, so a crashed/interrupted engine resumes from durable state (`:246`);
(3) **cancellation is completed, not abandoned** — `interrupted_response` (`:73-96`) synthesizes
`CallToolResult::error("Tool call was interrupted before completing")` for every unanswered
`ToolRequest` so history can never hold an orphan tool call, then `finalize_step_cancellation` runs
per step (`:295-314`). Also `#[cfg_attr(not(target_arch="wasm32"), async_trait)]` + `MaybeSend`
throughout ⇒ same core compiles to wasm. `[H]`

**Loop composition is data-driven.** `crates/goose/src/agents/state_machine_builder.rs:46-147`:
17 named operations in explicit order (ToolApproval, Skill, Recipe, ToolExecution,
RemainingToolCalls, Subagent(Concurrency), Steer, BangShell, [Compaction], ToolPairCompaction,
Doctor, Project, Retry, EmptyResponse, StopHook, ExitOnError, MaxTurns) + a
`SlashCommandOperation` first and `Step::Inference` last; `extra_operations` is appended (`:123`) —
that is the extension seam. Crucially: `let manages_own_context = provider.manages_own_context();`
(`:70`) **removes** `CompactionOperation` (`:99-106`) and disables `tool_pair_summarization` (`:112`)
when the provider compacts server-side; and `SubagentOperation::with_max_concurrency(Config::global()
.get_param::<NonZeroUsize>("GOOSE_MAX_CONCURRENT_SUBAGENTS").unwrap_or(5))` (`:90-94`). `[H]`

**Steps live one file per concern** (`agents/state_machine/`, 6 373 LOC): `ops_toolcalling.rs` 1 198,
`ops_subagent.rs` 565, `ops_skills.rs` 464, `ops_recipe.rs` 394, `ops_remaining_tool_calls.rs` 386,
`ops_llm.rs` 369, `ops_compaction.rs` 332, `ops_tool_approval.rs` 289, `ops_retry.rs` 279,
`ops_auto_effort.rs` 267, plus `ops_steer`, `ops_stop_hook`, `ops_maxturns`, `ops_doctor`,
`ops_empty_response`, `ops_exit_on_error`, `ops_bang_shell`, `ops_tool_pair_compaction`, `ops_project`.
`ops_remaining_tool_calls.rs` exists to answer tool calls left unanswered when a turn is cut short —
the same invariant goose enforces at machine level, defended twice. `[H]`

**Model independence is the strongest in class.** `crates/goose-providers/src/` has engine
implementations (`anthropic.rs`, `openai.rs`, `openai_compatible.rs`, `google.rs`, `azure_foundry.rs`,
`databricks*.rs`, `snowflake.rs`, `openrouter*.rs`, `ollama.rs`, `local_inference.rs`, plus
`openai_live*.rs`/`live_voice_provider.rs` for realtime) and **53 declarative JSON provider
definitions** in `declarative/definitions/*.json` compiled in via `include_str!`
(`declarative/macros.rs:12`). Each declares `engine`, `base_url`, `api_key_env`, `catalog_provider_id`,
`dynamic_models`, and per-model `context_limit` + `input_token_cost`/`output_token_cost`/`currency`
(see `definitions/aigateway.json`). Adding a provider = adding a JSON file, no Rust. `[H]`

**Local inference is real.** `crates/goose-local-inference/` (10 253 LOC) on `llama-cpp-2`
with `metal`, `cuda`, `vulkan`, `mtmd` (multimodal) features (`Cargo.toml:16-17, 53`) + `mlx.rs` for
Apple. The hard part is present: `native_tool_parsing.rs` (343), `tool_emulation.rs` (760),
`tool_parsing.rs`, `thinking_output.rs`, `prompt_template.rs`, `hf_models.rs`, `huggingface_auth.rs`,
`config_resolver.rs`, `multimodal.rs`. Local models are exposed through the *same* provider trait,
including a `manages_own_context` answer. `[H]`

**Tools/extensions.** Everything is MCP-shaped: `rmcp` 3.4.1 in workspace deps
(`Cargo.toml` `[workspace.dependencies]`), `ExtensionConfig::{Stdio, Builtin, StreamableHttp}`
(`crates/goose/src/agents/extension.rs:158-210`), `agents/mcp_client.rs` (1 442), `crates/goose-mcp/`
(6 084). Tool definitions come from `rmcp::model::Tool` and are built from a typed `ToolBase`
(`goose-agent/src/tool.rs:34-56`, carrying title/output-schema/annotations/icons/meta);
`ToolProvider` trait (`tool.rs:125-180`) + `ToolOperation` (`:193`) wrap it as a machine step;
duplicate tool names across operations are a **hard error** (`machine.rs:98-110`, test at `:340`).
Provider JSON-schema quirks get their own 1 068-line repair crate file
`agents/tool_schema_normalize.rs`; `agents/validate_extensions.rs` (247) checks configs. `[H]`

**Security/permissions.** `crates/goose/src/permission/` (`permission_judge.rs`,
`permission_inspector.rs`, `permission_store.rs`), `agents/tool_confirmation_coordinator.rs` (300),
`agents/state_machine/ops_tool_approval.rs`, `security/`, `tool_inspection.rs`, `tool_monitor.rs`.
`agents/extension_malware_check.rs` queries **Google OSV** (`OSV_ENDPOINT` overridable,
`DEFAULT_OSV_ENDPOINT`) for declared extension dependencies before use. There is **no OS-level
sandbox** in goose — no landlock/seccomp/seatbelt equivalent — enforcement is approval + policy +
inspection. **[H]** for the code layout, **[M]** for "no sandbox" (absence-of-evidence in the crates
I read).

**Persistence/other.** `crates/goose/src/session/session_manager.rs` + SQLite,
`chat_history_search.rs`, export to HTML/Markdown, `legacy.rs` migrations;
`skills/builtin.rs` + `skills/builtins/`; `scheduler/` (`full.rs`) for durable timed runs;
`recipe/` + `recipe_deeplink.rs` (declarative workflow bundles); `gateway/`; `crates/goose/src/acp/` +
`agent-client-protocol-schema` dep + `crates/goose-acp-macros` ⇒ **Goose also speaks ACP**;
`crates/goose/src/plugins/`; `crates/goose/src/hooks/mod.rs`; `moim.rs` (memory-of-intent
injection, `operation.rs:137 moim_parts`). `crates/goose/src/agents/services.rs` is 2 980 LOC and
`reply_parts.rs` 1 500 — the legacy `crates/goose/src/agents` god-objects the new `goose-agent`
crate is being carved out of. **[H]** for layout, **[M]** for the carving narrative.

---

## 3. OpenCode (`sst/opencode`) — durable-state machine in TypeScript

**It is no longer Go/Rust.** 4 794 `.ts` + 833 `.tsx` vs 34 `.rs`; the only Rust is
`packages/process-daemon` (pty/terminal daemon, unix/windows transports, ownership, musl). The core
is Effect-TS with explicit DI: `runner/llm.ts:438-457` lists 16 deps for one node. `[H]`

**Loop.** `packages/core/src/session/runner/llm.ts:61-218` `drain` → two nested `while(true)`
(`:82`, `:209`) plus `runStep`'s retry `while(true)` (`:237`). Input arrives through a **durable inbox
table** with two delivery classes: `"input"` (turn-start) and `"steer"` (mid-turn), promoted by
`SessionInbox.nextPromotable/promote/serialized` (`:70`, `:92`, `:187`). Compaction and "move" are
**inbox items too** (`:100-127`), i.e. a context op is queued and executed like a message rather than
being a side effect inside the loop. `[H]`

**Crash recovery is designed-in, not incidental.** Before running, `settleStaleCompactions` (`:379-407`)
finds `compaction` rows with `status='running'` in SQLite and marks them failed
("process death skipped the finalizer"), and `settleStaleToolCalls` (`:409-432`) fails any tool in
`streaming|running` state — propagating the child session id for interrupted subagents
(`:417-418`). Same invariant as goose, implemented against SQL. `[H]`

**Termination.** `needsContinuation` from the step outcome (`:312-319`) plus explicit policies: agent
step cap appends a synthetic assistant message `MAX_STEPS_PROMPT` **and sets `toolChoice: "none"`**
while keeping tool definitions for provider prefix-cache (`:249-271`); output-limit continuation
re-prompts with "break the work into smaller pieces" and hard-fails after **2** consecutive
`finish === "length"` assistant messages (`:336-366`, `CONTINUE_AFTER_OUTPUT_LIMIT :40`); interrupted
stream ⇒ synthetic "continue without repeating" (`:37`). `[H]`

**Compaction.** `session/compaction.ts` = 1 246 lines. Reserve headroom `keep` = 15 % of window,
min 16 k tokens (`:45-48`); `SUMMARY_OUTPUT_FLOOR = 32_000` (`:140`); **speculative background
summary** started when the context nears the threshold and swapped in later
(`:49`, `:75-80`, `:112-119`, called from the loop at `:248` `compaction.precompute`);
structured summary template + rules (`:167-212`) which explicitly instructs the model to *drop*
AGENTS.md/conventions/environment from the summary because "the next agent receives current versions
separately" (`:210`); checkpoint rewrite when the template changes (`:226-231`); tail kept verbatim
= `2×keep`, ≥30 k, ≤25 % of budget, else re-summarize from scratch (`:159-160`). Measured-vs-estimated
token accounting (`:57-58`). `[H]`

**Tools.** `packages/core/src/tool/plugin/*.ts` = 19 tools (read, glob, grep, edit, write,
file-diff, patch, shell, webfetch, websearch, question, todo, skill, subagent, mcp-resource,
image-generate, artifact, opencode, **schedule**); prompt/spec text kept as data
(`tool/patch.txt`, `tool/todowrite.txt`). Schemas are Effect `Schema` structs from
`@opencode/schema` — validation is at the typed boundary, and tool definitions are declared in a
separate `packages/schema` package (`schema/src/tool.ts`, `permission.ts`,
`durable-event-manifest.ts`, `event-log.ts`). `[H]` for files, **[M]** for "validation happens in
Schema decode" (I read the schema surface, not every decode).

**Permissions.** `Rule = {action, resource, effect}` (`packages/schema/src/permission.ts:60-66`),
`Ruleset = Rule[]`, wildcard matching, **deny-first** evaluation, and `merge`/`evaluate` as pure
functions (`core/src/permission.ts:86-101`); an agent with no rules gets `{action:"*",resource:"*",
effect:"deny"}` (`:19`); `Unattended` context reference supplies rules that settle requests
"for work nobody is present to answer, such as a schedule's check" (`:52-56`) — headless permission
semantics as a first-class concept. `allows`/`assert`/`ask` separate the three questions (`:102-110`). `[H]`

**Durable background work.** `tool/plugin/schedule.ts:79`: schedule a prompt "once after a delay, at a
time, every interval, on a cron, or gated by an `if`/`until` shell (`{run}`) or Code Mode (`{code}`)
check"; pause/resume keep settings, a one-shot that fell due fires once now (`:113-133`, 242 LOC).
`session/background.ts:54` `outstanding(jobs, store, sessionID)` feeds the
`experimental.idle` hook so plugins can extend a session that has live background work (`llm.ts:176-180`).
`Job` service (569 LOC) + `session/execution/restart.ts` (268) handle reload/restart. `[H]`

**Subagents.** `tool/plugin/subagent.ts:317`: child session with fresh context ("include all relevant
context"), optional `model: "providerID/modelID#variant"` with validation against the model catalog
(`:80-87`), `background: true` returns immediately and notifies on completion, with explicit
"DO NOT sleep, poll, or check progress" instruction (`:44-61`). `[H]`

**Git/persistence.** SQLite (`packages/core/src/database`, drizzle) as the event store;
`snapshot.ts` is a **git-tree based file snapshot system**: `session/revert.ts:32-49` captures a
snapshot id per file, restores selected paths, and diffs `from: original to: next` — rewind without
touching the user's index/worktree; `packages/core/src/git.ts` for repo facts. `[H]` for revert.ts.
**MCP** in `core/src/mcp/{client,oauth,stdio,socket-client,instructions}.ts`. **Skills**:
`core/src/skill.ts` with `DirectorySource | UrlSource | EmbeddedSource` (`:12-18`), SKILL.md
detection + sibling-file staging (`:55-71`), and skills filtered by permission ruleset
(`available(skills, permissions)` `:33`). **Plugins**: `core/src/plugin.ts` (316) +
`PluginHooks` (`plugin/hooks.ts`) with namespaced dotted events (`hooks.trigger("session",
"experimental.idle", …)`), and `plugins.awaitActivation` is called *before every step and retry*
(`llm.ts:75`, `:131`, `:222`, `:292`) so a plugin can hot-swap agents/models/tools/skills mid-turn. `[H]`

**No embeddings, no static index.** Search is `grep.ts`/`glob.ts`/`read.ts` tools;
tree-sitter appears only in `packages/tui/src/parsers-config.ts` (highlighting) and `packages/cli`
deps. `[H]`

---

## 4. Gemini CLI (`google-gemini/gemini-cli`)

**Loop.** `packages/core/src/core/client.ts` (1 357 LOC), `sendMessageStream` (`:945`) →
`processTurn` (`:648`) → `Turn` (`core/turn.ts`, 542) consumes the stream; recursion instead of a
`while`: `boundedTurns = Math.min(turns, MAX_TURNS)` (`:990`) passed down and decremented
(`:1061`), plus `MaxSessionTurns` event when `sessionTurnCount > getMaxSessionTurns()` (`:660-666`).
Termination is a **generator protocol**: `processTurn` returns the final `Turn`, and the caller checks
`turn.pendingCalls`. Loop detection yields `LoopDetected` and recovers via
`_recoverFromLoop` (`:784-791`, `:884`). `[H]`

**Loop detection** (`services/loopDetectionService.ts`): `TOOL_CALL_LOOP_THRESHOLD = 5`,
`CONTENT_LOOP_THRESHOLD = 10`, `MAX_HISTORY_LENGTH = 5000`, `MAX_LLM_CHECK_INTERVAL = 15`,
`LLM_CONFIDENCE_THRESHOLD = 0.9` (`:29-65`) — deterministic hashing of repeated chunks + distance
(`:358-531`) **plus an LLM judge** with a written definition of loop vs. legitimate debugging
progress (`:73-79`). `[H]`

**Hooks** (`packages/core/src/hooks/`, 10 modules: registry, planner, runner, aggregator,
translator, trustedHooks, hookSystem, runtimeHooks) with events including
`BeforeTool`, `AfterTool`, **`BeforeToolSelection`** — a hook that rewrites the advertised tool list,
aggregated with merge semantics (`hookAggregator.ts:101-104`, `:254-256`) — plus `AfterAgent` whose
output can stop, block-and-continue with a reason, and *optionally clear the context*
(`client.ts:1013-1070`). Trusted/builtin hooks are a distinct class (`trustedHooks.ts`). `[H]`

**Tool execution is a 7-state machine** (`scheduler/types.ts:27-33`): `Validating → Scheduled →
AwaitingApproval → Executing → Success | Error | Cancelled`, driven by
`scheduler/scheduler.ts` (1 018) + `state-manager.ts` (602) + `tool-executor.ts` (586), with
`tool-modifier.ts` (107) letting a hook rewrite arguments before execution, and
`confirmation.ts` (349) owning the prompt. `[H]`

**Policy engine** (`packages/core/src/policy/`): `policy-engine.ts` + `config.ts` implementing
**tiered fractional priorities** — default TOML 1.x, extension 2.x, workspace 3.x, user 4.x, admin 5.x,
with `priority + 950/1000` offsets and `ALWAYS_ALLOW_PRIORITY` at workspace tier
(`config.ts:66-89`, `:377-386`), `toml-loader.ts`, `policies/` directory,
`sandboxPolicyManager.ts`, `integrity.ts` (tamper detection), `stable-stringify.ts` (canonical rule
hashing), shell-safety + shell-substitution test suites, and automatic deny rules for excluded tools
and MCP (`config.ts:420`, `:433`). Decisions are `ALLOW | DENY | ASK_USER`
and non-interactive `ASK_USER` **throws** (`scheduler/policy.ts:51-86`). `[H]`

**Context management** (`packages/core/src/context/`): `contextManager.ts` +
`chatCompressionService.ts` + `contextCompressionService.ts` + `toolDistillationService.ts` +
`toolOutputMaskingService.ts` + `memoryContextManager.ts` + `agentHistoryProvider.ts` +
`graph/` + `pipeline/` + `processors/` + `profiles.ts` + `truncation.ts` + `eventBus.ts`.
This is the largest context-engineering codebase of the group (141 507 non-test LOC in core). `[H]`
Local inference exists: `core/localLiteRtLmClient.ts` (LiteRT). Sandbox: `sandbox/{linux,macos,windows}`
(`[M]` — layout only; per-command Seatbelt/bwrap equivalent not read). `[H]` for path existence.

**Agents/subagents** (`packages/core/src/agents/`): `agentLoader.ts`, `agent-scheduler.ts`,
`agent-tool.ts`, `codebase-investigator.ts`, `a2a-client-manager.ts` + `a2a-errors.ts` +
`a2aUtils.ts` + the separate `packages/a2a-server` ⇒ **Gemini CLI is both an A2A client and an A2A
server**; `browser/` agent. MCP: `tools/mcp-client.ts`, `mcp-client-manager.ts`, custom transports
(`mcp-compliance-transport.ts`, `socket-client` style). Skills: `skills/builtin` +
`skillLoader.ts` + `skillManager.ts`, surfaced to the model as a tool (`tools/activate-skill.ts`).
Plan mode is a tool pair (`tools/enter-plan-mode.ts`, `exit-plan-mode.ts`); `complete-task.ts` is a
model-facing **explicit task-finished signal**, and `ask-user.ts` explicit elicitation;
`jit-context.ts` for deferred context injection. `[H]`

---

## 5. OpenHands (All-Hands-AI) — the harness pivot

`All-Hands-AI/OpenHands` HEAD is now **`@openhands/agent-canvas` v1.34.0**, a React-Router/Vite/
Electron UI with **17 `.py` files**: there is no agent engine in this repo any more. `package.json`
deps are UI (`@heroui/react`, `monaco-editor`, `@xterm/xterm`, `zustand`) plus
`@openhands/typescript-client@1.59.0` and `@openhands/extensions@0.36.0`. `[H]`

The engine moved to `All-Hands-AI/agent-sdk`: `openhands-sdk` + `openhands-tools` +
`openhands-workspace` + `openhands-agent-server` + `clients` (173 451 py LOC non-test).
`openhands-sdk/openhands/sdk/` = `agent, automation, context, conversation, critic, event, git,
hooks, mcp, marketplace, plugin, profiles, security, settings, skills, subagent, secret, tool,
testing, workspace`. `[H]`

Two conversation implementations: `conversation/impl/local_conversation.py` (3 262 LOC, in-process
`Agent.step` loop with `max_iteration_per_run=500`, `stuck_detection=True`, configurable thresholds,
`StuckDetector` that **nudges once on an action-error streak before declaring stuck**
(`:787-795`), FIFO locks + `resource_lock_manager.py` for concurrent access) and
`remote_conversation.py` (2 026 LOC, WebSocket to the agent-server with reconnect/replay,
`max_iteration_per_run` negotiated from the server at `:1104`, `ERROR`/`STUCK` terminal states
`:1413`). `[H]`

**The decisive fact:** `agent/acp_agent.py` is **4 782 lines — ~3× the size of the native
`agent/agent.py` (1 700)**. Supporting modules `acp_contracts.py` 424, `acp_models.py` 138,
`acp_model_discovery.py` 170, `acp_skills.py`, `acp_file_credentials.py` 539, `acp_tracing.py` 289.
Agent Canvas documents the provider list as Claude Code / Codex / Gemini CLI / Pi / OpenCode, each
launched as a subprocess over ACP JSON-RPC stdio (`docs/ACP_AGENTS.md`: "the Agent Server spawns the
agent's own CLI as a subprocess… The external agent manages its own LLM, tools, and execution";
commands are literally `npx -y @agentclientprotocol/claude-agent-acp`,
`npx -y @agentclientprotocol/codex-acp`, `npx -y @google/gemini-cli --acp`, `npx -y opencode-ai acp`).
Credential reuse is delegated to the guest CLI's own login files (`~/.codex/auth.json`,
`~/.claude/.credentials.json`, `~/.gemini/oauth_creds.json`) with login taking priority over API key.
`agent/parallel_executor.py` (357) + `response_dispatch.py` (400) + `critic_mixin.py` (138) +
`stream_context.py` (294) are the native path; server-side hooks load via
`POST /api/hooks` (HEAD commit message). `[H]` for the files/lines, `[M]` for the strategic reading.

`security/` includes `shell_parser.py` and `defense_in_depth/shell_semantics.py` — tree-sitter-based
command semantic analysis for the sandbox/approval decision, i.e. the same trick codex uses. `[H]`

---

## 6. Aider (`Aider-AI/aider`) — still the context-ranking reference, but dormant

**Loop.** `aider/coders/base_coder.py:876` `run` → `:924` `run_one`: `while message:` — send, parse
the reply, apply edits, then feed a *reflection* message back ("… Anything else you need to do?");
`if self.num_reflections >= self.max_reflections: warn("Only 3 reflections allowed, stopping")`
(`:939-943`, defaults at `:100-101`). Termination = the model produces no tool-update blocks.
There is no tool schema, no state machine, no async — 13 359 lines total. `[H]`

**Repo map (the reference idea).** `aider/repomap.py` (867 lines): tree-sitter `Query`/`QueryCursor`
captures of definitions and references per language (`:266-302`, query `.scm` files under
`queries/tree-sitter-language-pack/` + `tree-sitter-languages/`, with explicit version-skew handling
for 0.23 vs 0.24 bindings `:267-276` and a Ruby def/ref bug workaround `:473`), fallback to
Pygments tokenization for unsupported languages (`:353-360`). It builds a `nx.MultiDiGraph` of
file/definition/symbol nodes with weighted edges (`:470`) and runs **PageRank**
(`nx.pagerank(G, weight="weight", **pers_args)` `:525`, personalization for the current chat files,
fallback to plain PageRank on numerical failure `:529`), then **distributes each node's rank across
its outgoing edges** to rank individual definitions (`:533-545`), emits ranked tags, then fills
remaining budget with whole files by node rank (`:562-572`). Token budget is enforced by **binary
search over the split point**, keeping the largest tree under `max_map_tokens` (`:669-691`), and
definition tags are cached on disk in a pickle-based `tags_cache` (`:217-226`) with corruption
recovery (`:177-192`). Map size defaults to `map_tokens=1024` and scales with
`map_mul_no_files` when a context window is known (`:49`, `:120-132`). `[H]`

**Git.** `aider/repo.py:131` `commit(...)` auto-generates the message from the diff
(`get_commit_message(diffs, context, user_language)` `:216`), adds
`Co-authored-by: aider (<model>) <aider@aider.chat>` (`:168`), with configurable attribution as
author vs committer (`:222-230`). No worktrees, no sandbox — writes are applied directly and reverted
via git. Linting is post-edit (`aider/linter.py`). `[H]`

**Status.** Last commit `5dc9490` **2026-05-22** (a PR merge adding Opus 4.5 to a model list);
1 934 open issues+PRs with 49 848 stars. Treat aider as an **algorithm reference, not an engineering
reference**. `[H]` for commit date, REST numbers 2026-10-31.

---

## 7. Claude Code — PUBLIC EVIDENCE ONLY

Everything in this section is public-repo evidence (`anthropics/claude-code` HEAD `80c6959`,
2026-10-31) or explicitly marked **ASSUMPTION**. The engine is closed source, LICENSE.md is
"© Anthropic PBC. All rights reserved" — I make **no claim** about private implementation.

**Public evidence 1 — the engine is a hook pipeline, and Anthropic's own built-ins are plugins in
it.** `mods/README.md`: "A mod is a Claude Code plugin whose behaviour lives in a hooks module: one
`register(on, options)` entry that hooks the engine's events as functions `($, e, next)`… **These
four ship inside Claude Code; this folder is their source, published as it is built into the binary.**"
Four mods, all real TS, all with `.claude-plugin/plugin.json` + `hooks/hooks.json`
(`mods/diff/hooks/hooks.json`: `{"description": …, "modules": ["./register.ts"]}`), runnable from
source with `claude --plugin-dir mods/diff` and testable with `claude plugin test mods/diff`.
Their size tells you what the engine exposes: `mods/diff` 810 ts files, `mods/telemetry` 272,
`mods/sec-default` 193, `mods/agents-md` 72.
`mods/sec-default` is a **policy-isolation mod**: keeps an org's classic hooks, prompt content,
managed settings, tool policy, permission rules and pinned environment "out of reach of the plugins a
person installs; adds no policy of its own", seated **outermost** and latched for the session —
i.e. plugin-vs-managed-policy confinement is expressed *through the plugin system itself*. `[H]`
`mods/agents-md` shows instruction-file precedence as a mod option
(`claude-md-or-agents-md` default | `claude-md-and-agents-md` | `managed-only` | `claude-md`),
including that nested `AGENTS.md` are loaded *on a `Read`*. `[H]`

**Public evidence 2 — a 13 440-line published API contract.** `mods/types/claude-code.d.ts`
declares module `claude-code` with `Register`, `On`, `EngineInterface extends CoreEngineInterface`,
`CoreEventOf = EngineEventOf & ClassicEventOf & OpEventOf` (`:3053-3059`) ⇒ three event families
(new engine hooks, backward-compatible classic hooks, operation hooks). Observed event names:
`engine.create`, `turn.start`, `turn.step`, `turn.complete`, `turn.abort`, `prompt.context`,
`prompt.section`, `prompt.attachment`, `tool.register`, `tool.describe`, `tool.list`, `tool.check`,
`tool.call`, `agent.register`, `agent.spawn`, `command.register`, `command.run`, `skill.prompt`,
`settings.read`, `store.get/set/keys/delete`, `ui.render/resolve/open/close/focus/input/press/panes/
message/log/toast/status/notice/select/scroll/blit/invalidate`. `engine.create` is described as a
**fold** ("added the noun in the `engine.create` fold where the engine has none", README) — plugins
compose the engine's own namespaces. Classic hook inputs are typed too
(`CwdChangedHookInput`, `DirectoryAddedHookInput` with `source: 'slash_command' | 'register_repo_root'`
`:3061-3077`). Subagent semantics are documented in the types: a plugin's spawn "always runs in the
background" and resolves `{agentId, resolvedModel}` (`:98-111`); `AgentInfo` carries
`id/description/type/status/parentId/spawnedBy` with statuses
`running|completed|failed|killed` (`:117-141`); events carry "which model loop this happened in"
(`agentId`, absent on main loop) (`:157-170`); agent types include built-in `Explore`, `Plan` and a
`teammate` kind (`:128-131`, `:186-196`). `ui.*` types declare a **JSX element table**
(`ElementChildren`, `$.ui.resolve(e)` → constructors → `h(Box, {gap:1}, children)`, `:3080-3098`) with
one mod rendered on `terminal | desktop | vscode | mobile` from the same source
(`mods/README`/testing section). `[H]` — this file is published by Anthropic.

**Public evidence 3 — feature surface from CHANGELOG.md** (434 released versions, newest 2.1.316):
`CLAUDE_CODE_SUBAGENT_EFFORT`, background jobs/`CLAUDE_CODE_DISABLE_BG_TASKS_REPORT_RUNNING`,
`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`, `CLAUDE_CODE_DISABLE_CRON` (⇒ scheduled tasks),
`CLAUDE_CODE_DISABLE_BUNDLED_SKILLS` (⇒ bundled skills), `CLAUDE_CODE_AUTO_MODE_MODEL`/`_SERVER`
(⇒ server-side "auto mode" safety verdicts that can stop a turn),
`managed.policy_mode: layered` ("every policy that matches applies, each merged over the ones below
it, not only the first"), Claude apps gateway with `availableModels`/`deniedModels`/`pooled_budget`
and Bedrock `guardrail.input_tagging`, `--restricted` sessions + `Read` allow-rule escapes
(fixed), file checkpointing, plan restore on resume (`/clear`, `/resume` interaction bug),
plugin MCP `headersHelper` credential scripts + `CLAUDE_PLUGIN_ROOT`, SDK
`register_repo_root` control request + `CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD`,
`claude remote-control` and cloud sessions/scheduled tasks, self-hosted runner.
**Everything in this paragraph is a public statement of behaviour, not implementation.** `[H]` that the
text says it; no implementation claim.

**ASSUMPTIONs (labelled, unverified).** (a) The model loop is a single tool-use loop with a
completion message as the exit condition, like every other agent here — **ASSUMPTION**.
(b) Auto-compact is a summarization of history plus re-injection of instructions/files, given
`CLAUDE_CODE_AUTO_COMPACT_WINDOW` and the "assume a 200K context window" note — **ASSUMPTION** for
mechanism, public for the flag. (c) Hooks run as out-of-process commands (classic hooks) *and* as
in-process JS modules (mods) — the two families in `CoreEventOf` support this reading, but the
in-process isolation model is **ASSUMPTION**. (d) Sandboxing/`--restricted` and the "sandbox"
mentioned at `mods/sec-default/README.md:197` are the same boundary — **ASSUMPTION**.
