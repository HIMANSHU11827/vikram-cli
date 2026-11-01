# Codex Skills — Complete Implementation Analysis

Status: research note. Every claim is labelled **DOCUMENTED** (OpenAI docs), **VERIFIED-IN-SOURCE** (read in the
local clone at the cited path), **INFERRED** (reasoned from source), or **UNKNOWN** (no evidence found).

Sources:

- Local clone: `/home/himanshu/Desktop/VIKRAM-CLI/codex/` — primary, because the shipped code is more complete than
  the shipped prose. `codex/docs/skills.md` in this tree is a **3-line stub** that only links to the web page
  (**VERIFIED-IN-SOURCE**).
- <https://developers.openai.com/codex/skills> (page title "Build skills") — fetched.
- <https://developers.openai.com/api/docs/guides/tools-skills> — fetched.
- <https://developers.openai.com/plugins/build/plugins> — fetched.

## 1. Two implementations coexist in one tree

This is the first thing to understand before reading any path: there are **two** skill subsystems.

| Crate | Role | Evidence |
| - | - | - |
| `codex-rs/skills` | Parser, metadata, interface, mention parsing, selection, bundled samples | VERIFIED-IN-SOURCE |
| `codex-rs/ext/skills` | The loader: root discovery, traversal, merge, namespace, rendering, prompt fragments, tools | VERIFIED-IN-SOURCE |

`ext/skills` is the newer/richer layer (it owns `host_roots.rs`, `render.rs`, `catalog_prompt.rs`, `fragments.rs`,
`loader/`). Anything about discovery, budgeting, or injection belongs there. Anything about frontmatter text belongs
in `skills/src/parser.rs`. Where the two disagree, `ext/skills` wins at runtime (**INFERRED** from the dependency
direction: `ext/skills` imports `codex_skills`).

## 2. Scopes

Four scopes exist as a protocol enum: `User`, `Repo`, `System`, `Admin` (**VERIFIED-IN-SOURCE**
`codex/codex-rs/protocol/src/protocol.rs:4162`). The client-facing v2 schema also adds `enabled: bool` to its
`SkillMetadata` (**VERIFIED-IN-SOURCE**
`codex/codex-rs/app-server-protocol/src/protocol/v2/plugin.rs`).

Docs use the same four names with example paths — `$CWD/.agents/skills`, `$CWD/../.agents/skills`,
`$REPO_ROOT/.agents/skills`, `$HOME/.agents/skills`, `/etc/codex/skills` (**DOCUMENTED**).

## 3. Discovery roots — the authoritative order

`roots_from_layer_stack()` iterates config layers high→low and pushes roots per layer
(**VERIFIED-IN-SOURCE** `codex/codex-rs/ext/skills/src/host_roots.rs:112-170`):

| Layer | Path | Scope |
| - | - | - |
| `Project` | `config_folder/skills` | `Repo` |
| `User` | `config_folder/skills` (deprecated `$CODEX_HOME/skills`) | `User` |
| `User` | `$HOME/.agents/skills` | `User` |
| `User` | `system_cache_root_dir(config_folder)` = `$CODEX_HOME/skills/.system` | `System` |
| `System` | `config_folder/skills` | `Admin` |

Note the two surprises in that table, both real in source:

1. The **bundled system skills are registered under the `User` config layer** but carry scope `System`
   (`host_roots.rs:149-152`). Scope is a property of the root, not of the layer that declared it.
2. `$CODEX_HOME/skills` is commented as *"Deprecated user skills location … kept for backward compatibility"*
   (`host_roots.rs:135-136`). It still resolves; it is no longer the recommended location. `~/.agents/skills` is.

`PackagedDefaults`, `Mdm`, `EnterpriseManaged`, `SessionFlags`, and both `LegacyManaged*` layers contribute **no**
skill roots (`host_roots.rs:160-165`).

## 4. Traversal and project boundaries

**Repo-layer probing is per-directory, not per-repo.** `repo_agents_skill_roots()` walks every directory from the
project root down to the current working directory and probes `<dir>/.agents/skills` in each
(**VERIFIED-IN-SOURCE** `host_roots.rs:176-247`, helper `dirs_between_project_root_and_cwd` at `:374`). This is why
the docs list `$CWD/.agents/skills`, `$CWD/../.agents/skills`, and `$REPO_ROOT/.agents/skills` as separate entries —
they are literally separate roots, so a monorepo package can carry its own skills without touching the repo root.

