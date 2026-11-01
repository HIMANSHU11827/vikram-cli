# Hermes Agent Skills System — Complete Analysis

Research target: how Nous Research's Hermes Agent implements skills, for porting into VIKRAM-CLI (Rust).
Evidence base: a real local install at `/home/himanshu/.hermes` including the full upstream source
checkout at `/home/himanshu/.hermes/hermes-agent` (git repo, Python), plus three official doc pages.

Labels used throughout:
- **DOCUMENTED** — official docs (`hermes-agent.nousresearch.com/docs`), fetched and read.
- **VERIFIED-IN-SOURCE** — read in the local source tree or on-disk state; path:line cited.
- **INFERRED** — reasoning from verified facts, not stated anywhere.
- **UNKNOWN** — not found in docs or source.

Upstream implementation language is Python (not Rust). Every "tool" below is a Python function
registered into an agent tool registry; nothing here is a network service except the Skills Hub.

---

## 1. Architecture

### 1.1 Representation
A skill is a **directory** containing `SKILL.md` (YAML frontmatter + markdown body) plus optional
support dirs. **VERIFIED-IN-SOURCE** — `tools/skills_tool.py:2-5`:
> "A skill is a directory holding SKILL.md (YAML frontmatter + instructions) plus optional references/, templates/, assets/, scripts/."

Support dirs are a closed set. **VERIFIED-IN-SOURCE** — `agent/skill_utils.py:30-32`:
> `SKILL_SUPPORT_DIRS = frozenset(("references", "templates", "assets", "scripts"))`

