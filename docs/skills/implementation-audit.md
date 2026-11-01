# Skills Subsystem — Pre-Implementation Audit

Read-only audit of `/home/himanshu/Desktop/VIKRAM-CLI` at the time of writing.
Every claim below is anchored to `file:line`. Nothing here was executed except `grep`/`read`;
the test count is **counted, not run**. Labelled deviations are marked _observed_ vs _recommended_.

Repo state: git repo on branch `master` with **zero commits** (`git log` → "does not have any
commits yet"); all files untracked. No `docs/skills/` existed before this file.

---

## 1. Workspace layout

`Cargo.toml:3-11` — 7 members, `resolver = "3"`, `edition = "2024"`, `rust-version = "1.86"`
(`Cargo.toml:13-18`). `vikram-cli` is a member but is **absent** from `[workspace.dependencies]`
(`Cargo.toml:21-26` lists only the other six) — CLI-internal crates cannot be a dependency of
another crate.

| Crate | Role | Internal deps (Cargo.toml lines) | External deps |
|---|---|---|---|
| `vikram-contracts` | ids, errors, journal, message, policy vocab, trust, versioning | — | serde, serde_json, thiserror (`crates/vikram-contracts/Cargo.toml:9-12`) |
| `vikram-kernel` | Machine, effects, grants, run/clock/token-meter | contracts | serde, serde_json; dev: policy (`vikram-kernel/Cargo.toml:9-15`) |
| `vikram-policy` | deny-first evaluator + mandatory floor | contracts | serde, serde_json (`vikram-policy/Cargo.toml:9-12`) |
| `vikram-store` | SQLite journal + metadata | contracts | serde, serde_json, thiserror, rusqlite; dev: tempfile (`vikram-store/Cargo.toml:9-17`) |
| `vikram-runtime` | sessions, agents, tools, MCP, plugins, skills, turn loop | contracts, kernel, policy, store | rusqlite, serde, serde_json, thiserror, tokio, futures-util, reqwest(+blocking), scraper 0.27, percent-encoding 2.3 (`vikram-runtime/Cargo.toml:9-25`) |
| `vikram-tui` | view model, widgets, input, layout | contracts, kernel, runtime (**not** store, **not** policy) | ratatui 0.30, crossterm 0.29, unicode-width; dev: insta, tempfile (`vikram-tui/Cargo.toml:9-19`) |
| `vikram-cli` | `vikram` binary: TUI, headless chat, operator commands | contracts, runtime, tui, store (**not** policy) | clap 4.6, tokio, futures-util, crossterm, ratatui, serde_json; dev: tempfile (`vikram-cli/Cargo.toml:13-27`) |

Dependency direction (leaf → host): `contracts` ← `kernel`, `policy`, `store` ← `runtime` ← `tui`
→ `cli`. `policy` is only linked by `runtime`; `kernel` is linked by `runtime` and `tui`.

Available (workspace-pinned, `Cargo.toml:28-54`): `serde 1.0 (derive)`, `serde_json 1.0`,
`thiserror 2.0`, `rusqlite 0.41 (bundled)`, `tokio 1.54` (`rt-multi-thread, macros, sync, time,
process, io-util, signal`), `futures-util 0.3`, `reqwest 0.13` (rustls/json/stream/http2),
`ratatui 0.30`, `crossterm 0.29`, `ratatui-textarea 0.9`, `clap 4.6 (derive, env)`,
`unicode-width 0.2`, `insta 1.49 (yaml)`, `tempfile 3`.

**Not present** (checked `Cargo.lock`): `serde_yaml`, `yaml-rust`, `toml`, `gray_matter`,
`pulldown-cmark`, `glob`, `notify`, `directories`/`dirs`. `walkdir` appears only as a transitive
package (`Cargo.lock:2399`), not as a direct dependency. No markdown parser exists in the
workspace; `vikram-tui/src/widgets/markdown.rs` is hand-rolled rendering.

---

## 2. Existing skills code

### 2.1 `crates/vikram-runtime/src/skills.rs` (230 lines, 7 tests)

Module doc states the design intent explicitly: *"A skill is a directory containing a `SKILL.md`
file. Discovery is read-only … nothing is executed."* (`skills.rs:1-4`).

| Item | Lines | Behaviour |
|---|---|---|
| `struct Skill` | `10-23` | `name`, `description`, `instructions`, `path`, `loaded`; derives `Clone, Debug, PartialEq, Eq` |
| `name` | `13-14` | **containing directory name**, not frontmatter (`50-53`) |
| `description` | `15-16` | **first line** of `SKILL.md`, trimmed, `"No description"` if blank (`54-60`) |
| `instructions` | `17-18` | full file body, read eagerly at scan time (`47`, `64`) |
| `loaded` | `21-22` | always `true` when pushed (`66`); unreadable files are skipped (`47-49`) |
| `struct SkillRegistry` | `25-29` | `Vec<Skill>` inside; `Clone, Debug, Default`; no interior mutability, no caching layer |
| `scan(dir)` | `33-71` | `read_dir`, dirs only, requires `SKILL.md`, silently skips read errors, sorts by name |
| `default_dir()` | `74-76` | `vikram_home().join("skills")` → `$VIKRAM_HOME/skills` |
| `skills_dir()`, `default_skills_dir()` | `78-84` | aliases of `default_dir()` |
| `from_default_dir()` | `86-88` | scans default dir only |
| `for_workspace(ws)` | `91-100` | default dir, then `<ws>/.vikram/skills`, then `<ws>/skills`, merged |
| `merge(other)` | `102-109` | **first wins by name**; later duplicates dropped; re-sorts |
| `list()`, `skills()` | `111-117` | clone / slice |
| `get`, `find`, `load`, `get_skill`, `load_skill` | `119-145` | five names for the same lookup/clone |
| `count()`, `scan_dir()` | `131-137` | aliases |

Behavioural facts that constrain a redesign:

1. **No frontmatter.** Description is literally `content.lines().next()` (`skills.rs:54-58`). A
   `---` YAML block would become the description and the whole block stays inside `instructions`.
2. **User dir is scanned before project dirs**, and `merge` drops duplicates (`91-109`). A project
   skill can **never** shadow a same-named user skill.
3. **Only `SKILL.md` is read** (`43-49`). `references/`, `scripts/`, auxiliary files are invisible;
   nothing recurses.
4. **No path confinement.** `scan` reads whatever directory it is handed (`35-38`); it does not use
   `config::resolve_within` (`config.rs:177-199`), unlike every filesystem tool
   (`tools.rs:146-153`).
5. **Full body held in memory** for every skill (`64`), and `list()` clones all of them (`111-113`).

### 2.2 Every reference to "skill" in `crates/` (110 grep hits, deduplicated)

| Location | What it is |
|---|---|
| `crates/vikram-runtime/src/lib.rs:20,36` | module + `pub use skills::{Skill, SkillRegistry}` |
| `crates/vikram-runtime/src/runtime.rs:29,62` | `RuntimeInner.skills: SkillRegistry` |
| `crates/vikram-runtime/src/runtime.rs:344` | `SkillRegistry::for_workspace(&workspace_root)` at startup — the only registry instance |
| `crates/vikram-runtime/src/runtime.rs:782-792` | `RuntimeHandle::skill_list/skill_get/skill_count` — **zero callers** (verified by grep outside these lines) |
| `crates/vikram-runtime/src/tools.rs:16,76-77` | registers `SkillList`, `SkillLoad` |
| `crates/vikram-runtime/src/tools.rs:1284-1371` | the two tool impls (see 3.4) |
| `crates/vikram-runtime/src/types.rs:220,236` | `ActivityKind::Skill` + label `"Skill"` |

Dead surface, verified:

- `RuntimeHandle::skill_*` has no caller in `crates/` (grep).
- `ActivityEvent` is **never constructed** anywhere (`grep "ActivityEvent {"` → only the struct
  definition at `types.rs:245`). `RuntimeEvent::Activity` is never emitted; the TUI ignores it
  (`app.rs:607`). So `ActivityKind::Skill` is currently unreachable.

**Duplication hazard:** both tools build a **fresh** `SkillRegistry` per call
(`tools.rs:1309`, `tools.rs:1359`) instead of reading `RuntimeInner.skills` (`runtime.rs:62`).
Disk is re-scanned per model call, and any caching/merge policy added to the runtime instance is
bypassed. Note tools are stateless unit structs (`struct SkillList;` at `tools.rs:1284`), so
sharing the registry requires giving them a field and registering instances rather than `Box::new`
singletons (`tools.rs:76-77`).

---

## 3. Tool architecture

### 3.1 Core types (`crates/vikram-runtime/src/tools.rs`)

```
MAX_TOOL_OUTPUT: usize = 20_000                    tools.rs:22
ToolOutput { content: String, ok: bool }           tools.rs:24-28
ToolContext { workspace_root: PathBuf }            tools.rs:30-33
pub use crate::provider::ToolSpec;                 tools.rs:35
trait Tool: Send + Sync                            tools.rs:37-42
    fn spec(&self) -> ToolSpec
    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError>
    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError>
ToolRegistry { tools: Mutex<BTreeMap<String, Arc<dyn Tool>>> }   tools.rs:44-47
```

`ToolSpec` lives in `provider.rs:12-17`: `{ name: String, description: String, schema: serde_json::Value }`.

### 3.2 Registry API

| Method | Lines | Notes |
|---|---|---|
| `with_builtins()` | `56-82` | registers 23 tools by `Box::new(Singleton)`; comment at `54-55`: *"Registering a tool is the only way to make it callable; there is no dynamic loading from model output"* |
| `register(Box<dyn Tool>)` | `84-89` | key = `tool.spec().name`; later registration replaces earlier |
| `get(&str) -> Option<Arc<dyn Tool>>` | `92-94` | cloneable handle, moved into `spawn_blocking` |
| `specs() -> Vec<ToolSpec>` | `96-103` | sent to the provider every turn (`turn.rs:116`) |
| `names()`, `all()` | `105-122` | used by `/tools` (`terminal.rs:311`) and MCP registration (`runtime.rs:426`) |

Registration is **global and compile-time**: the model's `tools` array is `inner.tools.specs()`
(`turn.rs:116`). Nothing can be enabled per-skill or per-session today.

### 3.3 Argument validation and errors

Validation is per-tool and hand-rolled. The only helpers:

- `arg_str(args, key)` → `VikramError::ProviderRejected("missing string argument \`{key}\`")`
  (`tools.rs:139-144`).
- `args.get(k).and_then(Value::as_str).unwrap_or(default)` for optional args (e.g. `tools.rs:212`,
  `223`, `477`).
- `canonical_resource(root, raw)` → `resolve_within` or `VikramError::PolicyDenied`
  (`tools.rs:146-153`; `config.rs:177-199`).
- `truncated(String)` appends `"\n… [output truncated]"` past 20 000 bytes (`tools.rs:125-137`).

There is **no JSON-Schema validation** of arguments against `ToolSpec.schema` anywhere. A malformed
call reaches the tool and fails inside `run`.

Error type: `vikram_contracts::VikramError` (`contracts/src/error.rs:6-49`) — variants used by tools
are `ProviderRejected`, `PolicyDenied`, `EffectFailed`, `Storage`.
`VikramError: Clone + PartialEq + Eq` (`error.rs:6`).

### 3.4 Built-in tool inventory

| Verb | Struct | spec | permission | ActionKind | SideEffect | Resource |
|---|---|---|---|---|---|---|
| `fs.read` | ReadFile | 160 | 170 | Read | None | path |
| `fs.list` | ListDir | 202 | 211 | Read | None | path (default `"."`) |
| `fs.write` | WriteFile | 257 | 270 | Write | LocalMutation | path |
| `fs.edit` | EditFile | 311 | 325 | Write | LocalMutation | path |
| `fs.delete` | DeleteFile | 395 | 407 | Write | Destructive | path |
| `search.text` | SearchText | 451 | 464 | Read | None | query |
| `shell.run` | RunShell | 617 | 630 | Execute | LocalMutation | first word of command |
| `git` | Git | 730 | 747-749 | Read / Write (commit) | None / LocalMutation | subcommand |
| `build.run` | BuildRun | 794 | 805 | Execute | LocalMutation | (fixed) |
| `test.run` | TestRun | 842 | 853 | Execute | LocalMutation | (fixed) |
| `memory.save` | MemorySave | 884 | 901 | Write | LocalMutation | key |
| `memory.search` | MemorySearch | 933 | 949 | Read | None | query |
| `memory.list` | MemoryList | 989 | 1006 | Read | None | scope |
| `memory.delete` | MemoryDelete | 1045 | 1061 | Write | Destructive | key |
| `web.search` | WebSearch | 1096 | 1112 | Network | None | query (DDG scrape) |
| **`skill.list`** | **SkillList** | **1289** | **1300** | **Read** | **None** | **`"skills"`** |
| **`skill.load`** | **SkillLoad** | **1334** | **1349** | **Read** | **None** | **skill name** |
| `team.create` | TeamCreate | 1384 | 1397 | Write | LocalMutation | name |
| `team.list` | TeamList | 1426 | 1437 | Read | None | `"teams"` |
| `team.delegate` | TeamDelegate | 1479 | 1496 | Delegate | LocalMutation | team |
| `diagnostics.collect` | Diagnostics | 1527 | 1540 | Read | None | `"diagnostics"` |
| `process` | Process | 1642 | 1657 | Execute | LocalMutation | `"process"` |
| `task` | Task | 1774 | 1793 | Write | LocalMutation | `"tasks"` (JSON at `$VIKRAM_HOME/tasks.json`, `tools.rs:1745-1747`) |
| `mcp.<server>.<tool>` | McpTool | 1903 (`mcp_tool_name` 1374-1376) | 1915 | Execute | LocalMutation | `<server>.<tool>` |

`skill.list` returns `"{name}: {description}"` joined by newline, or `"no skills available"`
(`tools.rs:1308-1326`). `skill.load` returns `skill.instructions` verbatim, or
`"skill not found: {name}"` with `ok: false` (`tools.rs:1357-1370`). Both re-scan the disk (see 2.2).

### 3.5 Dispatch in `run_tool` (`crates/vikram-runtime/src/turn.rs:376-542`)

| Step | Lines |
|---|---|
| Registry lookup, `"not registered"` + `ToolFinished{ok:false}` on miss | `383-392` |
| `tool.permission(&call.arguments)`; error → `ToolFinished` + returned as content | `394-406` |
| `inner.remembered_allow(&request)` short-circuits to `Decision::Allow` | `408-410` |
| otherwise `inner.policy().evaluate(&request)` | `411` |
| `Deny` → journal `RecordKind::Policy`, `ToolFinished`, `("permission denied: …", false)` | `415-429` |
| `RequireApproval` → approval flow (see 4.4) | `430-480` |
| `Allow` → journal `Policy{decision:"allow"}` then `ToolCall` | `484-493` |
| `summarize_arguments` (first 3 keys, 80 chars) → `ToolStarted` | `495-501`, helper `558-574` |
| `session.set_turn(Tooling)`, agent state `Running` | `502-506` |
| `inner.tool_context()`; `tokio::task::spawn_blocking(|| handle.run(&args, &ctx))` | `508-511` |
| Result mapping: `Ok(Ok(out))`, `Ok(Err(err))` → `err.to_string()`, `Err(join)` → `"tool task failed"` | `513-517` |
| Journal `ToolResult` with content truncated to 64 000 | `519-528` |
| `ToolFinished{ok}` + summary (`"failed · …"`) | `529-539` |

Consequence: **tools are synchronous and run on a blocking thread**. An async skill fetch/install
cannot use `async fn run` without changing the trait (`tools.rs:37-42`).

---

## 4. Permission system

### 4.1 Vocabulary — `crates/vikram-contracts/src/policy.rs`

| Type | Lines | Values / shape |
|---|---|---|
| `ActionKind` | `8-18` | `Read, Write, Execute, Network, Delegate, Spend, Admin` (7); `snake_case` wire |
| `SideEffect` | `21-28` | `None, LocalMutation, ExternalChange, Destructive` |
| `PermissionRequest` | `30-40` | `action, verb, resource, side_effect, originating_trust` |
| `PermissionRequest::new` | `42-58` | positional constructor used by every tool |
| `RuleOutcome` | `62-68` | `Allow, Deny(String), Ask(String)` (tag `outcome`) |
| `Decision` | `70-82` | `Allow`, `Deny{reason}`, `RequireApproval{reason, amendment: Option<String>}` (tag `decision`) |
| `Decision::is_allowed` | `84-88` | only `Allow` counts |
| `trait PermissionPolicy` | `92-101` | `contract()`, `evaluate_at_depth(req, depth)`, default `evaluate` = depth 0 |

### 4.2 Evaluator — `crates/vikram-policy/src/lib.rs`

| Item | Lines | Rules |
|---|---|---|
| `ResourcePattern` | `19-56` | `Any` (`*`), `Exact(str)`, `Subtree("prefix/**")`; `matches` rejects `/tmpx` for `/tmp/**` (`36-47`) |
| `Rule` + builders | `58-110` | `allow/deny/ask(action, pattern)`, `.with_verb_prefix(prefix)` |
| `Rule::applies_to` | `99-109` | action filter → verb prefix → resource match |
| `MandatoryFloor` | `113-131` | `workspace_root: Option<String>`, `forbidden_from_untrusted = ["grant.", "policy.", "self."]` (`127`), `max_delegation_depth = 3` (`128`) |
| `MandatoryFloor::veto` | `135-171` | (a) untrusted origin + forbidden verb prefix → Deny; (b) `Delegate` at `depth >= 3` → Deny; (c) `side_effect != None` + `action == Write` + resource outside `workspace_root` → Deny |
| `Policy::evaluate_at_depth` | `209-247` | floor first (`210-212`); single pass, **first Deny returns** (`222-227`); `Ask` beats `Allow` (`237-242`); **no matching rule = Deny** (`244-246`) |
| `amendment_for` | `250-264` | emits `"allow {action} {verb} resource={scope}/**"` for the approval prompt |
| `impl Versioned/PermissionPolicy` | `267-281` | contract id `POLICY_EVALUATOR` 1.0 |

Tests pin the invariants: deny-outranks-allow (`318-332`), absent-rule-is-deny (`335-344`),
ask-outranks-allow (`347-361`), floor cannot be widened (`364-382`), depth cut-off (`401-418`),
outside-root writes refused including prefix-lookalikes (`421-447`).

### 4.3 Runtime policy instance — `crates/vikram-runtime/src/runtime.rs:109-133`

Recomputed on **every** call (`pub fn policy(&self) -> Policy`), rules:

```
allow Read  *                                  114
allow Write Subtree(workspace_root)            115-119
ask   Execute *   "commands run with your authority"   120-124
ask   Network *   "network access leaves this machine"  125-129
allow Delegate *                                130
floor.workspace_root = Some(workspace_root)     110-113
```

Consequences worth designing around:

- `Admin` and `Spend` have **no rule** → always `Deny` (`policy/lib.rs:244-246`).
- `skill.list` / `skill.load` are `Read` → auto-allowed, no prompt.
- **A tool that writes into `$VIKRAM_HOME` (e.g. skill install) is vetoed by the floor** before
  rules are consulted (`policy/lib.rs:156-168` with `runtime.rs:111`), because `$VIKRAM_HOME` sits
  outside `workspace_root` and `SideEffect::LocalMutation != None`. _Observed._ Install must
  therefore be host-side (CLI command / explicit runtime method, like `RuntimeHandle::create_session`)
  or must write under the workspace root.

### 4.4 Approval flow

| Step | Evidence |
|---|---|
| `ApprovalId::generate()`, `ApprovalPrompt` built | `turn.rs:431-439` |
| `oneshot::channel()`, `PendingApproval{prompt, respond}` inserted into `inner.approvals` | `turn.rs:440-447`, struct at `runtime.rs:37-40` |
| `session.set_turn(WaitingApproval)`, agent `Waiting`, `ApprovalRequested`, `ToolStarted("awaiting approval")` | `turn.rs:448-459` |
| `wait_for_approval(rx, cancel)` — `tokio::select!` on oneshot vs 100 ms cancel poll | `turn.rs:461`, `544-556` |
| Cancelled → `"cancelled while awaiting approval"`; denied → `"permission denied by operator"` | `turn.rs:462-474` |
| `answer_approval(id, allow, persist)` removes pending, optionally `remember`, sends bool, emits `ApprovalResolved` | `runtime.rs:634-662` |
| `remember` stores `RememberedAllow{action, scope: Subtree(parent dir of resource)}` **in memory only** | `runtime.rs:143-159`, struct `71-75` |
| `remembered_allow` matches `action == req.action && scope.matches(req.resource)` | `runtime.rs:135-141` |

Two _observed_ defects in the persist path, relevant if skills add "always allow":

1. `answer_approval` hardcodes `ActionKind::Write` for the remembered request
   (`runtime.rs:647-651`), but `remembered_allow` requires the action to match (`runtime.rs:140`).
   Persisting an `Execute` (`shell.run`) or `Network` (`web.search`) approval therefore never
   short-circuits the next identical call.
2. The remembered set is never persisted (`Vec<RememberedAllow>` at `runtime.rs:64`, no store
   write); a restart loses it.

---

## 5. Configuration

### 5.1 `crates/vikram-runtime/src/config.rs`

| Item | Lines | Detail |
|---|---|---|
| `struct Config` | `11-34` | `base_url`, `model`, `api_key_env`, `context_window: Option<u64>`, `workspace_root: Option<PathBuf>`, `max_steps: u32`, `max_output_tokens: Option<u32>`, `ascii_fallback: bool`, `thinking: bool` |
| serde | `12` | `#[serde(default)]` on the struct + `thinking` redundantly (`32`) → adding fields is backward compatible **provided** `Default` is updated |
| `Default` | `36-50` | `base_url = https://api.openai.com/v1`, `api_key_env = OPENAI_API_KEY`, `max_steps = 24` |
| `load()` | `54-67` | missing file → `Ok(default)`; parse error → `RuntimeError::Config{path, reason}` |
| `save()` | `69-82` | `create_dir_all(parent)` then pretty JSON; overwrites wholesale |
| `api_key()` | `85-89` | read from env **at call time**, never persisted, empty filtered out |
| `workspace_root()` | `91-96` | config value else `current_dir` else `"."` |
| `is_model_configured()` | `99-101` | model non-empty **and** key present |
| `vikram_home()` | `105-116` | `$VIKRAM_HOME` (non-empty) else `$HOME`/`$USERPROFILE` + `/.vikram` else `"."` |
| `config_path()` | `118-120` | `$VIKRAM_HOME/config.json` |
| `database_path()` | `122-124` | `$VIKRAM_HOME/vikram.db` |
| `ensure_home()` | `127-139` | `create_dir_all` + `chmod 0700` on Unix |
| `mcp_config_path()` | `141-143` | `$VIKRAM_HOME/mcp.json` |
| `plugins_dir()` | `145-147` | `$VIKRAM_HOME/plugins` |
| `McpServerConfig` | `150-158` | `name`, `command`, `args` (default), `env: BTreeMap` (default) |
| `McpConfig` | `160-174` | `servers: Vec<_>`; `load()` is infallible — any read/parse error yields `default()` (`167-173`); **no `save()`** |
| `resolve_within(root, candidate)` | `177-199` | lexical normalisation, `ParentDir` pops, rejects anything not `starts_with(root)` |

Env vars actually read anywhere in `crates/`: `VIKRAM_HOME`, `HOME`, `USERPROFILE`
(`config.rs:106-112`), the dynamic `api_key_env` (`config.rs:86`), `COLUMNS` and `TERM`
(`doctor.rs:77-78`). Nothing else.

### 5.2 Patterns worth copying for a skills config

| Pattern | Evidence | Why it fits skills |
|---|---|---|
| Separate JSON file under the home, hand-edited, `load()` never fails | `McpConfig` `config.rs:160-174`, path `141-143` | `$VIKRAM_HOME/skills.json` (enable flags, extra roots) without touching `Config`'s save path |
| Directory scan that records per-entry `error` and distinguishes **installed vs loaded** | `PluginRegistry::scan` `plugins.rs:34-99`, `installed()/loaded()` `105-111`, manifest `25-31` | exactly the "discovered ≠ usable" distinction a skill list needs |
| Missing dir = empty registry, not an error | `plugins.rs:36-38`, `skills.rs:35-37` | same for `$VIKRAM_HOME/skills` |
| Counters surfaced via `Metrics` + a dedicated event | `plugins.rs:105-111` → `runtime.rs:442-446` → `RuntimeEvent::PluginsChanged` `types.rs:328-331` | cheapest way to add "Skills N" to the status bar |
| CLI prints a plain column table with `format!` widths | `main.rs:280-283` | `vikram skills` |

---

## 6. Persistence — `crates/vikram-store/src/lib.rs`

| Item | Lines |
|---|---|
| `StoreError` (`Sqlite`, `Io`, `Json`) | `17-25` |
| `SessionRow` / `AgentRow` / `TeamRow` | `29-38` / `40-48` / `50-57` |
| `Store { conn: Arc<Mutex<Connection>> }` + manual `Debug` | `61-70` |
| `open(path)` (creates parent dirs) | `73-79` |
| `open_in_memory()` | `81-83` |
| `from_connection`: `journal_mode=WAL`, `foreign_keys=ON`, `synchronous=NORMAL`, `migrate()` | `85-93` |
| `journal()` → `SqliteJournal` sharing the same connection | `99-103` |
| sessions: `create_session`, `list_sessions`, `rename_session`, `set_session_model`, `set_session_agent`, `touch_session`, `delete_session` (cascades journal in one tx) | `107-217` |
| agents: `save_agent` (upsert), `list_agents`, `delete_agent` | `221-264` |
| teams: `create_team`, `list_teams`, `rename_team`, `set_team_session`, `delete_team`, `add_team_member`, `remove_team_member`, `list_team_members`, `remove_agent_from_teams` | `268-388` |
| `replay_messages(session)` — journal rows ordered by `seq` | `394-419` |
| `session_count()` | `421-425` |
| `kind_from_str` / `kind_to_str` | `428-455` |
| `migrate(conn)` — `execute_batch` of `CREATE TABLE IF NOT EXISTS` | `457-508` |
| `SqliteJournal`: `append` (tx + effect dedupe → `Appended`), `read_after`, `last_seq` | `511-620` |

Schema (`migrate`, `457-508`):

| Table | Columns | Notes |
|---|---|---|
| `sessions` | `id PK, title, cwd, model, agent_id, created_ms, updated_ms` | `460-468` |
| `agents` | `id PK, name, role, description, icon, created_ms` | `470-477` |
| `teams` | `id PK, name, session_id, created_ms, updated_ms` | `479-485` |
| `team_members` | `team, agent`, PK(team, agent) | `487-491` |
| `journal` | `session, seq, ts_ms, kind, effect, payload`, PK(session, seq) | `493-501` |
| index | `journal_effect` unique on (session, effect) where effect not null | `503-504` |

**Migrations:** there is no `schema_version` table, no `ALTER`, no ordered migration list.
`migrate` is idempotent DDL only (`457-508`). Adding a **new table** is safe (append
`CREATE TABLE IF NOT EXISTS`); adding a **column** to an existing table is not supported by this
pattern and would need a real migration step. `memory.rs:203-220` repeats the same
`CREATE TABLE IF NOT EXISTS memory(...)` idiom on a **second connection to the same file**
(`runtime.rs:320-323`), so WAL + short transactions is the established convention.

---

## 7. Event and type system — `crates/vikram-runtime/src/types.rs`

### 7.1 `Metrics` (`38-55`) — nothing skill-related exists

`model: Option<String>`, `provider: Option<String>`, `context_used: u64`,
`context_window: Option<u64>`, `turn_elapsed_ms: Option<u64>`, `elapsed_ms: u64`,
`tokens_per_sec: Option<f64>`, `background_tasks: usize`, `mcp_connected: usize`,
`mcp_configured: usize`, `plugins_loaded: usize`, `plugins_installed: usize`,
`subagents_active: usize`, `thinking: bool`.
`context_ratio()` returns `Option<f32>` — `None` when the window is unknown (`57-66`).
Built in `runtime.rs:170-215`; note there is no `skills_*` field, so a "Skills N" status-bar
segment needs a new field here (or a dedicated event).

### 7.2 Model types

| Type | Lines | Notes |
|---|---|---|
| `RuntimeStatus` | `12-35` | `Initializing/Active/Idle/Stopping/Error(String)` + `label()`, `is_error()` |
| `AgentState` | `68-103` | `Idle, Running, Thinking, Waiting, Completed, Failed, Paused, Stopped`; `label()` maps `Running`→`"Working"` |
| `AgentProfile` | `105-113` | `id, name, role, description, icon, created_ms` — **the BOT model**, persisted in `agents` |
| `AgentSnapshot` | `115-124` | profile + `state`, `current_task`, `progress: Option<f32>`, `session`, `unread` |
| `TeamSnapshot` | `129-137` | **HIVE**: `id, name, members, session, paused, working` |
| `TaskState` | `139-145` | `Running, Completed, Failed, Cancelled` |
| `BackgroundTask` | `147-154` | `id, name, session, state, started_ms`; created by `TaskManager::start` (`agents.rs:164-177`), id = `task-{now_ms}` |
| `TurnState` | `156-164` | `Idle, Streaming, Tooling, WaitingApproval, Cancelled, Failed` |
| `SessionSnapshot` | `166-179` | includes `queued: usize` |
| `ApprovalPrompt` | `181-189` | `approval, session, verb, resource, reason, amendment` |
| `NoticeLevel` | `191-196` | `Info, Warn, Error` |
| `StateSnapshot` | `198-207` | `status, metrics, sessions, agents, teams, tasks, approvals` — **no skills** |
| `ActivityKind` | `210-241` | 11 values incl. **`Skill`** (`220`, label `236`) |
| `ActivityEvent` | `244-262` | 17 fields; **never constructed** (see 2.2) |
| `RuntimeEvent` | `264-341` | 26 variants, listed below |
| `now_ms()` | `343-349` | epoch millis |

`RuntimeEvent` variants (`264-341`): `Status`, `Metrics`, `SessionCreated`, `SessionUpdated`,
`SessionClosed`, `UserMessage`, `StreamStarted`, `StreamDelta`, `StreamThinkingDelta`,
`MessageCommitted`, `ToolStarted`, `ToolFinished`, `TurnFinished`, `AgentChanged`,
`AgentRemoved`, `TeamChanged`, `TeamRemoved`, `TaskChanged`, `ApprovalRequested`,
`ApprovalResolved`, `McpChanged{configured,connected,tools}`, `PluginsChanged{installed,loaded}`,
`Notice`, `Error`, `Shutdown`, `Activity(ActivityEvent)`.
No `SkillsChanged`.

### 7.3 HIVE presence (grep `-i hive`)

`hive.rs` (`ManagedTeam` `16-97`, `TeamManager` `101-133`, 5 tests) → `TeamSnapshot`
(`types.rs:129-137`) → `RuntimeInner.teams` (`runtime.rs:59`) → snapshot (`runtime.rs:279-291`) →
`hive_panel` (`panels.rs:140-204`) + `Regions.hive` (`layout.rs:15`, split at `63-80`) →
`ui.rs:25,40-42`. HIVE is a *team roster*, unrelated to skills but a good template for "runtime
collection → snapshot → side panel".

---

## 8. TUI

### 8.1 `App` state (`crates/vikram-tui/src/app.rs`)

| Item | Lines |
|---|---|
| `TabHit` (mouse hit regions) | `17-24` |
| spinner/plasma frame tables | `26-37` |
| `Focus { Input, Conversation, Bots }` | `39-44` |
| `ActivityKind { Tool, Notice, Error, Agent }` (view-side, distinct from contracts') | `46-52` |
| `Activity`, `Streaming`, `Notice` | `54-77` |
| `InspectorKind { Background, Mcp, Plugins, SubAgents, Context, Model }` | `78-86` |
| `Overlay { Help, Approvals, Inspector(InspectorKind), AddAgent{selected}, RenameSession{editor}, Autocomplete{candidates, selected} }` | `88-103` |
| `AgentAction { Pause, Resume, Stop }` | `105-110` |
| `UiCommand` (17 variants: `Submit, Queue, RunShell, SlashCommand, Cancel, NewSession, SwitchSession, RenameSession, CloseSession, AnswerApproval, SpawnAgent, AgentControl, RefreshMetrics, Quit`) | `112-161` |
| `SessionView` (+ `from_snapshot`, `is_busy`, `push_activity` capped at 64, `apply_snapshot`) | `163-260` |
| `struct App` fields | `262-287` |
| `sync_snapshot` (replaces status/metrics/agents/teams/tasks/approvals, then upserts sessions) | `364-384` |
| `on_event` — exhaustive fold of every `RuntimeEvent` | `403-618` |
| `active_task_count`, `active_agent_count`, `selected_agent_snapshot`, `agent_state_label`, `cycle_session`, `scroll_*` | `638-707` |

`App` has **no** `skills` field and `StateSnapshot` (`types.rs:198-207`) carries none, so the TUI
currently cannot see skills at all.

### 8.2 Key handling (`input.rs`)

Structure: `handle_key` (`13-173`) → release filter (`14-16`) → `Esc` clears agent selection
(`17-21`) → **any open overlay takes the key** (`22-24`) → global shortcut `match` (`30-166`) →
focus dispatch `match app.focus` (`168-172`) → `handle_input` (`175-356`) / `handle_conversation`
(`358-426`) / `handle_bots` (`428-482`).

Occupied shortcut slots: `Ctrl+C` `31`, `Ctrl+N` `44`, `Ctrl+Shift+Tab`/`BackTab`/`Ctrl+Tab`
`48-59`, `Ctrl+B` `60`, `Ctrl+P` `64`, `Ctrl+L` `68`, `F1` `75`, `F2`–`F7` `79-102`, `Ctrl+O` `103`,
`Ctrl+R` `115`, `Ctrl+A` `125`, `?` `137`, `PgUp/PgDn` `147-154`, `Esc` `155`.
**`F8`+ are free.** (`F2`–`F7` are all `open_inspector` calls, `79-102`.)

Overlay keys: `handle_overlay` (`571-700`) is an **exhaustive** `match` on `Overlay`, so a new
variant is a compile-time prompt to add an arm. `Help | Inspector(_)` share one arm
(`576-582`), `Approvals` (`583-616`), `AddAgent` (`617-642`), `Autocomplete` (`643-648`),
`RenameSession` (`649-698`) — the last is the template for a stateful text overlay (re-writes
`app.overlay` on each keystroke).

Input prefixes: `!` → `UiCommand::RunShell` (`536-542`), `/` → `UiCommand::SlashCommand` (`543-553`),
`@path` → file content inlined into the prompt (`485-519`, `556`).
Slash autocomplete candidate list is a hard-coded `vec![…]` of 13 commands (`197-211`) — a
`/skills` entry must be added there **and** in the terminal's dispatch (`terminal.rs:268-387`).

### 8.3 Panel functions (`widgets/panels.rs`, all `pub fn(frame, area, app, theme)` unless noted)

| Function | Lines | Signature detail |
|---|---|---|
| `session_tabs` | `45-138` | `(&mut Frame, Rect, &mut App, &Theme)` — the only `&mut App` (writes `tab_hits`, `new_session_x`) |
| `hive_panel` | `140-204` | teams list, `working && !paused` pulse |
| `bots_panel` | `206-295` | agents, selection marker when `Focus::Bots` |
| `plasma` | `297-349` | activity strip: running tool → streaming → last notice → `"ready"` |
| `status_bar` | `351-468` | model, ctx bar, uptime, turn, tok/s | Background, MCP, Plugins, Agents |
| `shortcuts` | `484-500` | one hint line; different text when an overlay is open (`489-493`) |
| `overlay` | `502-563` | autocomplete special-case `507-543`; otherwise `overlay_content` + `centered`, `Clear`, border, `Paragraph` |
| `overlay_content` | `565-588` | `match` on `Overlay` → `(title, Vec<Line<'static>>)` |
| `approval_lines` | `590-627` | list + `"a allow once · p keep on the list · d deny"` footer |
| `inspector_title` / `inspector_lines` | `629-638` / `640-768` | per-`InspectorKind` content |
| `add_agent_lines` | `770-805` | selection list + footer |
| `rename_lines` | `807-850` | mirrors the `Editor` |
| helpers | `kv 852`, `task_state_label 859`, `task_state_style 868`, `ratio_colour 877`, `ratio_bar 887`, `human 901`, `clip 911`, `centered 934` | `clip` is width-aware via `unicode-width` and ASCII-safe |
| constants | `TAB_TITLE_CAP 20`, `AGENT_NAME_CAP 21`, `AGENT_LINE_CAP 22`, `DIALOG_WIDTH 62` (`23`), `HELP_ROWS` 17 entries (`25-43`) |

### 8.4 Layout and draw order

`Regions` (`layout.rs:9-22`): `tabs`, `main`, `conversation`, `hive: Option<Rect>`,
`bots: Option<Rect>`, `input`, `status`, `shortcuts: Option<Rect>`, `compact: bool`.
`layout(area, show_hive, show_bots, input_lines)` (`31-104`): fixed rows = tabs 1 + input
`(lines+2).clamp(3,8)` + status 1 + shortcuts 0/1 (`33-35`); side panel only when
`width >= 88` (`BOTS_MIN_WIDTH`, `24`) **and** `main_height >= 4` (`41-42`); panel width
`(total/3).clamp(24,40)` (`27-29`); hive/bots split the panel 50/50 when both are shown
(`63-77`). Tests assert no overlap for 20 size combinations (`136-148`).

`ui::draw` (`ui.rs:14-57`) order: too-small guard `18-23` → `layout` `31` → `session_tabs` `33` →
conversation or empty state `35-38` → hive `40-42` → bots `43-45` → input `47` → status `48` →
shortcuts `50-52` → **overlay last, over the full area** `54-56`.

### 8.5 Minimal-disruption path for a Skills browser

Recommended shape (_recommendation_, grounded in the existing patterns):

| Change | File:line to touch | Mirrors |
|---|---|---|
| `App.skills: Vec<Skill>`, `App.skill_selection: usize` | `app.rs:262-287`, init in `new` `289-314` | `tasks`, `approval_selection` |
| `Overlay::Skills { selected: usize }` (+ optional `Overlay::SkillDetail { index: usize, scroll: u16 }`) | `app.rs:88-103` | `AddAgent { selected }` `93-95` |
| `UiCommand::RefreshSkills` | `app.rs:112-161` | `RefreshMetrics` `159` |
| `KeyCode::F(8)` → set overlay + push `RefreshSkills` | `input.rs:99-102` (after `F(7)`) | `F2`–`F7` `79-102` |
| new arm in `handle_overlay` (Esc/q close, Up/Down/Enter) | `input.rs:575-699` | `AddAgent` arm `617-642` |
| new arm in `overlay_content` + a `skills_lines(app, selected, theme)` fn | `panels.rs:566-587`, new fn near `approval_lines` `590` | `approval_lines` |
| help row `("F8", "skills")` | `panels.rs:25-43` | existing 17 rows |
| handle `RefreshSkills` via `handle.skill_list()` | `terminal.rs:183-425` (needs the handle) | `RefreshMetrics` `417-419`, `/tools` `310-320` |
| populate `app.skills` | new assignment in `terminal.rs`'s execute arm | `app.sync_snapshot(handle.snapshot())` `202` |
| re-export if the fn is public from a new module | `widgets/mod.rs:9` | current re-export list |
| status-bar count (optional) | `panels.rs:418-442` + `Metrics` field `types.rs:38-55` | Plugins segment `432-436` |

Zero changes needed to `layout.rs` or `ui.rs`: the overlay is already drawn last over the whole
`area` (`ui.rs:54-56`, `panels.rs:545-563`), centered, width-clamped (`546-548`).
Caveat: overlay height is `lines.len() + 2` capped by the area (`546`) and there is **no scrolling**
for overlay content today — a skill whose instructions exceed the terminal needs either a
`scroll` field on the variant or a line-window in the render function.

Second caveat: slash commands are a **dead end for durable output**. `/tools`, `/mcp`, `/plugins`
push `Message::system` straight into `view.messages` (`terminal.rs:271`, `314`, `325`, `337`),
which is the view copy — not journaled (`runtime.rs:161-168` is the only journal path) and not in
`SessionRuntime.messages`, so the text disappears on the next `sync_snapshot`
(`app.rs:364-384`). A `/skills` listing implemented the same way inherits that bug; the overlay
route avoids it.

---

## 9. CLI (`crates/vikram-cli/src/main.rs`)

| Item | Lines |
|---|---|
| `struct Cli` + global flags `--ascii --model --base-url --workspace` | `15-42` |
| `enum Command { Tui, Chat{message, fresh}, Sessions{action}, Agents, Config{action}, Doctor, Mcp }` | `44-72` |
| `enum SessionsAction { List, New{title}, Rm{id} }` | `74-85` |
| `enum ConfigAction { Show, Init, Set{key,value}, Path }` | `87-98` |
| `main` — load config, apply CLI overrides, dispatch (default `Tui`) | `100-146` |
| `runtime()` — multi-thread tokio builder | `148-153` |
| `run_async` / `run_tui` | `155-166` / `168-176` |
| `open_store()` → `Store::open(&database_path())` | `178-180` |
| `sessions()` subcommand impl | `182-220` |
| `format_age` / `format_date` (civil-from-days, no chrono) | `222-240` / `242-254` |
| `agents()` — `println!("{:<24} {:<14} {}", row.name, row.role, row.description)` | `256-269` |
| `mcp()` — `println!("{:<20} {}", "NAME", "COMMAND")` header + rows | `271-285` |
| `config_command` (Show prints path + pretty JSON + key presence; Set string-matches keys) | `287-372` |
| `exit(Result<(), String>)` | `374-382` |

Formatting conventions: no table crate, no colour, `format!` width columns
(`main.rs:264`, `280-282`), and `format_age` for relative timestamps (`222-240`).
Each subcommand is a free function returning `std::process::ExitCode`; errors print
`vikram: {err}` to stderr and return `FAILURE` (`374-382`).
`doctor.rs` uses a `ok/warn/fail/info` prefix convention (`doctor.rs:12-66`).
A `vikram skills` subcommand therefore = one `Command` variant (`main.rs:44-72`), one match arm
(`123-145`), one function following `agents()` (`256-269`). Note `vikram-cli` does **not** depend on
`vikram-policy` (`Cargo.toml:13-23`), so anything needing a `Decision` must go through the runtime.

---

## 10. Tests

| Fact | Evidence |
|---|---|
| **210** `#[test]`/`#[tokio::test]` functions across 33 files (counted with `grep -cE '^\s*#\[(tokio::)?test\]'`; **not executed**) | — |
| 6 of them are `#[tokio::test]` | `bus.rs:47,66`; `runtime.rs:838,852,871,900` |
| No `#[ignore]` | grep → 0 |
| All tests are **unit tests** in `#[cfg(test)] mod tests` at the bottom of the file; **no `crates/*/tests/`, no `benches/`, no `examples/`** | `find crates -type d -name tests` → empty |
| Heaviest coverage: `tools.rs` 27, `panels.rs` 21, `markdown.rs` 15, `machine.rs` 12, `policy/lib.rs` 11 | per-file counts |
| Zero tests in `turn.rs`, `types.rs`, `app.rs`, `ui.rs`, `main.rs`, `terminal.rs` | per-file counts |

Naming: full-sentence snake_case assertions of behaviour, not of method names —
`deny_outranks_a_later_allow` (`policy/lib.rs:318`), `installed_and_loaded_are_distinct_counts`
(`plugins.rs:129`), `submitting_without_a_provider_fails_visibly_instead_of_faking_output`
(`runtime.rs:872`), `the_regions_never_overlap_or_overflow` (`layout.rs:136`).

Fixtures:

- Filesystem: `tempfile::tempdir().unwrap()` (`skills.rs:161`, `plugins.rs:130`,
  `tools.rs:1962`).
- DB: `Store::open_in_memory()` (`store/lib.rs:628`, `656`, `679`, …).
- TUI: a `draw(width, height, |frame| …)` helper on `ratatui::backend::TestBackend` that flattens
  every cell symbol into one `String`, asserted with `rendered.contains("…")` and
  `assert_eq!(rendered.chars().count(), w*h)` for clipping (`panels.rs:957-962`, `1073`, `1143`).
- `insta` is a dev-dependency of `vikram-tui` (`vikram-tui/Cargo.toml:18`) but is **unused**: no
  `insta::` call, no `*.snap` files anywhere.
- `VIKRAM_HOME` is process-global, so runtime tests serialize on a `static HOME_ENV: Mutex<()>` and
  `unsafe { std::env::set_var }` the home (`runtime.rs:820-837`). Any new test that touches the home
  must take the same guard.

CI: `.github/workflows/ci.yml` — `cargo fmt --all -- --check` (documented **known red** on 29 files
at baseline 2026-10-31), `cargo clippy` **without** `-D warnings` (12 warning sites recorded),
`cargo test --workspace --all-targets`. nextest and insta are not installed, so
`cargo test` is the only gate.

---

## 11. Gaps and constraints

### 11.1 What does not exist

| Capability a skills subsystem needs | Status | Evidence |
|---|---|---|
| YAML frontmatter parser (`name`, `description`, `allowed-tools`, `version`) | **absent** | description = first raw line, `skills.rs:54-60`; no YAML crate in `Cargo.toml`/`Cargo.lock` |
| Slug/name validation, duplicate-name policy beyond first-wins | **absent** | `skills.rs:102-109` |
| Project skill overriding a user skill | **absent (impossible today)** | default dir scanned first, `skills.rs:92-98` |
| Multi-file skill (`references/`, `scripts/`) | **absent** | only `SKILL.md` read, `skills.rs:43-49` |
| Trust / provenance on a skill | **absent** | `Skill` has no trust field, `skills.rs:12-23`; content becomes `Trust::External` only when it re-enters as a tool result (`message.rs:79-89`) |
| Signature / hash / pinning | **absent** | `Skill` carries only `path` (`skills.rs:20`) |
| Install / uninstall / update | **absent** | no write path; and a tool writing to `$VIKRAM_HOME` is vetoed by the floor (`policy/lib.rs:156-168` + `runtime.rs:111`) |
| Enable/disable, precedence, per-workspace allowlist | **absent** | three fixed dirs, `skills.rs:91-100` |
| Size / path-traversal validation before load | **absent** | `scan` reads any `SKILL.md`, `skills.rs:33-71`; `resolve_within` unused here (`config.rs:177-199`) |
| Bundles / dependency between skills | **absent** | nothing in `crates/` |
| Injection into the model's system prompt (skill activation) | **absent** | no `Message::system` is ever built inside the runtime (grep: only CLI-side at `terminal.rs:243+`); `CompletionRequest.messages = session.messages()` (`turn.rs:113-118`) |
| Per-skill tool gating | **absent** | one global `ToolRegistry`, `tools.rs:44-89`; `turn.rs:116` sends every spec |
| Persistence of skill state (enabled, last used, source) | **absent** | schema has 5 tables, none skill-related (`store/lib.rs:457-508`) |
| `RuntimeEvent` for skills | **absent** | `types.rs:264-341` |
| `Metrics` counters | **absent** | `types.rs:38-55` |
| TUI surface | **absent** | `App` `app.rs:262-287`; `StateSnapshot` `types.rs:198-207` |
| CLI subcommand | **absent** | `Command` enum `main.rs:44-72` |
| `/skills` slash command | **absent** | candidate list `input.rs:197-211`; dispatch `terminal.rs:268-387` |
| Tests for any of the above | **absent** | `skills.rs` has 7 tests, all for scan/get/merge |

### 11.2 Architectural constraints to respect

1. **`run_turn` is the real loop; the kernel `Machine` is dead code.** `Machine` is only
   re-exported (`kernel/lib.rs:16`); grep for `Machine` outside `machine.rs` returns that single
   line. Do not build a skills execution path on `Machine` (`machine.rs`, 1031 lines) — extend
   `run_turn` (`turn.rs:42-260`) or the tool layer.
2. **Policy floor beats configuration.** A `Write` with `SideEffect != None` outside
   `workspace_root` is denied before rules run (`policy/lib.rs:156-168`). Skill install/write into
   `$VIKRAM_HOME/skills` cannot be a tool call; it must be a host-side operation (CLI / runtime
   method) or restricted to the workspace.
3. **Skill text can never grant permission.** It arrives as `Trust::External`
   (`message.rs:79-89`), and the floor forbids `grant.`, `policy.`, `self.` verbs from untrusted
   origins (`policy/lib.rs:127`, `136-147`). Any "skill may do X" metadata is untrusted input and
   must be treated as a *request subject to policy*, never as an allow rule.
4. **Tools are synchronous and statically registered.** `fn run(&self, …)` (not async,
   `tools.rs:41`) executed via `spawn_blocking` (`turn.rs:511`); registration only in
   `with_builtins` (`tools.rs:56-82`). Async install/fetch needs either a blocking client
   (`reqwest` with `blocking` is already enabled, `vikram-runtime/Cargo.toml:20`) or a runtime-side
   task.
5. **Tool output is truncated at 20 000 bytes** (`tools.rs:22`), so a long `SKILL.md` delivered via
   `skill.load` is silently (but marked) clipped. A detail view must read the file directly, not
   reuse the tool.
6. **`ActivityKind::Skill` exists but the activity pipeline is inert** (`types.rs:220`;
   `ActivityEvent` never constructed; `RuntimeEvent::Activity(_)` ignored at `app.rs:607`). Emitting
   skill activity means either wiring `ActivityEvent` for real or using `ToolStarted/ToolFinished`
   summaries (`turn.rs:496-539`).
7. **Two exhaustive matches will break on new enum variants** — `App::on_event`
   (`app.rs:403-618`) and `handle_overlay` (`input.rs:571-700`). That is the safety net for free.
8. **`VIKRAM_HOME` is process-global**; tests touching it must serialize on
   `runtime.rs:820-828`'s mutex pattern.
9. **Schema changes are DDL-only.** New tables can be appended to `migrate`
   (`store/lib.rs:457-508`); altering existing tables needs a migration mechanism that does not
   exist.
10. **`vikram-cli` has no `vikram-policy` dependency** (`Cargo.toml:13-23`) — a CLI command cannot
    evaluate policy locally.
11. **`cargo fmt` is known-red on 29 files** (`ci.yml` header comment). Do not run a workspace-wide
    `cargo fmt` inside a skills change; format only touched files, or the diff becomes unreviewable.
12. **The runtime's skill registry is never used.** `RuntimeInner.skills` (`runtime.rs:62`) is read
    only by three uncalled handle methods (`782-792`); the tools re-scan the disk
    (`tools.rs:1309`, `1359`). Reuse by injecting the registry into the tools, not by adding a
    fourth loader.

### 11.3 Reuse-before-create checklist

| Need | Reuse | Do not create |
|---|---|---|
| skill discovery | `SkillRegistry::scan/for_workspace/merge` (`skills.rs:33-109`) | a second scanner |
| skill home | `vikram_home()` + `default_dir()` (`config.rs:105-116`, `skills.rs:74-76`) | `dirs`/`directories` |
| summary type for lists | extend `Skill` (`skills.rs:12-23`) and add `SkillSummary` if `instructions` must stay out of snapshots | a parallel `SkillItem` duplicating 5 fields |
| tool exposure | `Tool` impl pattern (`tools.rs:1284-1371`) registered in `with_builtins` (`76-77`) | a special-cased branch in `run_tool` (`turn.rs:376`) |
| permission shape | `PermissionRequest::new(ActionKind::Read, "skill.*", …)` (`contracts/policy.rs:42-58`) | a new `ActionKind` |
| durable config | `McpConfig`-style sidecar JSON under the home (`config.rs:141-174`) | a new serializer |
| durable state | new `CREATE TABLE IF NOT EXISTS` in `store::migrate` (`457-508`) | a second database file |
| TUI list | `overlay` + `overlay_content` (`panels.rs:502-588`), `approval_lines` as the template (`590-627`) | a new layout region |
| TUI counters | `Metrics` + a `*Changed` event (`types.rs:38-55`, `328-331`; `runtime.rs:442-446`) | polling in `App::on_event` |
| CLI listing | `agents()` `format!("{:<…}")` pattern (`main.rs:256-269`) | a table crate |
| tests | `tempfile::tempdir` (`skills.rs:161`), `TestBackend` `draw()` (`panels.rs:957-962`), `HOME_ENV` mutex (`runtime.rs:820`) | new harness |

---

## 12. One-paragraph summary

Skills exist as a 230-line read-only directory scanner (`skills.rs`): a `SKILL.md` per directory,
description from the first raw line, full text held in memory, discovered from
`$VIKRAM_HOME/skills` then the workspace, first-wins by name, surfaced to the model through exactly
two read-only tools (`skill.list`, `skill.load` at `tools.rs:1284-1371`) that re-scan the disk on
every call, with a `RuntimeHandle::skill_*` API (`runtime.rs:782-792`) nobody calls and a
`Metrics`/`RuntimeEvent`/TUI surface that contains zero skill fields. Everything a real subsystem
needs — frontmatter, trust, install, bundles, enable/disable, per-skill tool gating, system-prompt
activation, persistence, CLI and TUI — is absent, and the two hard architectural facts that shape
the design are: policy's mandatory floor forbids writing outside the workspace (so install cannot be
a tool), and skill text always enters as `Trust::External` (so skill metadata can never be a
permission grant). The cheapest correct build is: extend `skills.rs` + `Skill`, inject the single
runtime registry into the existing two tools, add a sidecar config file following `McpConfig`, add a
`skills` table to `store::migrate`, one `RuntimeEvent::SkillsChanged` + three `Metrics` counters, a
`Overlay::Skills` variant modelled on `AddAgent`/`approval_lines` drawn over the existing full-area
overlay, and a `vikram skills` subcommand following `agents()` — six files, no new crate, no new
dependency, no changes to `layout.rs`, `ui.rs`, or `turn.rs`.