**The project root is found by markers,** via `project_root_markers_from_stack` / `find_project_root`
(`host_roots.rs:309`, `:328`). Traversal stops there — it does not climb to `/` (**INFERRED** from
`dirs_between_project_root_and_cwd` bounding both ends).

**Hard bounds on the walk** (**VERIFIED-IN-SOURCE** `codex/codex-rs/ext/skills/src/loader/mod.rs:18-25`,
`loader/discovery.rs:17-18`):

| Constant | Value | Meaning |
| - | - | - |
| `MAX_SCAN_DEPTH` | 6 | recursion depth in `Recursive` mode |
| `MAX_SKILLS_DIRS_PER_ROOT` | 2000 | skill *directories* per root |
| `MAX_SKILLS_ENTRIES_PER_ROOT` | 20_000 | directory entries per root |
| `MAX_CONCURRENT_ROOT_SCANS` | 8 | roots scanned in parallel |
| `MAX_CONCURRENT_SKILL_LOADS` | 64 | skills parsed in parallel |

**Two walk modes** (`loader/discovery.rs:70-71`): `Recursive` uses `MAX_SCAN_DEPTH` (6); `DirectChildren` stops at
depth 2 — i.e. `<root>/<skill>/SKILL.md` only. Which mode a root uses is a property of the root/adapter, not the
skill (**INFERRED** — the mode is an input to discovery options, not read from the skill).

**Hidden directories are pruned** during the walk (`HiddenDirectoryPolicy`), alongside a
`DirectorySymlinkPolicy` — both explicit enums in `discovery.rs:20-25`. Docs confirm symlinked skill folders are
followed (**DOCUMENTED**).

**Plugin discovery alongside skills**: while scanning, `DISCOVERABLE_PLUGIN_MANIFEST_PATHS` is checked
(`discovery.rs:134`). The list is
`[".codex-plugin/plugin.json", ".claude-plugin/plugin.json", ".cursor-plugin/plugin.json"]`
(**VERIFIED-IN-SOURCE** `codex/codex-rs/exec-server-protocol/src/protocol.rs:55`).

**Per-skill metadata probe**: `SkillMetadataDiscovery` is a three-state enum — `Present(PathUri)`, `Absent`,
`Probe(PathUri)` (`discovery.rs:48-51`, returning at `:200-217`). `Probe` matters: the loader does not require
`agents/openai.yaml` to pre-exist, it can carry a path it will look at later.

## 5. Indexing, dedupe, namespacing

**Dedupe is by file path, not by name.** `dedupe_skill_roots_by_path` (`host_roots.rs:395`) de-duplicates *roots*;
the merge step de-duplicates *skills* by `path_to_skills_md` (**VERIFIED-IN-SOURCE**
`codex/codex-rs/ext/skills/src/loader/host_merge.rs`). Two skills with the same `name` in different directories are
**both kept**.

This reconciles a documented/source tension: the docs say "If two skills share the same `name`, Codex doesn't merge
them; both can appear in skill selectors" (**DOCUMENTED**). That is consistent — no *name*-based merge exists. What
source dedupes is *identical paths* reached twice (e.g. via overlapping roots symlinks make reachable).

**Ordering / precedence.** `scope_rank()` (`host_merge.rs:260-265`):

```
Repo => 0,  User => 1,  System => 2,  Admin => 3
```

Skills are sorted by `(scope_rank, name, path)` (**INFERRED** from the comparator in `host_merge.rs`). Lower rank
sorts first, so **Repo beats User beats System beats Admin** in listing order. Sorting is not overwriting: nothing is
dropped by rank.

Also in `host_merge.rs`: **migrated command-skill suppression** — skills that duplicate a migrated legacy command are
suppressed rather than shown twice (**VERIFIED-IN-SOURCE**; the mechanism exists in the merge pass).

**Namespacing for plugins.** `ResolvedSkillNamespace::{Plain, Plugin(String)}`, and `qualify()` produces
`format!("{namespace}:{base_name}")` (**VERIFIED-IN-SOURCE**
`codex/codex-rs/ext/skills/src/loader/namespace.rs:168-179`). Nested plugin roots use `nested_namespaces` and **the
deepest matching root wins** (`for_skill()` at `:150`). This is why `MAX_QUALIFIED_NAME_LEN = 2*64+1 = 129` exists.

## 6. Enabled / disabled state