Excluded from every scan. **VERIFIED-IN-SOURCE** — `agent/skill_utils.py:24-28`: `.git`, `.github`,
`.hub`, `.archive`, `.curator_backups`, `.locks`, `.venv`, `venv`, `node_modules`, `site-packages`,
`__pycache__`, `.tox`, `.nox`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`.

Frontmatter parsing is deliberately forgiving. **VERIFIED-IN-SOURCE** — `agent/skill_utils.py:67-87`:
strips a UTF-8 BOM, requires a leading `---` and a closing `\n---\n`, and on YAML error **falls back
to naive `key: value` line splitting** rather than failing the skill.

### 1.2 Discovery
One shared root ordering feeds list, prompt index, slash commands, `skill_view`, preload and cron.
**VERIFIED-IN-SOURCE** — `agent/skill_utils.py:398-401`:
> "Cross-directory precedence, lowest tier wins: trusted project > local profile > skills.create_dir >
> skills.external_dirs."
```
TIER_PROJECT, TIER_LOCAL, TIER_CREATE_DIR, TIER_EXTERNAL = range(4)
```
Roots are assembled in that order with dedupe (`get_skill_search_roots`, `agent/skill_utils.py:409-420`).

Index-file iteration (`iter_skill_index_files`, `agent/skill_utils.py:859`) is the single walker:
it rglobs `SKILL.md`, skipping `EXCLUDED_SKILL_DIRS` and support dirs that sit under a directory
already containing a `SKILL.md`. A `skills/scripts/foo` dir with no `SKILL.md` above it stays
discoverable — a deliberate carve-out (`agent/skill_utils.py:41-52`).

### 1.3 Registration
There is no registry file. A skill is "registered" by existing on disk in a scanned root.
**DOCUMENTED** (work-with-skills): "Drop it in `~/.hermes/skills/` and it's live." Plugin skills are
the exception: they live in a plugin-manager registry, addressed as `plugin:skill`
(`tools/skills_tool.py:320-361`).

### 1.4 Indexing
Two indexes exist:
1. **Per-session catalog cache** in `tools/skills_tool.py:33-41` — `_SKILLS_CACHE` keyed by a
   stat-based directory signature, `_SKILLS_CACHE_TTL_SECONDS = 30.0`, depth-bounded walk
   (`_SIGNATURE_MAX_DEPTH = 6`). The signature includes child dir *names*, not just mtimes, because
   "an install and an uninstall inside one coarse mtime tick … leave every mtime equal"
   (`tools/skills_tool.py:50-53`).
2. **Prompt snapshot** `~/.hermes/.skills_prompt_snapshot.json` (`_SKILLS_SNAPSHOT_VERSION = 4`),
   validated against a file-signature manifest of every `SKILL.md` and `DESCRIPTION.md`.
   **VERIFIED-IN-SOURCE** — `agent/prompt_builder.py:1091-1139`.

### 1.5 Loading (three levels)
**DOCUMENTED** (features/skills):
```
Level 0: skills_list()           → [{name, description, category}, ...]   (~3k tokens)
Level 1: skill_view(name)        → Full content + metadata
Level 2: skill_view(name, path)  → Specific reference file
```

### 1.6 Activation
Activation is an explicit `skill_view` call or a slash-command expansion — not an auto-injection.
Skill content arrives as a **user-message block**, not as system-prompt text, which is why
`/reload-skills` and new installs never invalidate the prompt cache. **VERIFIED-IN-SOURCE** —
`agent/skill_commands.py:43-58` describes a `/skill` turn being "expanded" into a scaffold message
with markers including `_BUNDLE_MARKER = " skill bundle,"`.

### 1.7 Invocation
- Slash: `/<skill-name> [instruction]`, stacking up to 5 leading skill tokens.
  **VERIFIED-IN-SOURCE** — `_MAX_STACKED_SKILLS = 5` (`agent/skill_commands.py:832`); parse loop at
  `843-867`; stop condition is the first non-skill token.
- Tool: `skill_view(name=..., file_path=...)` (`tools/skills_tool.py:712-729`).

### 1.8 Execution
Skills are **text**. Hermes executes nothing from a skill except two opt-in preprocessing features:
`${HERMES_SKILL_DIR}` / `${HERMES_SESSION_ID}` substitution, and inline `` !`cmd` `` snippets (off by
default). **VERIFIED-IN-SOURCE** — `agent/skill_preprocessing.py:15-21`:
```
_SKILL_TEMPLATE_RE = re.compile(r"\$\{(HERMES_SKILL_DIR|HERMES_SESSION_ID)\}")
_INLINE_SHELL_RE = re.compile(r"!`([^`\n]+)`")
_INLINE_SHELL_MAX_OUTPUT = 4000
```
**DOCUMENTED** (creating-skills): inline shell is gated by `skills.inline_shell: true` with
`skills.inline_shell_timeout` (default 10s per snippet), and "community hub installs never
auto-execute snippets".

Bundled `scripts/` are run by the agent through the normal `terminal` tool.

### 1.9 Storage
Everything is files under `HERMES_HOME` (default `~/.hermes`, per-profile override possible):
| Path | Purpose | Evidence |
|---|---|---|
| `skills/<cat>/<name>/SKILL.md` | the skill | VERIFIED `tools/skills_tool.py:88-97` |
| `skills/.bundled_manifest` | `name:hash` origin hashes | VERIFIED `tools/skills_sync.py:38` + real file |
| `skills/.hub/lock.json` | hub provenance | VERIFIED `tools/skills_hub.py:322-370` |
| `skills/.hub/taps.json` | custom GitHub sources | VERIFIED `tools/skills_hub.py:386-389` |
| `skills/.hub/audit.log` | hub audit trail | VERIFIED `tools/skills_hub.py:65` |
| `skills/.hub/quarantine/` | pre-install staging | VERIFIED `tools/skills_hub_install.py:60-67` |
| `skills/.hub/index-cache/` | 1h hub index cache | VERIFIED `INDEX_CACHE_TTL = 3600`, `skills_hub.py:44` |
| `skills/.usage.json` | usage telemetry/lifecycle | VERIFIED `tools/skill_usage.py:1-6` |
| `skills/.curator_state` | curator scheduler | VERIFIED `agent/curator.py:40-41` |
| `skill-bundles/<slug>.yaml` | bundles | VERIFIED `agent/skill_bundles.py:29-32` |
| `.skills_prompt_snapshot.json` | prompt index cache | VERIFIED `agent/prompt_builder.py:1096-1097` |
| `cache/project_skill_scans/` | project scan cache | VERIFIED `agent/skill_utils.py:645` |
| `pending/skills/` | staged approval writes | DOCUMENTED only |

Real install confirms the layout: `~/.hermes/skills/.hub/` contains `index-cache/`, `quarantine/`,
`scan-cache/`, `.ignore`, `audit.log`, `lock.json`, `taps.json` — **VERIFIED-IN-SOURCE** (directory read).

### 1.10 Runtime/tool integration
Three agent tools, one toolset named `skills`. **VERIFIED-IN-SOURCE** — `tools/skills_tool.py:731-734`,
`763-765`, `tools/skill_manager_tool.py:925-928`:
```
registry.register(name="skills_list", toolset="skills", schema=SKILLS_LIST_SCHEMA, ...)
registry.register(name="skill_view",  toolset="skills", schema=SKILL_VIEW_SCHEMA, ...)
registry.register(name="skill_manage",toolset="skills", schema=SKILL_MANAGE_SCHEMA, ...)
```
The main agent prompt gets a compact `<available_skills>` block built by
`build_skills_system_prompt` (`agent/prompt_builder.py:1244-1276`).

### 1.11 Config
All under the `skills:` key of `config.yaml`. **VERIFIED-IN-SOURCE / DOCUMENTED**:
`skills.disabled` (list), `skills.platform_disabled.<platform>` (`hermes_cli/skills_config.py:37-53`),
`skills.external_dirs`, `skills.create_dir`, `skills.trusted_project_dirs`, `skills.project_discovery`,
`skills.write_approval`, `skills.guard_agent_created`, `skills.template_vars`, `skills.inline_shell`,
`skills.inline_shell_timeout`, `skills.tier1_advisory`, `skills.config.*`, `skills.creation_nudge_interval`.

### 1.12 State
Three state machines coexist:
- **Catalog status** per resolved name: `unique` | `ambiguous` | `shadowed`
  (`agent/skill_utils.py:469-504`).
- **Lifecycle** per skill in `.usage.json`: `active -> stale -> archived` (moved to `.archive/`),
  with an orthogonal `pinned` flag (`tools/skill_usage.py:35`, `agent/curator.py:1-7`).
- **Quarantine** per skill dir: boolean, cached in-process (`agent/skill_utils.py:628`).

### 1.13 Error handling
Errors surface as structured JSON, never exceptions to the model. `skills_list` returns
`{"success": false}` via `tool_error(...)` (`tools/skills_tool.py:316-317`); `skill_view` wraps all
failures (`693-694`). A parse failure never kills a listing: the scan catches `UnicodeDecodeError`,
`PermissionError` and generic exceptions and just skips the file (`252-268`). A broken bundle is
logged and skipped so "a broken bundle can't break discovery" (`agent/skill_bundles.py:53-62`).

---

## 2. Skill sources

| Source | Discovered how | Trust | Enable/update/remove |
|---|---|---|---|
| **Bundled** (repo `skills/`) | Seeded into `~/.hermes/skills/` at install/update | `builtin` — never scanned | hash-manifest sync; `hermes skills reset` |
| **Official optional** (repo `optional-skills/`) | Only via hub browse/install | `official` — "builtin trust, no third-party warning" | hub install/uninstall |
| **Hub / external registries** | `skills search/browse/inspect` | per-source: `trusted` / `community` | install → scan → lock entry |
| **Installed from URL** | direct `https://.../SKILL.md` | always `community` | re-fetched by `skills update` from stored URL |
| **GitHub taps** | `tap add owner/repo`, default path `skills/` | `community` unless in `TRUSTED_REPOS` | `tap list/add/remove` |
| **User-created** | drop a dir in `~/.hermes/skills/` | user-owned, unmanaged | manual |
| **Agent-created** | `skill_manage(create)` | `agent-created` policy row | curator lifecycle |
| **Project / repo-local** | `<root>/.hermes/skills`, `<root>/.agents/skills` | requires `skills.trusted_project_dirs` | `hermes skills trust/untrust` |
| **External / shared dirs** | `skills.external_dirs` | same as local, writable | edit config |
| **Plugin-provided** | plugin manager registry, `plugin:skill` | plugin enablement | plugin lifecycle |
| **Third-party marketplaces** | clawhub, lobehub, browse-sh, skills.sh, well-known | `community` | hub install |