**Not part of `SKILL.md`.** State is config, in `codex-rs/config/src/skills_config.rs` (**VERIFIED-IN-SOURCE**):

- `SkillsConfig { bundled: Option<BundledSkillsConfig>, include_instructions: Option<bool>,
  max_context_tokens: Option<NonZeroUsize>, config: … }` (`:41-52`)
- `BundledSkillsConfig { enabled: bool }`, default `true` (`:60-62`, `default_enabled()` at `:15`)
- `SkillConfig { path, name, enabled }` (`:21-28`)
- `SkillConfigRule` / `SkillConfigRules` (`:88-95`), with `resolve_disabled_paths()`

**Resolution semantics** (`:100`, `:103`, `:184`): *"Applies rules in order; later rules override earlier rules for
matching skills"*, and layer order is preserved *"so a later name selector can override an earlier path selector."*

Docs give the user-facing form: `[[skills.config]] path = "…" enabled = false` (**DOCUMENTED**), and
`skills.max_context_tokens` up to **10,000** (**DOCUMENTED**, matching
`MAX_CONFIGURED_SKILL_METADATA_TOKEN_BUDGET = 10_000` at `render.rs:24`).

`ScopedSkillsConfig { required: Vec<String> }` (`:34-36`) pins skills as non-optional — **INFERRED**: a `required`
entry is forced into the session regardless of implicit-invocation policy.

Disabled paths propagate everywhere: name counting excludes them (`name_counts.rs`), and selection refuses to resolve
to them (`selection.rs:24`, `:29`).

## 7. Activation — three routes

**7a. Explicit `$name`.** `TOOL_MENTION_SIGIL: char = '$'` (**VERIFIED-IN-SOURCE**
`codex/codex-rs/skills/src/mentions.rs:41`). Mention characters are `a-z A-Z 0-9 _ - :` — the `:` exists for
namespaced plugin skills. A linked form `[$name](skill://path)` is also parsed: `SKILL_PATH_PREFIX = "skill://"`
(`:39`). Env-var-looking tokens are suppressed via a suppression list so `$HOME` is not a skill mention.

**7b. Structured selection.** `collect_explicit_skill_mentions()` (`selection.rs:44`) resolves
`UserInput::Skill { name, path, .. }` **by path first**, then falls back to text `$skill-name`; "plain names are only
used when the match is unambiguous" (`:35-39`). Ambiguity = two skills with the same name → plain name does not
resolve. That is the practical consequence of the no-merge rule.

**7c. Automatic / implicit.** The catalogue tells the model: *"If the user names a skill (with `$SkillName` or plain
text) OR the task clearly matches a skill's description shown above, you must use that skill for that turn"*
(**VERIFIED-IN-SOURCE** `codex/codex-rs/ext/skills/src/catalog_prompt.rs:8`). Implicit activation can be disabled per
skill via `policy.allow_implicit_invocation: false` in `agents/openai.yaml`; `allows_implicit_invocation()` defaults
to **true** (`codex-rs/skills/src/model.rs`, `metadata.rs:55`).

**7d. `/skills` is a menu, not a text command.** `SlashCommand::Skills => self.open_skills_menu()`
(**VERIFIED-IN-SOURCE** `codex/codex-rs/tui/src/chatwidget/slash_dispatch.rs:527`); the slash command's description
is "use skills to improve how Codex performs specific tasks" (`tui/src/slash_command.rs:117`). Docs mention `/skills`
and `$` for CLI/IDE (**DOCUMENTED**).

**Turn scoping.** *"Do not carry skills across turns unless re-mentioned"* (`catalog_prompt.rs:8`, repeated at `:25`).
Activation is per-turn, deliberately.

**Implicit skill *access* detection.** Separately from activation, `codex-rs/skills/src/invocation.rs` detects when a
shell command touches a skill's files: script runners `python, python3, bash, zsh, sh, node, deno, ruby, perl, pwsh`
(`:81`), extensions `.py .sh .js .ts .rb .pl .ps1` (`:103`), and `ParsedCommand::Read { path, .. }` (`:63`, `:135`).
This is how Codex knows a skill's script *ran* — useful for trust decisions (§14).

## 8. Instruction injection — exact roles and markers

Two distinct fragments, two distinct roles (**VERIFIED-IN-SOURCE**
`codex/codex-rs/ext/skills/src/fragments.rs`):

| Fragment | Role | `content_kind` | Markers |
| - | - | - | - |
| `AvailableSkillsInstructions` | `developer` | `skills.catalog` | `<skills_instructions>` … `</skills_instructions>` |
| `SkillInstructions` | `user` | `skills.selected_skill_instructions` | `<skill>` … `</skill>` |

Marker constants: `SKILLS_INSTRUCTIONS_OPEN_TAG = "<skills_instructions>"` (**VERIFIED-IN-SOURCE**
`codex/codex-rs/protocol/src/protocol.rs:134`).

The selected skill body is wrapped with `<name>`, `<path>`, and optionally `<resource_access>`
(`fragments.rs:73-85, 108+`). `SkillResourceAccess { authority, package, main_resource }` exists for
executor-/cloud-package skills that have no host path.

**This is the single most important security fact about Codex skills:** the *catalogue* is a **developer** message,
but the *skill body itself is injected as a user message*. OpenAI's API docs state it directly: "Skill instructions
are user prompt input (not system prompt input)" (**DOCUMENTED**, tools-skills guide). A skill therefore sits at
user-privilege in the prompt hierarchy, not system-privilege. Any claim that a skill overrides system instructions
is wrong.

## 9. Context budgeting

Constants (**VERIFIED-IN-SOURCE** `codex/codex-rs/ext/skills/src/render.rs:23-33`):

| Constant | Value |
| - | - |
| `DEFAULT_SKILL_METADATA_CHAR_BUDGET` | 8_000 |
| `MAX_CONFIGURED_SKILL_METADATA_TOKEN_BUDGET` | 10_000 |
| `MAX_SKILL_PROMPT_BYTES` | 8_000 |
| `SKILL_METADATA_CONTEXT_WINDOW_PERCENT` | 2 |
| `MAX_CATALOG_SKILL_DESCRIPTION_CHARS` | 1_024 |
| `MAX_SKILL_NAME_BYTES` | 256 |
| `MAX_SKILL_PATH_BYTES` | 1_024 |

Docs state the same rule in prose: the list gets "at most 2% of the model's context window, or 8,000 characters when
the context window is unknown" (**DOCUMENTED**) — which is exactly the two constants above, so docs and code agree
here.

**The allocation algorithm** (`skill_metadata_budget()` at `render.rs:169`, `allocate_description_chars()` at `:469`):
each skill line gets a `SkillLineAllocation` — full description, or `DescriptionChars(n)` (a per-skill character
allowance), and when the budget cannot be met at all, some lines are omitted. Truncation is *measured*:
`truncated_description_chars` / `truncated_description_count` are tracked and surfaced
(`render.rs:135-136`, `:158-165`). The omission marker text is:

`- {n} additional skill(s) omitted from this bounded skills list.` (`render.rs:1183`)

So the degradation is **graceful and reported**, not silent: descriptions shrink first, then skills drop out, and the
model is told how many it cannot see. (`INFERRED` for the exact ordering of stages; `VERIFIED-IN-SOURCE` for the
constants, the per-line allocation type, the counters, and the omission string.)