Evidence: bundled/optional dirs **VERIFIED** `tools/skills_sync.py:75-81`; project subdirs
`PROJECT_SKILLS_SUBDIRS = (".hermes/skills", ".agents/skills")` at `agent/skill_utils.py:513`; taps
**VERIFIED** `tools/skills_hub.py:386-389`; trust table **DOCUMENTED** (features/skills, "Trust levels").

Notably, `official` installs are recorded with `"trust_level": "builtin"` in the real lock file —
**VERIFIED-IN-SOURCE**, `~/.hermes/skills/.hub/lock.json:4-8`:
> `"source": "official", "trust_level": "builtin", "scan_verdict": "dangerous"`

A `dangerous` verdict coexisting with `builtin` trust shows the scan is *recorded*, not *enforced*,
for builtin trust — `INSTALL_POLICY["builtin"] = ("allow","allow","allow")` (`tools/skills_guard.py:28`).

---

## 3. Directory structure

```
skills/                        # single source of truth
├── <category>/                # 1..n path segments; category = "/".join(parts[:-2])
│   └── <skill>/
│       ├── SKILL.md           # REQUIRED
│       ├── references/        # optional, loaded on demand
│       ├── templates/         # optional
│       ├── scripts/           # optional
│       ├── assets/            # optional
│       └── examples/          # documented, NOT in SKILL_SUPPORT_DIRS
├── <category>/DESCRIPTION.md  # category description (frontmatter `description:`)
├── .hub/                      # hub state
└── .bundled_manifest
```
- **Required:** `SKILL.md` only. **DOCUMENTED** (features/skills) + enforced at write time
  (**VERIFIED** `tools/skill_manager_tool.py:153-178`: must start with `---`, must have `name` and
  `description`, must have a non-empty body after the frontmatter).
- **`examples/` gap:** docs list `examples/` as a support dir, but `SKILL_SUPPORT_DIRS` omits it
  (`agent/skill_utils.py:32`), and `ALLOWED_SUBDIRS` for `skill_manage write_file` omits it too
  (`tools/skill_manager_tool.py:113`). **INFERRED:** `examples/` is readable via `skill_view`'s
  linked-file serving but is not a first-class support dir; a nested `examples/SKILL.md` would be
  indexed as its own skill.
- **Category derivation:** `category = "general" if len(parts) < 2 else "/".join(parts[:-2]) if
  len(parts) > 2 else parts[0]` (**VERIFIED** `agent/prompt_builder.py:1152`).