**Metadata-only catalogue.** The catalogue is name + description + locator only. The intro strings make the locator
semantics explicit — `SKILLS_INTRO_WITH_SOURCE_LOCATORS`, `..._WITH_HOST_ALIASES`, `..._WITH_RESOURCE_ALIASES`
(`catalog_prompt.rs:3-5`), with matching "how to use" blocks at `:7` and `:25` and
`RESOURCE_ALIAS_INSTRUCTIONS`. Three locator kinds exist: `file` (host filesystem), `executor package` / `cloud
package` (accessed via `skills.read`), `custom resource` (provider's own mechanism).

**Skill roots table.** In alias mode the catalogue is accompanied by a `### Skill roots` table so the model can expand
a short path to an absolute one (`catalog_prompt.rs:24`) — a token-saving device: repeat each root once instead of
each skill's full path.

## 10. `agents/openai.yaml`

Location: `SKILLS_METADATA_DIR = "agents"`, `SKILLS_METADATA_FILENAME = "openai.yaml"`
(**VERIFIED-IN-SOURCE** `codex/codex-rs/ext/skills/src/loader/mod.rs:20-21`). Parser:
`codex-rs/skills/src/metadata.rs`.

Fields, from the shipped reference (**VERIFIED-IN-SOURCE**
`codex-rs/skills/src/assets/samples/skill-creator/references/openai_yaml.md`):

```yaml
interface:
  display_name: "Optional user-facing name"
  short_description: "Optional user-facing description"   # 25–64 chars
  icon_small: "./assets/small-400px.png"
  icon_large: "./assets/large-800px.png"
  brand_color: "#3B82F6"
  default_prompt: "…"
dependencies:
  tools:
    - type: mcp
policy:
  allow_implicit_invocation: true
```

Rules:

- `interface.display_name` — "Human-facing title shown in UI skill lists and chips."
- `interface.short_description` — "Human-facing short UI blurb (25–64 chars) for quick scanning."
- `interface.icon_small` — "Path to a small icon asset (relative to skill dir). Default to `./assets/`."
- `interface.brand_color` — hex color; validated as `#RRGGBB` (`interface.rs` `resolve_color_str`).
- `dependencies.tools[].type` — **only `mcp` is supported** (stated in the reference file).
- `policy.allow_implicit_invocation` — "When false, the skill is not injected into…" (default `true`).
- `policy.products` — `Vec<Product>` (`metadata.rs:57`), a product gate.

**Validation limits** (`metadata.rs:11-16`): `MAX_DEPENDENCY_TYPE_LEN=64`, `MAX_DEPENDENCY_TRANSPORT_LEN=64`,
`MAX_DEPENDENCY_VALUE_LEN=1024`, `MAX_DEPENDENCY_DESCRIPTION_LEN=1024`, `MAX_DEPENDENCY_COMMAND_LEN=1024`,
`MAX_DEPENDENCY_URL_LEN=1024`. In `interface.rs:10-11`: `MAX_NAME_LEN=64`, `MAX_DESCRIPTION_LEN=1024`.

**Icon path rules are enforced, and violations are ignored-with-a-warning rather than fatal**
(`interface.rs:88-144`): absolute path → ignored; component outside `assets/` → ignored; `..` allowed only via
`PluginShared` and the lexically normalised result must start with the plugin's `assets/` directory.

## 11. skill-creator and skill-installer

Both ship as **bundled embedded** skills under `codex-rs/skills/src/assets/samples/`
(`skill-creator`, `skill-installer`, `openai-docs`, `imagegen`, `review-agent`), embedded at compile time via
`include_dir!` (`codex-rs/skills/src/lib.rs:58`).

**skill-creator** teaches the format (**VERIFIED-IN-SOURCE**
`…/samples/skill-creator/SKILL.md`): folder named after the skill, lowercase letters/digits/hyphens, "under 64
characters", a three-stage disclosure model, and two scripts — `scripts/init_skill.py` (scaffold) and
`scripts/quick_validate.py` (check). Authoring location is stated as `$CODEX_HOME/skills` or `~/.codex/skills`.

`quick_validate.py` rules (**VERIFIED-IN-SOURCE**):
- `MAX_SKILL_NAME_LENGTH = 64` (`:12`)
- frontmatter fenced by `re.match(r"^---\n(.*?)\n---", content, re.DOTALL)` (`:27`)
- `allowed_properties = {"name", "description", "license", "allowed-tools", "metadata"}` (`:40`) — **no
  `compatibility`**, a real divergence from the open spec
- name `^[a-z0-9-]+$` (`:61`), no leading/trailing `--`, length cap (`:71`)
- description ≤1024 (`:87`), rejects `<`/`>` and TODO placeholders; also checks code-fence integrity (`:97`)

**skill-installer** installs from curated sources (**VERIFIED-IN-SOURCE**
`…/samples/skill-installer/SKILL.md`): destination `$CODEX_HOME/skills/<skill-name>` (default `~/.codex/skills`);
curated source `github.com/openai/skills/tree/main/skills/.curated`, plus `.experimental`; script flags
`--repo --path --ref --dest --method --name`; **aborts if the destination already exists** — no overwrite.

## 12. Distribution and updates

**Bundled system skills** install to `CODEX_HOME/skills/.system` (`system_cache_root_dir`, `lib.rs:60-72`). An
idempotence fingerprint guards re-installation: `.codex-system-skills.marker` keyed by
`SYSTEM_SKILLS_MARKER_SALT = "v1"` (`lib.rs:63`, hashed at `:119`). Bump the salt → all system skills rewrite.
These are **not** user-installable; `skill-installer` documents that they are managed.

**Plugins as a distribution channel** (**DOCUMENTED**, plugins docs): a plugin directory carries `skills/`;
manifest `plugin.json`; marketplaces at `$REPO_ROOT/.agents/plugins/marketplace.json` and
`~/.agents/plugins/marketplace.json`; CLI `codex plugin marketplace add|list|upgrade|remove`; install cache
`~/.codex/plugins/cache/$MARKETPLACE_NAME/$PLUGIN_NAME/$VERSION/`. `onboardingSkill` is a
`skills/setup/SKILL.md` convention. Plugin-provided skills get the `namespace:name` treatment from §5, and v2
`SkillMetadata` carries `plugin_id` / `remote_plugin_id` (**VERIFIED-IN-SOURCE** `model.rs`,
`app-server-protocol/.../v2/plugin.rs`).

**Updates**: no per-skill update mechanism exists in the skills crates (**UNKNOWN** — none found). Updates are the
host's problem: plugins re-resolve by marketplace version, bundled skills by marker fingerprint.

## 13. Programmatic access — the `skills` tool namespace

`codex-rs/ext/skills/src/tools/mod.rs`: namespace `skills`, `MAX_HANDLE_BYTES = 2_048`,
`MAX_SKILL_RESPONSE_BYTES = 512 * 1024` (`:54-55`). Tools: `skills.list`, `skills.read`, and `skills.installations`
when `discovery_v2` is on. The executor protocol notes the path convention: *"Path to SKILL.md, not its containing
directory"* (`exec-server-protocol/src/capabilities.rs`, `ExecutorSkill`). This matters because §14's trust check
operates on the `SKILL.md` path, not the folder.

## 14. Security

**14a. Skill body is user-role text.** Covered in §8. A skill cannot claim system authority. The model *is*
instructed to obey matching skills (`catalog_prompt.rs:8`), so a malicious skill is a
**trusted-looking instruction from user level** — the classic injection shape, not an escalated one.

**14b. Trusted skill roots.** `codex-rs/ext/guardian-v2/src/async_scorer/trusted_skills.rs` builds
`TrustedSkillRoots` from exactly two places: `codex_home/skills` and `$HOME/.agents/skills` (`:20-22`). Bounds on what
it will remember (**VERIFIED-IN-SOURCE** `:10-12`):

- `MAX_TRUSTED_SKILLS = 16`
- `MAX_TRUSTED_SKILL_PATH_BYTES = 512`
- `MAX_TRUSTED_SKILL_PATHS_BYTES = 2_048`

The file's own doc comment: *"Captures bounded, canonical paths for invoked user-owned skills."* Practical
consequence: **repo-layer skills are outside the trusted-root set**. A skill checked into a repository is not
trusted the way one in your home directory is (**INFERRED** from the root list — this is a meaningful asymmetry and
should be the headline for anyone adopting repo skills).

**14c. Script execution.** Nothing in the skills crates executes a skill's scripts. Codex detects that a script was
invoked (`invocation.rs`, §7) and the shell tool runs it under the same approval policy as any other command
(**INFERRED** — no skills-specific sandbox was found). Therefore: **a skill's script inherits the user's shell
permissions**. `allowed-tools` does not constrain this (§ agent-skills-standard.md §7).

**14d. Tool permissions.** `agents/openai.yaml` can declare `dependencies.tools[].type: mcp` — a *requirement*, not a
grant. No skills-level permission model was found (**UNKNOWN**).

**14e. Prompt-injection stance, as documented.** The API skills guide is blunt (**DOCUMENTED**): "Treat Skills as
privileged code and instructions", "Don't expose an open Skills repository to end-users", and it names
"prompt injection-driven data exfiltration" as the risk. Codex's own mitigations are structural rather than
detective: per-turn activation (no carry-over), developer-role catalogue vs user-role body, bounded trusted roots,
and size caps.

**14f. What is *not* defended.** No signing, no integrity hash, no publisher identity, no script sandbox, no
`allowed-tools` enforcement. A skill from an untrusted repo is code review you owe yourself.

## 15. `SKILL.json` — a dangling doc comment

v2 protocol doc comments say *"Prefer SKILL.json interface.short_description"*
(**VERIFIED-IN-SOURCE** `app-server-protocol/src/protocol/v2/plugin.rs:461-511` and generated
`schema/typescript/v2/SkillMetadata.ts`). **There is no `SKILL.json` implementation anywhere** — 7 comment hits, zero
code paths. Label: **UNKNOWN**. Do not build against it. The real file is `SKILL.md` + `agents/openai.yaml`.

## 16. Summary of real divergences

| Topic | Open spec | Codex | Status |
| - | - | - | - |
| `compatibility` field | Supported, ≤500 | Rejected by `quick_validate.py` | Real gap |
| Duplicate `name` | Unspecified | Not merged; both listed; plain `$name` becomes ambiguous | Documented + source |
| `allowed-tools` | Experimental, varies | Validated, not enforced | Real gap |
| Name vs directory | Must match | Not checked by validator | Real gap |
| Root precedence | Unspecified | Repo 0 → User 1 → System 2 → Admin 3 | Source only |
| Repo skills trust | Unspecified | Excluded from `TrustedSkillRoots` | Source only |

## Evidence index

| # | Claim | Source | Label |
| - | - | - | - |
| 1 | `codex/docs/skills.md` is a 3-line stub linking to the web page | codex/docs/skills.md | VERIFIED-IN-SOURCE |
| 2 | Two crates: `codex-rs/skills` and `codex-rs/ext/skills` | crate trees | VERIFIED-IN-SOURCE |
| 3 | `enum SkillScope { User, Repo, System, Admin }` | codex-rs/protocol/src/protocol.rs:4162 | VERIFIED-IN-SOURCE |
| 4 | Root order: Project→Repo, User→`$CODEX_HOME/skills`(deprecated)+`~/.agents/skills`+`.system`(System), System→Admin | ext/skills/src/host_roots.rs:112-170 | VERIFIED-IN-SOURCE |
| 5 | `$CODEX_HOME/skills` marked deprecated | host_roots.rs:135-136 | VERIFIED-IN-SOURCE |
| 6 | Per-directory `.agents/skills` probing root→cwd | host_roots.rs:176-247, :374 | VERIFIED-IN-SOURCE |
| 7 | Project root from `project_root_markers` | host_roots.rs:309, :328 | VERIFIED-IN-SOURCE |
| 8 | `MAX_SCAN_DEPTH=6`, `MAX_SKILLS_DIRS_PER_ROOT=2000`, `MAX_CONCURRENT_ROOT_SCANS=8` | ext/skills/src/loader/mod.rs:18-25 | VERIFIED-IN-SOURCE |
| 9 | `MAX_SKILLS_ENTRIES_PER_ROOT=20_000`, `MAX_CONCURRENT_SKILL_LOADS=64` | loader/discovery.rs:17-18 | VERIFIED-IN-SOURCE |
| 10 | `Recursive`=MAX_SCAN_DEPTH, `DirectChildren`=2 | loader/discovery.rs:70-71 | VERIFIED-IN-SOURCE |
| 11 | `SkillMetadataDiscovery::{Present,Absent,Probe}` | loader/discovery.rs:48-51, :200-217 | VERIFIED-IN-SOURCE |
| 12 | Plugin manifests: `.codex-plugin`, `.claude-plugin`, `.cursor-plugin` | exec-server-protocol/src/protocol.rs:55 | VERIFIED-IN-SOURCE |
| 13 | Dedupe by `path_to_skills_md`; scope_rank Repo0/User1/System2/Admin3 | ext/skills/src/loader/host_merge.rs:260-265 | VERIFIED-IN-SOURCE |
| 14 | Namespaced name `"{namespace}:{base_name}"`, deepest root wins | loader/namespace.rs:150, :168-179 | VERIFIED-IN-SOURCE |
| 15 | `MAX_QUALIFIED_NAME_LEN = MAX_NAME_LEN*2+1` (129) | loader/mod.rs:22-23 | VERIFIED-IN-SOURCE |
| 16 | `SkillsConfig{bundled,include_instructions,max_context_tokens,config}`; later rules override | config/src/skills_config.rs:41-52, :100-103, :184 | VERIFIED-IN-SOURCE |
| 17 | `skills.max_context_tokens` max 10,000 | developers.openai.com/codex/skills + render.rs:24 | DOCUMENTED + VERIFIED |
| 18 | `TOOL_MENTION_SIGIL='$'`, `skill://` prefix, name chars incl. `:` | codex-rs/skills/src/mentions.rs:39-41 | VERIFIED-IN-SOURCE |
| 19 | Structured `UserInput::Skill` resolved by path first; plain names only if unambiguous | codex-rs/skills/src/selection.rs:35-63 | VERIFIED-IN-SOURCE |
| 20 | Disabled paths excluded from counting and selection | name_counts.rs; selection.rs:24, :29 | VERIFIED-IN-SOURCE |
| 21 | `/skills` opens a menu | tui/src/chatwidget/slash_dispatch.rs:527; slash_command.rs:117 | VERIFIED-IN-SOURCE |
| 22 | "Do not carry skills across turns unless re-mentioned" | ext/skills/src/catalog_prompt.rs:8, :25 | VERIFIED-IN-SOURCE |
| 23 | Catalogue role `developer` / `skills.catalog` / `<skills_instructions>` | ext/skills/src/fragments.rs:47-61; protocol.rs:134 | VERIFIED-IN-SOURCE |
| 24 | Selected skill role `user` / `skills.selected_skill_instructions` / `<skill>` | fragments.rs:92-106 | VERIFIED-IN-SOURCE |
| 25 | "Skill instructions are user prompt input (not system prompt input)" | developers.openai.com/api/docs/guides/tools-skills | DOCUMENTED |
| 26 | Budget constants 8_000 / 10_000 / 8_000 / 2% / 1_024 / 256 / 1_024 | ext/skills/src/render.rs:23-33 | VERIFIED-IN-SOURCE |
| 27 | 2% of context window, or 8,000 chars when unknown | developers.openai.com/codex/skills | DOCUMENTED |
| 28 | Omission marker `- {n} additional skill(s) omitted…` | ext/skills/src/render.rs:1183 | VERIFIED-IN-SOURCE |
| 29 | Three catalogue intro/locator modes + skill-roots table | catalog_prompt.rs:3-7, :24-25 | VERIFIED-IN-SOURCE |
| 30 | `agents/openai.yaml` field set and 25–64 char `short_description` | skills/src/assets/samples/skill-creator/references/openai_yaml.md:9-47 | VERIFIED-IN-SOURCE |
| 31 | `dependencies.tools[].type` only `mcp`; `allow_implicit_invocation` | same:36-47 | VERIFIED-IN-SOURCE |
| 32 | Dependency/interface length caps 64/1024 | skills/src/metadata.rs:11-16; interface.rs:10-11 | VERIFIED-IN-SOURCE |
| 33 | Icon must be relative, under `assets/`, `..` under plugin assets | skills/src/interface.rs:88-144 | VERIFIED-IN-SOURCE |
| 34 | Bundled samples embedded by `include_dir!`; `.system` cache dir | skills/src/lib.rs:58-72 | VERIFIED-IN-SOURCE |
| 35 | Marker salt `v1` | skills/src/lib.rs:63, :119 | VERIFIED-IN-SOURCE |
| 36 | `quick_validate.py` allow-list excludes `compatibility` | samples/skill-creator/scripts/quick_validate.py:40 | VERIFIED-IN-SOURCE |
| 37 | Name regex `^[a-z0-9-]+$`, ≤64; description ≤1024 | quick_validate.py:61, :71, :87 | VERIFIED-IN-SOURCE |
| 38 | Installer dest `$CODEX_HOME/skills/<name>`, aborts if exists | samples/skill-installer/SKILL.md | VERIFIED-IN-SOURCE |
| 39 | Plugin marketplaces, cache path, `onboardingSkill` | developers.openai.com/plugins/build/plugins | DOCUMENTED |
| 40 | `skills.list` / `skills.read` / `skills.installations`, response cap 512 KiB | ext/skills/src/tools/mod.rs:54-55 | VERIFIED-IN-SOURCE |
| 41 | "Path to SKILL.md, not its containing directory" | exec-server-protocol/src/capabilities.rs | VERIFIED-IN-SOURCE |
| 42 | TrustedSkillRoots = `codex_home/skills` + `~/.agents/skills`; 16/512/2048 caps | ext/guardian-v2/…/trusted_skills.rs:10-22 | VERIFIED-IN-SOURCE |
| 43 | Repo skills excluded from trusted roots | same file (root list) | INFERRED |
| 44 | Script exec inherits shell approval policy | invocation.rs:81, :103, :63 + no skills sandbox found | INFERRED |
| 45 | "Treat Skills as privileged code", "Don't expose an open Skills repository" | developers.openai.com/api/docs/guides/tools-skills | DOCUMENTED |
| 46 | `SKILL.json` referenced only in doc comments, zero implementation | app-server-protocol/.../v2/plugin.rs:461-511 | UNKNOWN |
| 47 | No per-skill update mechanism | grep of skills crates | UNKNOWN |