- **Naming:** `VALID_NAME_RE = ^[a-z0-9][a-z0-9._-]*$`, `MAX_NAME_LENGTH = 64`
  (**VERIFIED** `tools/skill_manager_tool.py:108-115`). Categories must be a single dir name —
  `/` and `\` rejected (`133-143`).
- **Path resolution:** names resolve by three aliases — declared frontmatter name, directory name,
  and path relative to root (**VERIFIED** `agent/skill_utils.py:480`).
- **Validation:** two independent validators — `_validate_frontmatter` at write time (above) and the
  advisory linter (`tools/skill_linter.py`).

---

## 4. Metadata (frontmatter fields)

Authoritative union of docs + source. Required = `name`, `description` only.

| Field | Req | Meaning | Evidence |
|---|---|---|---|
| `name` | yes | identity; falls back to dir name when absent | VERIFIED `skills_tool.py:259`, `646` |
| `description` | yes | routing text in the index; truncated to 60 | VERIFIED `skill_utils.py:839` |
| `version` | no | informational string; **no resolution logic found** | DOCUMENTED only |
| `author`, `license` | no | informational | DOCUMENTED (creating-skills) |
| `platforms` | no | `[macos, linux, windows]` OS gate | VERIFIED `skill_utils.py:22, 90-107` |
| `environments` | no | `kanban`/`docker`/`s6` relevance gate, **not** compat | VERIFIED `skill_utils.py:110-170` |
| `requires_apps` | no | fail-closed platform-app gate | VERIFIED `skill_utils.py:173-192` |
| `compatibility` | no | agentskills.io field, echoed back by `skill_view` | VERIFIED `skills_tool.py:688-689` |
| `deps` | no | package-manager deps, `pm.ensure()` on load | VERIFIED `skills_tool.py:650-675` |
| `required_environment_variables[]` | no | `.name/.prompt/.help/.required_for` | DOCUMENTED |
| `required_credential_files[]` | no | `.path/.description`, mounted into sandboxes | DOCUMENTED |
| `prerequisites.commands`, `.env_vars` | no | legacy alias kept for compat | VERIFIED (`fastmcp/SKILL.md:13-14`), DOCUMENTED |
| `metadata.hermes.tags` | no | list or string, parsed by `_parse_tags` | VERIFIED `skills_tool.py:639-640` |
| `metadata.hermes.related_skills` | no | idem | VERIFIED same lines |
| `metadata.hermes.category` | no | display category | DOCUMENTED |
| `metadata.hermes.homepage` | no | informational | VERIFIED `fastmcp/SKILL.md:11` |
| `metadata.hermes.requires_toolsets` / `requires_tools` / `fallback_for_toolsets` / `fallback_for_tools` | no | conditional activation | VERIFIED `prompt_builder.py:1179-1197` |
| `metadata.hermes.config[]` | no | `.key/.description/.default/.prompt`, stored in `skills.config` | DOCUMENTED |
| `metadata.hermes.blueprint` | no | `.schedule/.deliver/.prompt/.no_agent` marks a runnable automation | DOCUMENTED |
| `metadata.hermes.session_platforms` | no | gateway-channel gate | VERIFIED `prompt_builder.py:1185-1187` |
| `org_id`, `org_author` | no | provenance, snapshot v2 | VERIFIED `prompt_builder.py:1091-1092` |
| `created_by` | no | `agent` marker written by `skill_manage` | VERIFIED `skill_usage.py:4-5` |

**Unknown fields:** silently ignored. Nothing rejects extras. **VERIFIED-IN-SOURCE** — frontmatter is
parsed into a dict and only known keys are read; `skill_manage` validation checks only that `name`
and `description` are *present* (`tools/skill_manager_tool.py:164-166`). Contrast with the tool
*schema*, where `additionalProperties: false` rejects an unknown key in the **call**, not in the
frontmatter (`tools/skill_manager_tool.py:857`).

**Description constraint is asymmetric.** New skills must fit 60 chars; edits are exempt.
**VERIFIED-IN-SOURCE** — `tools/skill_manager_tool.py:170-176`:
> "Description is {n} chars — new skills must fit the 60-char system-prompt budget … edit/patch skip
> it so existing over-limit skills stay maintainable."

---

## 5. Discovery & loading

**How names/descriptions reach the agent.** A compact text block, rendered per category.
**VERIFIED-IN-SOURCE** — `agent/prompt_builder.py:1343-1354`:
```
  {category}: {cat_desc}
    - {name}: {desc}
```
Project skills are prefixed `[project]` (`1314-1315`). Demoted categories collapse to
`{category} [names only]: a, b, c` — "NEVER drop entries — agent-created skills are the model's
project memory and it won't rediscover them via skills_list" (`1328-1330`).

**Selection.** The model decides, using the description. There is no embedding search and no router.
`skills_list` exists for a bulk re-read but the index is already in the prompt.

**Full instruction load.** `skill_view` returns `content` plus `linked_files`, `tags`,
`related_skills`, `path`, `skill_dir`, readiness fields, optional `metadata`, `compatibility`
(**VERIFIED** `tools/skills_tool.py:677-692`).

**Resource load.** Second call with `file_path`, path-validated against the skill dir
(`skills_tool.py:632-635`) and served by `_serve_skill_file`.

**Multiple skills.** Slash stacking up to 5 (`skill_commands.py:832`), or bundles (§9).

**Context management.** Three guards:
- 30s catalog TTL + directory signature (`skills_tool.py:33-41`).
- Prompt snapshot avoids a cold `os.walk` (32-entry LRU, `prompt_builder.py:1088-1089`).
- Repeat-view dedup returns a stub for a byte-identical repeat of the same skill in one session,
  cleared on compression (`skills_tool.py:737-759`).

**Mid-conversation changes.** A per-turn note behind the cached prefix.
**VERIFIED-IN-SOURCE** — `agent/skills_index_delta.py:26-29`:
> `[System: The skills available in this conversation changed after its system prompt was built.`
> … `Now available (load with skill_view(name) when relevant, …)` / `No longer available:`

**Duplicates.** Same tier, different content, same name → both listed by exact relative path and the
bare name refuses to load. **VERIFIED-IN-SOURCE** — `AMBIGUOUS_SKILL_PREFIX = "Ambiguous skill name "`
(`agent/skill_utils.py:403`). Byte-identical copies (or a symlink) resolve to the shallowest path,
decided by realpath equality or SHA-256 equality (`provably_same_skill`, `429-438`).

**Contradiction found:** the guide says "Plugin skills are **not** listed in the system prompt and
don't appear in `skills_list`" (work-with-skills). The source contradicts this — plugin skills are
explicitly injected into `<available_skills>` under category `plugin`
(`agent/prompt_builder.py:1200-1229`, `1462-1469`) and appended by `skills_list`
(`tools/skills_tool.py:295-302`). Doc page appears stale. **VERIFIED-IN-SOURCE** on the code side.

---

## 6. Management operations

### 6.1 Agent-facing tools (exact shapes)

`skills_list` — **VERIFIED-IN-SOURCE** `tools/skills_tool.py:697-710`:
```json
{ "name": "skills_list",
  "parameters": { "type": "object",
    "properties": { "category": {"type": "string",
      "description": "Optional category filter to narrow results"} },
    "required": [] } }
```
Returns `{success, skills:[{name,description,category}], categories, count, hint}` (`312-315`).

`skill_view` — **VERIFIED-IN-SOURCE** `tools/skills_tool.py:712-729`: params `name` (required),
`file_path` (optional). Response keys at `677-692`.

`skill_manage` — **VERIFIED-IN-SOURCE** `tools/skill_manager_tool.py:862-919`. One advertised shape,
an `operations` array; five action branches via `anyOf`:
| action | required slots |
|---|---|
| `create` | `content` (+ optional `category`) |
| `patch` | `old_string`, `new_string` (+ `replace_all`, `file_path`) |
| `patch` (rewrite) | `content` — "Full SKILL.md rewrite (REPLACES the whole file; last resort)" |
| `write_file` | `file_path`, `file_content` |
| `remove_file` | `file_path` |
| `delete` | — (+ `absorbed_into` for curator consolidation) |
`.additionalProperties: false` per branch; the legacy flat shape is "also accepted, never advertised"
(`915-916`). Unknown action error, verbatim (`808-809`):
> "Unknown action '{action}'. Use: create, edit, patch, delete, write_file, remove_file"

Note the asymmetry: the error text advertises `edit`, but the schema only advertises `patch`
(`edit` is the legacy alias, per DOCUMENTED features/skills).

### 6.2 CLI surface
**VERIFIED-IN-SOURCE** — `hermes_cli/subcommands/skills.py`:
`trust`, `untrust`, `browse` (`--page --size --source`), `search <query> --source --limit --json`,
`install <id> --category --name --force`, `inspect`, `list --source {all,hub,builtin,local}`,
`check`, `update`, `audit`, `uninstall`, `reset [--restore --yes]`, `list-modified`, `diff`,
`opt-out`, `opt-in`, `repair-official`, `publish --to --repo`, `snapshot export|import`,
`tap list|add|remove`, `config` (interactive enable/disable).
Source choices (`:15-17`): `all, official, skills-sh, well-known, github, clawhub, lobehub,
browse-sh, nvidia, openai, anthropic, huggingface, voltagent, gstack, minimax`.

In-chat equivalents: `/skills <same>`, plus `/reload-skills` (`hermes_cli/commands.py:303-304`,
`cli.py:1141`).

### 6.3 Guardrails
**VERIFIED-IN-SOURCE** — `tools/skill_manager_tool.py:108-115`:
```
MAX_NAME_LENGTH = 64
MAX_DESCRIPTION_LENGTH = 1024
MAX_SKILL_CONTENT_CHARS = 100_000   # ~36k tokens
MAX_SKILL_FILE_BYTES = 1_048_576    # 1 MiB per supporting file
ALLOWED_SUBDIRS = {"references", "templates", "scripts", "assets"}
```
Path traversal is blocked before lookup (`skill_lookup_path_error`, `tools/skills_tool.py:104-115`),
including Windows drive paths because `:` is the plugin separator. Mutations run under a per-skill
lock (`_skill_mutation_lock`, `skill_manager_tool.py:796`) and are captured to an append-only JSONL
ledger *before* the write (`skill_ledger.capture_before`, `800-807`) — telemetry, explicitly not a
gate.

### 6.4 Linter (advisory, never blocks)
**VERIFIED-IN-SOURCE** — `tools/skill_linter.py:40-56`:
```
_FORBIDDEN_FILES = ("README.md","CHANGELOG.md","install.sh",".env",".env.example",".gitignore")
_INCIDENT_REF_MIN = 4 ; _INCIDENT_REF_PER_KCHAR = 0.5
_MAX_REFERENCE_FILES = 60
_BODY_SOFT_BUDGET_CHARS = 24_000
```
Rules: `oversized-body`, `incident-log-shape`, `references-sprawl`, plus a load-bearing-section check
for "When to Use" (`:42-44`). **DOCUMENTED:** "They warn; they never block a write."

### 6.5 Other ops
- **enable/disable:** `skills.disabled` + `skills.platform_disabled.<platform>`
  (`hermes_cli/skills_config.py:37-53`); disable matches declared name *or* load name
  (`is_disabled_entry`, `agent/skill_utils.py:462-466`).
- **export/import:** `hermes skills snapshot export|import` (CLI parser `:155-165`).
- **update:** `check` compares stored identifier + current upstream content hash.
  **DOCUMENTED:** locally-edited skills are skipped unless `--force`.
- **restore:** `reset` (re-baseline manifest) / `reset --restore` (delete + re-copy bundled).

---

## 7. Skills Hub

**Source abstraction.** Every backend implements one ABC. **VERIFIED-IN-SOURCE** —
`tools/skills_hub_models.py:127-154`: `search(query, limit) -> list[SkillMeta]`,
`fetch(identifier) -> Optional[SkillBundle]`, `inspect(identifier) -> Optional[SkillMeta]`,
`source_id()`, `trust_level_for()`, `current_revision()`.

`SkillMeta` (`27-37`): `name, description, source, identifier, trust_level, repo, path, tags, extra`.
`SkillBundle` (`41-48`): `name, files: dict[rel_path -> str|bytes], source, identifier, trust_level,
metadata`.

**Search.** Fan-out over sources with a cached central index and a bounded fallback.
**VERIFIED-IN-SOURCE** — `tools/skills_hub_search.py`: `create_source_router`, `parallel_search_sources`,
`unified_search`, `_fan_out`, `_index_miss_fallback_sources`, `INDEX_CACHE_TTL = 3600`.
**DOCUMENTED:** the fallback pass "gets at most 8 seconds of the search budget".

**Install pipeline** (verified order):
1. `quarantine_bundle(bundle)` writes to `skills/.hub/quarantine/<name>`
   (`tools/skills_hub_install.py:60-67`).
2. `scan_skill` → `ScanResult` (regex scan, `SCANNER_VERSION = "skills-guard-v10"`).
3. `should_allow_install(result, force)` → `(allowed, reason)` (`tools/skills_guard.py:819-831`).
4. `install_from_quarantine(...)` re-validates the source path is inside the quarantine dir, then
   `shutil.move` into the skills tree (`skills_hub_install.py:143-186`).
5. `HubLockFile.record_install(...)` writes source, identifier, trust_level, scan_verdict,
   content_hash, install_path, files, metadata, scan_provenance, timestamps
   (`tools/skills_hub.py:335-370`).

**Trust policy.** **VERIFIED-IN-SOURCE** — `tools/skills_guard.py:24-35`:
```
TRUSTED_REPOS = {"openai/skills", "anthropics/skills", "huggingface/skills", "NVIDIA/skills"}
INSTALL_POLICY = {  #            safe      caution    dangerous
  "builtin":       ("allow",  "allow",   "allow"),
  "trusted":       ("allow",  "allow",   "block"),
  "community":     ("allow",  "block",   "block"),
  "agent-created": ("allow",  "allow",   "ask"),
}
```
Severity mapping: `critical → dangerous`, `high → caution`, else `safe` (`:950-952`).
`--force` overrides caution/warn but never `dangerous` (`:823`, `:831`).

**Versions / updates.** The lock stores `content_hash`; there is **no semver resolution** —
`version:` in frontmatter is informational only. Update = re-fetch + hash compare.
**UNKNOWN / not found in docs or source:** any version constraint, range, or pinning mechanism.

**Cryptographic verification.** **UNKNOWN / not found in source.** Docs mention NVIDIA skills ship a
signed `skill.oms.sig` + `skill-card.md`, but `TRUSTED_REPOS` keys trust on the repo name string
(`skills_guard.py:946`), and no signature-verification call exists in the install path. Integrity =
SHA-256 content hash, not a signature. **INFERRED** from the absence of any verify call in
`skills_hub_install.py`.

**Dependencies.** Two unrelated notions, neither a registry-style dep graph:
- `deps: [ffmpeg]` → `pm.ensure()` at load time, failure is a note not a block
  (**VERIFIED** `tools/skills_tool.py:650-675`).
- `required_environment_variables` / `required_credential_files` → setup gating, docs only.
**UNKNOWN:** skill-depends-on-skill resolution. `related_skills` is display metadata.

**Failures / offline.** Corrupt state degrades instead of failing: a bad lock read returns EMPTY
(`_JsonStateFile._read`, `skills_hub.py:287-291`), an unusable file lock degrades to an unlocked update
(`299-314`). **DOCUMENTED:** `check` skips `orphaned` / `invalid_install` entries; no entry is removed
automatically. GitHub unauthenticated limit is 60 req/h, ~2 calls per install; set `GITHUB_TOKEN`.

**Uninstall.** `uninstall_skill(name)` (`skills_hub_install.py:205`) + `record_uninstall`.
The lock validates name and install-path *shape at write time* because "a poisoned lock entry is the
precondition for the uninstall_skill rmtree-escape" (`skills_hub.py:348-351`).

**Community / taps.** `~/.hermes/skills/.hub/taps.json` (`{taps:[{repo, path}]}`), default path
`skills/`, new taps are `community` trust; raising it requires adding to `TRUSTED_REPOS` via a core PR.
**DOCUMENTED.**

---

## 8. Project trust & precedence

**Precedence (one order, everywhere):** `project → local (~/.hermes/skills) → skills.create_dir →
external_dirs`. **VERIFIED-IN-SOURCE** — `agent/skill_utils.py:398-401`; consumed by
`get_skill_search_roots` (`409-420`) and re-documented at `agent/prompt_builder.py:1442-1444`.

**Trust gate.** Project skills are "a prompt-injection vector if auto-sourced from any clone"
(`agent/skill_utils.py:507-511`); they load only when the root is in `skills.trusted_project_dirs`.
`skills.project_discovery: false` disables scanning and the notice entirely (`:587`).
CLI: `hermes skills trust [path]` / `untrust [path]` (`subcommands/skills.py:28-36`).

**Project root:** nearest ancestor containing `.git` (dir *or* worktree file), max depth 64
(`_PROJECT_ROOT_MAX_DEPTH`, `:515`). Resolution prefers the session cwd over process cwd because a
multi-session host pins each session's workspace (`:518-528`).

**Scan-time quarantine.** Trust is repo-level and made once; content changes on every `git pull`, so
every project `SKILL.md` is scanned on index. `dangerous` → excluded from index, `skills_list`,
`skill_view` and slash commands. **VERIFIED-IN-SOURCE** — `agent/skill_utils.py:631-654`; cache at
`cache/project_skill_scans/` "never inside the repo"; failure mode is **fail-closed**
(`except Exception: … quarantined = True`, `:650-652`). Note `caution` still loads, matching hub behavior.

**Overrides/conflicts:** shadowing is silent except one log line per pairing
(`logger.warning("Skill '%s' at %s is shadowed by a higher-precedence copy …")`, `:498-499`), skipped
when the copies are provably identical. Same-tier name collision is *refused*, never guessed
(`:399-400`).

**Non-interactive surfaces** (cron, API, ACP) inherit the interactive trust decision and never prompt;
a job whose `workdir` is in an untrusted/undecided repo loads no project skills. **DOCUMENTED**;
consistent with `TERMINAL_CWD`-based resolution at `:518-528`.

---

## 9. Bundles

**Format** — `~/.hermes/skill-bundles/<slug>.yaml`; `name` optional (defaults to filename stem),
`skills` required non-empty list, `description` and `instruction` optional.
**VERIFIED-IN-SOURCE** — validation + skip reasons, `agent/skill_bundles.py:63-78`:
`"is not a mapping"`, `"has no name"`, `"has no skills list"`, `"has empty skills list"`,
`"yielded empty slug"`.

**Precedence:** "Bundles take precedence over individual skills when slugs collide." **DOCUMENTED.**
Duplicate bundle slugs keep the first alphabetically and warn (`scan_bundles`, `:96-100`).

**Missing members are non-fatal** — loaded ones proceed, the rest are listed in the header
(`build_bundle_invocation_message`, `:131-166`; `disabled` re-applied here because bundles bypass the
scan-time filter). Both bundle and stacked invocations share one scaffold header
(`_scaffold_header(..., lead_lines=[f"Bundle: {bundle_name}"])`, `:162-165`).

---

## 10. Advanced

**Agent-created skills / self-improvement.** `skill_manage` is described in-prompt as "your procedural
memory" (`skill_manager_tool.py:822-838`). The prompt itself instructs the agent to record non-trivial
workflows. **DOCUMENTED:** skill *entries* must be "lessons, not logs" — no PR numbers, dates,
incident narration; `references/` extended in place rather than one file per session.

**Approvals.** `skills.write_approval: true` stages every `skill_manage` write to
`~/.hermes/pending/skills/`, reviewed via `/skills pending|diff|approve|reject|approval on|off`.
**DOCUMENTED.** Implemented by `_apply_skill_write_gate(action, name, **args)`
(`skill_manager_tool.py:785`) — staging applies regardless of foreground or background origin
("skills are too large to review inline", `:780-781`).

**Curator (upstream maintenance of agent skills).** Idle-triggered, no cron daemon.
**VERIFIED-IN-SOURCE** — `agent/curator.py:1-7`:
> "only curator-managed skills are touched; never delete, only archive (recoverable); pinned skills
> bypass all auto-transitions."

Defaults (`:31-35`): `DEFAULT_INTERVAL_HOURS = 24*7`, `DEFAULT_MIN_IDLE_HOURS = 2`,
`DEFAULT_STALE_AFTER_DAYS = 14`, `DEFAULT_ARCHIVE_AFTER_DAYS = 30`, `DEFAULT_CONSOLIDATE = False`.
`PROTECTED_BUILTIN_SKILLS` is an **empty set** in this source tree (`tools/skill_usage.py:40`) and the
comment says "Keep tiny" — i.e. no builtin is currently hard-protected.

**Upstream sync / modification detection.** Origin-hash manifest, v2 `name:hash` lines.
**VERIFIED-IN-SOURCE** — `tools/skills_sync.py:1-7`:
> "EXISTING skills update only when bundled changed AND the user copy still matches the origin hash
> (else user-customized -> SKIP); user-DELETED skills are not re-added."

Runtime caches (`__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, sibling `.pyc`) are
excluded from the hash so running a helper script doesn't mark a skill user-modified. **DOCUMENTED.**

**Opt-out marker:** `NO_BUNDLED_SKILLS_MARKER = ".no-bundled-skills"` (`tools/skills_sync.py:72`).

**Security scanning summary.** One regex scanner (`skills-guard-v10`) with `Finding` records carrying
`pattern_id, severity, category, file, line, match, description` (`skills_guard.py:38-46`). Known gap,
stated by the authors (`:6-8`): language write APIs aimed at agent-config files "surface only the low
`*_ref` finding — static regexes cannot tie the call to a dynamic destination."

**Notifications / nudge.** `skills.creation_nudge_interval: 15` is present in the live config
(`~/.hermes/config.yaml:189`) — **VERIFIED-IN-SOURCE** as a config key; its exact semantics were not
read and are **UNKNOWN**.

---

## 11. Limitations (as stated or directly observable)

1. **No version resolution.** `version:` is never compared anywhere in the install/update path.
   **VERIFIED** by the lock schema (content hashes only, `skills_hub.py:355-367`).
2. **No signature verification** of hub bundles. **UNKNOWN / not found in source.**
3. **Regex-only scanning**, with a self-declared blind spot for dynamic write destinations
   (`skills_guard.py:6-8`).
4. **In-process caches can go stale for ≤30s** on in-place `SKILL.md` edits — "which no directory
   signature can see" (`skills_tool.py:36-37`).
5. **Ambiguity is a dead end by design**: two different same-name skills in one tier cannot be loaded
   by bare name, and the prompt emits a rename note (`prompt_builder.py:1336-1339`).
6. **Docs drift**: plugin skills *are* in the system prompt although the guide says otherwise (§5).
7. **`examples/` is documented but not a first-class support dir** (§3).
8. **No skill-to-skill dependency graph**; `related_skills` is metadata only. **UNKNOWN.**

---

## 12. Porting notes for a Rust implementation (INFERRED)

These are inferences from the verified mechanics, not statements by the Hermes authors.
- The load-bearing design is **three cheap tiers** (regex-free frontmatter read at scan, full read at
  view, per-file read on demand) plus a **stat-signature cache**. That maps cleanly onto
  `std::fs::metadata().modified()` + a `HashMap<PathBuf, (u64, Instant)>`.
- The one truly subtle rule is **tier-then-identity resolution**: lowest tier wins; within a tier,
  only *provably identical* copies (realpath or hash equal) may be collapsed. Reproduce this before
  adding anything else — it is the invariant every surface depends on.
- Fail-closed scanning for project skills is a security decision worth copying verbatim; fail-open
  would silently reintroduce the `git pull` injection vector.

---

## Evidence index

| Claim | Source |
|---|---|
| Skill = dir with SKILL.md + optional support dirs | `tools/skills_tool.py:2-5` |
| Tier order project > local > create_dir > external | `agent/skill_utils.py:398-401` |
| Excluded dirs list | `agent/skill_utils.py:24-28` |
| Support dirs = references/templates/assets/scripts | `agent/skill_utils.py:30-32` |
| Frontmatter fallback parsing | `agent/skill_utils.py:67-87` |
| Platform gate | `agent/skill_utils.py:22, 90-107` |
| Ambiguous-name refusal constant | `agent/skill_utils.py:403` |
| Shadowed warning text | `agent/skill_utils.py:498-499` |
| Project quarantine, fail-closed, cache dir | `agent/skill_utils.py:631-654` |
| Project subdirs `.hermes/skills`, `.agents/skills` | `agent/skill_utils.py:513` |
| 30s catalog TTL, depth 6 | `tools/skills_tool.py:33-41, 50-53` |
| `skills_list` schema + response | `tools/skills_tool.py:290-317, 697-710` |
| `skill_view` schema + response | `tools/skills_tool.py:592-694, 712-729` |
| Tool registration, toolset `skills` | `tools/skills_tool.py:731-734, 763-765` |
| `deps` → `pm.ensure` on load | `tools/skills_tool.py:650-675` |
| `skill_manage` operations schema | `tools/skill_manager_tool.py:862-919` |
| Limits 64/1024/100k/1MiB/ALLOWED_SUBDIRS | `tools/skill_manager_tool.py:108-115` |
| Frontmatter validation rules | `tools/skill_manager_tool.py:146-179` |
| 60-char desc budget for new skills | `tools/skill_manager_tool.py:170-176`; `agent/skill_utils.py:839` |
| Write-approval gate call site | `tools/skill_manager_tool.py:780-786` |
| Ledger pre-capture | `tools/skill_manager_tool.py:800-807` |
| Install policy matrix + trusted repos | `tools/skills_guard.py:24-35` |
| Severity→verdict mapping | `tools/skills_guard.py:950-952` |
| `--force` never overrides dangerous | `tools/skills_guard.py:823, 831` |
| Scanner self-declared gap | `tools/skills_guard.py:6-8` |
| Hub paths (lock/taps/audit/quarantine/index-cache) | `tools/skills_hub.py:61-71, 44` |
| Lock entry schema | `tools/skills_hub.py:335-370` |
| Degraded reads/locks | `tools/skills_hub.py:287-314` |
| Quarantine→install move | `tools/skills_hub_install.py:60-67, 143-186, 205` |
| SkillSource ABC, SkillMeta, SkillBundle | `tools/skills_hub_models.py:27-48, 127-154` |
| Hub search fan-out / TTL | `tools/skills_hub_search.py` (`unified_search`, `create_source_router`, `INDEX_CACHE_TTL`) |
| Bundle YAML schema + skip reasons | `agent/skill_bundles.py:53-104` |
| Bundle precedence / missing members | DOCUMENTED features/skills; `agent/skill_bundles.py:131-166` |
| Stacking limit 5 | `agent/skill_commands.py:832` |
| Prompt index rendering | `agent/prompt_builder.py:1320-1362` |
| `[project]` tag | `agent/prompt_builder.py:1314-1315` |
| Snapshot v4 + manifest | `agent/prompt_builder.py:1091-1139, 1470-1476` |
| DESCRIPTION.md → category descriptions | `agent/prompt_builder.py:1283-1294` |
| Conditional activation logic | `agent/prompt_builder.py:1179-1197` |
| Plugin skills in prompt index | `agent/prompt_builder.py:1200-1229, 1462-1469` |
| Mid-conversation delta note | `agent/skills_index_delta.py:1-33` |
| Template vars + inline shell + 4000 cap | `agent/skill_preprocessing.py:15-21` |
| Linter thresholds + forbidden files | `tools/skill_linter.py:40-56` |
| Usage sidecar + lifecycle states | `tools/skill_usage.py:1-6, 35-40` |
| Curator invariants + defaults | `agent/curator.py:1-7, 31-35` |
| Bundled sync origin-hash rule | `tools/skills_sync.py:1-7, 38, 72, 75-81` |
| CLI subcommands + source choices | `hermes_cli/subcommands/skills.py:15-17, 28-177` |
| `/reload-skills` | `hermes_cli/commands.py:303-304`; `cli.py:1141` |
| Progressive disclosure levels | DOCUMENTED features/skills (also guides/work-with-skills) |
| Trust levels table | DOCUMENTED features/skills |
| Frontmatter field catalogue | DOCUMENTED features/skills + developer-guide/creating-skills |
| `required_credential_files` | DOCUMENTED developer-guide/creating-skills |
| Blueprints / `/suggestions` | DOCUMENTED developer-guide/creating-skills |
| Live manifest format `name:hash` | `/home/himanshu/.hermes/skills/.bundled_manifest:1-15` |
| Live hub state dirs | `/home/himanshu/.hermes/skills/.hub/` (dir read) |
| Live lock entry incl. `trust_level: builtin` + `scan_verdict: dangerous` | `/home/himanshu/.hermes/skills/.hub/lock.json:4-40` |
| Real frontmatter with `prerequisites.commands` | `/home/himanshu/.hermes/skills/mcp/fastmcp/SKILL.md:1-15` |
| `skills` config block in a real profile | `/home/himanshu/.hermes/config.yaml:188-189` |

Docs fetched (HTTP 200, full page read):
`/docs/user-guide/features/skills`, `/docs/guides/work-with-skills`,
`/docs/developer-guide/creating-skills`.

### Research gaps (stated by the researcher, not hidden)
`write_approval` / `pending/skills/` internals are DOCUMENTED-only — the gate's call site
`_apply_skill_write_gate` was found but its implementation was not read; `skills.creation_nudge_interval`
semantics unknown; "no signature verification" is asserted from the absence of a verify call in
`skills_hub_install.py` — a verifier inside a source adapter would overturn it; the upstream repo may be
newer than the docs pages, which is what the plugin-skill contradiction suggests. Reading
`tools/skill_manager_guards.py` and the hub adapter classes in full would close the first and third gaps.
