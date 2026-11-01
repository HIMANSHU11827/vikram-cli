# OpenCode Skills — Complete Analysis

Compiled: 2026-11-01. **This document distinguishes OpenCode 1 from OpenCode 2
throughout, because the two differ in frontmatter requirements, tool input
field, discovery depth and permission schema.**

Labels: **DOCUMENTED** (fetched official docs) · **VERIFIED-IN-SOURCE** (real
local file, path cited) · **INFERRED** · **UNKNOWN**.

---

## 0. The version trap — read this first

The current docs site serves OpenCode 1 content at `/docs/*` and OpenCode 2 at
`/v2/docs/*`, with a banner: "NewOpenCode v2 is now available". **DOCUMENTED**
(E1, E2)

The permission pages state the rename explicitly: "V1 uses different field and
action names. In V2, use `permissions`, `shell`, and `subagent` instead of
`permission`, `bash`, and `task`." **DOCUMENTED** (E4)

| Dimension | OpenCode 1 | OpenCode 2 |
|---|---|---|
| Tool input | `skill({ name: "git-release" })` | `{ "id": "git-release" }` |
| ID source | frontmatter `name`, must match directory | file path |
| `name` in frontmatter | **required** | optional display label |
| `description` | **required**, 1–1024 chars | optional; without it, not advertised |
| Nested dirs | one level (`skills/<name>/SKILL.md`) | any depth ≤ 6 |
| Permission key | `permission.skill` map | `permissions` array, `action: "skill"` |
| Non-advertise switch | `tools.skill: false` | `metadata.opencode/autoinvoke: false` |

The machine under analysis runs **V2** — see Section 12.

`INFERRED`: publishing VIKRAM-CLI skill docs against `/docs/skills` alone will
produce instructions that silently fail on a V2 install, because V1's
"`name` must match the directory" is no longer how the ID is derived.

---

## 1. The native `skill` tool

How it is registered **DOCUMENTED** (E2, E3): there is no user-facing
registration step. OpenCode scans the discovery roots at startup and builds a
skill registry; the tool exposes that registry to the model. In V1 the docs
describe it as "the native `skill` tool — agents see available skills and can
load the full content when needed".

### 1.1 How skills are advertised

V1 shows the advertisement as XML inside the tool description **DOCUMENTED** (E2):

```xml
<available_skills>
  <skill>
    <name>git-release</name>
    <description>Create consistent releases and changelogs</description>
  </skill>
</available_skills>
```

V2 states the rule more precisely **DOCUMENTED** (E3): "At each model step,
OpenCode lists permitted skills that have a description and do not set
`opencode/autoinvoke` to `false`. The list includes only the ID, name, and
description, not the full Markdown body."

Consequences **DOCUMENTED** (E3):
- A skill with **no `description` is not advertised** at all (still loadable by
  ID if you know it).
- The list is recomputed per model step, so permission changes take effect
  without restart.
- Only metadata is in context; the body is not.

### 1.2 Input shape

| Version | Call |
|---|---|
| V1 | `skill({ name: "git-release" })` **DOCUMENTED** (E2) |
| V2 | `skill({ "id": "git-release" })` **DOCUMENTED** (E3) |

V2 also allows a human-side shortcut: "To load a skill yourself, mention it in
your prompt as `@skill-id`." **DOCUMENTED** (E3)

### 1.3 What a load returns — the return shape

V2 documents the four steps performed on load **DOCUMENTED** (E3):

1. Selects the current definition for that ID.
2. Checks the selected agent's `skill` permission.
3. "Adds the Markdown body, **without frontmatter**, to the conversation."
4. "Provides the skill's base directory and a sample of up to ten supporting
   file paths."

Additional documented detail **DOCUMENTED** (E3):
- "Supporting file contents are not loaded automatically. The agent reads them
  when the skill directs it to do so."
- The file sample exists only for **directory** skills; "flat Markdown skills do
  not receive a neighboring file list."
- "Paths written inside the skill are relative to the directory containing
  `SKILL.md`."

`INFERRED`: step 3 means the frontmatter is *stripped*, so a skill cannot smuggle
metadata into context by writing it in YAML — but equally, instructions placed in
frontmatter are lost.

### 1.4 Errors

V1 documents no error strings; its troubleshooting is a checklist
**DOCUMENTED** (E2): `SKILL.md` must be all caps, frontmatter must have `name`
and `description`, names must be unique, and `deny` skills are hidden.

V2 documents the *conditions* for a miss but still no message text
**DOCUMENTED** (E3): wrong filename shape, wrong case-sensitive path-derived ID,
missing description, `opencode/autoinvoke: false`, agent permission denied, a
later source defining the same ID, or a broken HTTP catalog.

**UNKNOWN**: the literal error payload returned by the `skill` tool when an ID
does not exist or is denied. Neither doc page states it, and there is no runtime
here to probe.

---

## 2. Discovery paths

V1 **DOCUMENTED** (E2) — six roots:

- project: `.opencode/skills/<name>/SKILL.md`
- global: `~/.config/opencode/skills/<name>/SKILL.md`
- project Claude-compat: `.claude/skills/<name>/SKILL.md`
- global Claude-compat: `~/.claude/skills/<name>/SKILL.md`
- project agent-compat: `.agents/skills/<name>/SKILL.md`
- global agent-compat: `~/.agents/skills/<name>/SKILL.md`

V1 traversal: "For project-local paths, OpenCode walks up from your current
working directory until it reaches the git worktree." **DOCUMENTED** (E2)

V2 **DOCUMENTED** (E3) — same four compatibility roots plus:

| Scope | Sources |
|---|---|
| Global | `~/.config/opencode/skills` |
| Global compat | `~/.claude/skills`, `~/.agents/skills` |
| Project | `.opencode/skills` |
| Project compat | `.claude/skills`, `.agents/skills` |

Plus two V2-only mechanisms **DOCUMENTED** (E3):

- **Explicit `skills` array** in any `opencode.json`/`opencode.jsonc`, accepting
  relative paths, `~/` paths, absolute paths, and `http(s)://` catalog URLs.
  "Every discovered config file contributes its entries. `skills` arrays are
  combined rather than replaced."
- **HTTP catalogs**: `<base>/index.json` listing
  `{name, version, files[]}`; files downloaded from `<base>/<name>/<file>`;
  increment `version` to bust the cache. Paths must be "safe, relative, and
  same-origin".

V2 path shapes and limits **DOCUMENTED** (E3):
- "Markdown files must be at the source root, such as `skills/review.md`."
- "Files named exactly `SKILL.md` can be nested in grouping directories."
- "A directory containing `SKILL.md` is one skill. OpenCode does not search
  inside it for more skills."
- "Hidden files and directories and `node_modules` are skipped."
- "Search stops six directories below the source root."

Project traversal in V2: "OpenCode searches from the current directory up to the
project root and includes matching directories at every level." **DOCUMENTED** (E3)

**Divergence:** V1's walk terminates at the *git worktree*; V2 says *project
root*. **DOCUMENTED** (E2 vs E3) — for a bare directory outside a repo these can
differ, and the docs do not define what happens in the non-repo case. **UNKNOWN**

---

## 3. Name collisions and precedence

**V1 has no documented precedence.** It only says: "Ensure skill names are
unique across all locations" — i.e. collisions are the author's problem.
**DOCUMENTED** (E2) → V1 behaviour on duplicate IDs is **UNKNOWN**.

**V2 defines a total order** **DOCUMENTED** (E3): "Skills are selected by ID. If
two sources define `git-release`, the source registered later supplies the skill
that OpenCode loads." Registration order, low → high:

1. Built-in skills
2. `.claude/skills` (global, then farthest ancestor → current dir)
3. `.agents/skills` (same order)
4. `~/.config/opencode/skills`
5. project `.opencode/skills` (project root → current dir)
6. explicit `skills` config entries (config priority, then array order)

Practical reading **INFERRED**: all six tiers are loaded and shadowed, not
merged, and there is **no warning** on collision — the docs only advise "Avoid
duplicate IDs unless you intend to override an earlier skill."

Important asymmetry **INFERRED** from (E3): `.claude/skills` is registered
*earlier*, so `~/.config/opencode/skills` wins over `.claude/skills`. Anyone
assuming "native beats compat" is wrong in V2.

---

## 4. Symlinks

**UNKNOWN for both versions.** Neither the V1 skills page (E2) nor the V2
skills page (E3) mentions symlinks, link resolution, or loop protection. There is
no statement that symlinked skill directories are followed, nor that they are
skipped.

`INFERRED`: symlinked roots plausibly work because discovery is described as a
directory walk that skips "hidden files and directories and `node_modules`" —
symlinks are not in the skip list (E3). Do not rely on this; verify on the target
build.

Contrast: Claude Code **does** document symlink behaviour explicitly (loads once
even if several locations point at the same target) — see
`claude-code-complete-analysis.md`, Section 4.

---

## 5. Permissions

### 5.1 V1 schema — `permission.skill` map

**DOCUMENTED** (E2):

```json
{ "permission": { "skill": {
  "*": "allow", "pr-review": "allow",
  "internal-*": "deny", "experimental-*": "ask"
} } }
```

| Effect | Behaviour |
|---|---|
| `allow` | "Skill loads immediately" |
| `deny` | "Skill hidden from agent, access rejected" |
| `ask` | "User prompted for approval before loading" |

Wildcards: `internal-*` matches `internal-docs`, `internal-tools`.
**DOCUMENTED** (E2)

Per-agent override, two forms **DOCUMENTED** (E2):
- custom agent, frontmatter: `permission:` → `skill:` → pattern map
- built-in agent, `opencode.json`: `agent.plan.permission.skill`

Disabling entirely **DOCUMENTED** (E2): `tools: { skill: false }` in agent
frontmatter, or `agent.plan.tools.skill: false` in JSON. "When disabled, the
`<available_skills>` section is omitted entirely."

### 5.2 V2 schema — `permissions` array

**DOCUMENTED** (E3, E4):

```jsonc
{ "permissions": [
  { "action": "skill", "resource": "*",             "effect": "allow" },
  { "action": "skill", "resource": "internal-*",    "effect": "deny"  },
  { "action": "skill", "resource": "experimental-*","effect": "ask"   }
] }
```

| Effect | Behaviour (V2) |
|---|---|
| `allow` | "Advertises and loads matching skills without approval" |
| `ask` | "Advertises matching skills and asks before loading them" |
| `deny` | "Hides matching skills from the model and rejects loading" |

Semantics **DOCUMENTED** (E4):
- `skill` action resource = "Skill ID".
- "Rules run in order, and the last matching rule wins."
- "If no rule matches, OpenCode uses `ask`." — but the shipped base policy opens
  with `{ "action": "*", "resource": "*", "effect": "allow" }`, so the effective
  default is allow.
- Agent rules are **appended after** global rules, not replacing them.
- "A custom subagent uses its own permissions, not a subset of its parent's
  permissions."
- Approvals persist: "Saved approvals are durable, project-scoped `allow` rules.
  They never override a configured `deny`."
- Policies can hard-deny after rules and approvals run: `{ "action": "tool.use",
  "resource": "shell:sudo *", "effect": "deny" }`.

Per-agent placement **DOCUMENTED** (E4): `agents.<id>.permissions` array, or in
an agent Markdown file's `permissions:` key.

---

## 6. Frontmatter handling

**V1** **DOCUMENTED** (E2):
- `name` (required), `description` (required), `license`, `compatibility`,
  `metadata` (string→string map).
- "Unknown frontmatter fields are ignored."
- `name`: 1–64 chars, lowercase alphanumeric with single hyphens, no leading or
  trailing `-`, no `--`, **"Match the directory name that contains
  `SKILL.md`"**, regex `^[a-z0-9]+(-[a-z0-9]+)*$`.
- `description`: 1–1024 chars.
- "OpenCode 1 does not use metadata to control slash commands or automatic skill
  selection."

**V2** **DOCUMENTED** (E3):
- "All frontmatter is optional at runtime."
- `name` = "Display name; defaults to the path-derived ID".
- `metadata.opencode/autoinvoke: false` hides the skill from the model's list.
- `disable-model-invocation: true` is accepted as an alias; "when both … are set,
  `opencode/autoinvoke` wins."
- "V2 accepts portability fields such as `license` and `compatibility` but does
  not interpret them."
- Enforcement explicitly dropped: "V2 currently does not enforce this pattern,
  the 1–64 character recommendation, a match between `name` and the directory,
  or a maximum description length."

**ID vs name — the single most misread rule** **DOCUMENTED** (E3):
> "The file path determines the skill ID. The frontmatter `name` is only a
> display label."

| File | ID |
|---|---|
| `<source>/git-release.md` | `git-release` |
| `<source>/git-release/SKILL.md` | `git-release` |
| `<source>/teams/release/SKILL.md` | `release` |

"IDs are exact and case-sensitive."

---

## 7. Agent integration

- Agents are Markdown files with frontmatter (`mode: subagent`, `description`,
  `model`, permissions). **VERIFIED-IN-SOURCE** (E5)
- Permissions merge: global rules first, agent rules appended last, last match
  wins. **DOCUMENTED** (E4)
- Visibility is per-agent: a `deny` hides the skill from *that* agent's
  `<available_skills>`; `ask` still advertises it. **DOCUMENTED** (E3)
- Subagents do not inherit the parent's restrictions — "A custom subagent uses
  its own permissions, not a subset of its parent's permissions."
  **DOCUMENTED** (E4)
- V1: disabling `tools.skill` omits the whole `<available_skills>` block.
  **DOCUMENTED** (E2)

---

## 8. Compatibility with Agent Skills / Claude skills

- OpenCode reads `.claude/skills` and `.agents/skills`, global and project, in
  both versions. **DOCUMENTED** (E2, E3)
- Non-spec fields are tolerated: V1 "Unknown frontmatter fields are ignored";
  V2 accepts `license`/`compatibility` without interpreting them.
  **DOCUMENTED** (E2, E3)
- Therefore a Claude-only field like `allowed-tools` is **parsed and discarded**
  on OpenCode — the tool pre-approval it expresses does **not** transfer.
  **INFERRED** from the "unknown fields ignored" statement; confirmed as a live
  hazard by a real file that carries `allowed-tools: Bash`
  **VERIFIED-IN-SOURCE** (E6).
- Claude-only body features — dynamic `!` command injection, `${CLAUDE_SKILL_DIR}`,
  `${CLAUDE_PROJECT_DIR}`, `context: fork`, `agent:`, `paths:` gating, plugin
  namespacing — are **not documented anywhere in the OpenCode skills pages**.
  Treat them as inert text. **INFERRED** (absence in E2/E3).
- `disable-model-invocation` happens to be honoured by V2 (documented alias),
  ignored by V1. **DOCUMENTED** (E2, E3)

`INFERRED` risk: a skill authored for Claude Code that relies on
`allowed-tools` to avoid prompts will load fine in OpenCode and then prompt (or
fail) at tool-call time — the failure is detached from the skill.

---

## 9. How instructions actually enter context

Three distinct channels, only one of which is documented as a skill feature:

1. **Skill tool load** — body without frontmatter, plus base dir and up to ten
   supporting paths. **DOCUMENTED** (E3)
2. **`@skill-id` mention** — human-side explicit load. **DOCUMENTED** (E3)
3. **Plugin-injected system prompt** — a JS plugin can read a `SKILL.md` off disk
   and splice its body into every request's system prompt, bypassing the skill
   tool and its permissions entirely. **VERIFIED-IN-SOURCE** (E7): the local
   plugin resolves `~/.config/opencode/skills/godmode`, reads
   `activation/SKILL.md`, strips frontmatter with a hand-rolled regex, and wraps
   the body in `<EXTREMELY_IMPORTANT>` before injecting.

Channel 3 matters for VIKRAM-CLI: skill *permissions* do not govern plugin
injection. A plugin can place any instruction into context unconditionally, at
full token cost, on every request. **INFERRED** from (E7) + the permission model
in (E3).

---

## 10. Documented limitations

- No description → not advertised. **DOCUMENTED** (E3)
- Hidden dirs, `node_modules`, and depth > 6 are not searched. **DOCUMENTED** (E3)
- One `SKILL.md` per directory; no recursive skill search inside it.
  **DOCUMENTED** (E3)
- Flat `.md` skills get no supporting-file sample. **DOCUMENTED** (E3)
- Supporting files are never auto-loaded; the model must choose to read them.
  **DOCUMENTED** (E3)
- V1 enforces name/dir match and length caps; V2 does not. **DOCUMENTED** (E2, E3)
- `webfetch` URL-pattern objects unsupported in OpenCode 1. **DOCUMENTED** (E4)
- Policies are "not enforced in OpenCode 1"; only `provider.use` exists.
  **DOCUMENTED** (E8)

---

## 11. What is genuinely UNKNOWN

1. Symlink semantics (Section 4) — not documented, not testable here.
2. Literal error strings from the `skill` tool.
3. Behaviour when the working directory is not inside a git repo (V1 walk vs V2
   "project root").
4. Whether V2 still honours the V1 `permission.skill` map for backward
   compatibility — no doc page says either way. **UNKNOWN**
5. Whether `ask` on a skill blocks the model's turn or the load resolves
   asynchronously; V2 says "Wait for a decision from the client" (E4) without
   specifying skill-tool behaviour mid-turn.

---

## 12. Observations from this machine (OpenCode 2)

All **VERIFIED-IN-SOURCE** unless noted.

- Config is V2-flavoured: `opencode.json` uses the V2 MCP shape
  `"mcp": { "servers": { ... } }` (E9), and the bundled plugin header says
  "V2 migration notes" and "V2 `export default { id, setup(ctx) }`" (E7).
- `opencode.json` contains **no** `permissions` and no `skills` array → every
  skill is advertised under the shipped allow-all base policy. (E9)
- Skills tree is large: the glob for `skills/*/SKILL.md` was truncated at **400
  results**, so >400 skills are installed. Cost note: V2 advertises name +
  description per skill per model step. (E10, E3)
- **Duplicate content under two IDs.** `godmode` ships both a flat copy and a
  grouped copy:
  - `skills/godmode-activation/SKILL.md` → `name: godmode-activation` (E11)
  - `skills/godmode/activation/SKILL.md` → `name: activation` (E12)
  Both bodies are identical, so the same instructions are advertised twice and
  consume listing budget twice. This is exactly the collision class V2's
  precedence rules do *not* catch, because the IDs differ.
- Real skills carry Claude-only frontmatter keys that OpenCode ignores —
  `allowed-tools: Bash` and `disable-model-invocation: true` both appear in the
  tree (E6).
- Agent files use the **V2** permission schema — a list of
  `{action, resource, effect}` — not the V1 `permission:` map:
  `agents/web-researcher.md` declares `edit`/`shell` deny and `read` allow
  (E5). V1 docs would describe this file as malformed. **DOCUMENTED** contrast (E2, E4)
- Global compat roots are populated and **overlap**: `~/.claude/skills` and
  `~/.agents/skills` each contain `diagnose-crash` and `omarchy` (E13, E14).
  Under V2 ordering, `.agents/skills` registers after `.claude/skills` and
  therefore supplies the loaded copy. **INFERRED** from (E3) + the on-disk sets.
- `~/.agents/skills` exists (with `mcp.json` alongside) — the agent-compat path
  is real, not theoretical. (E14)

---

## 13. Recommendations for VIKRAM-CLI

1. **State the OpenCode version** your skill docs target. The V1 and V2 contracts
   are not interchangeable (Section 0).
2. Ship directory skills `skills/<id>/SKILL.md` with `id` = lowercase kebab-case
   and keep `name` equal to the directory even though V2 does not enforce it —
   it keeps the skill portable to V1 and to Claude Code, both of which do.
3. Always write a `description`; without one the skill is invisible to the model.
   **DOCUMENTED** (E3)
4. Do not put directory-level instructions in frontmatter — load strips it.
   **DOCUMENTED** (E3)
5. Do not rely on `allowed-tools` for portability; it is discarded.
6. If you add an `opencode.json`, add an explicit `permissions` rule set rather
   than inheriting allow-all (Section 12, first bullet).
7. Keep the discovery tree flat. Grouping directories are legal in V2 but invite
   the duplicate-ID class observed in Section 12.

---

## Evidence index

| ID | Claim area | Source | Label |
|---|---|---|---|
| E1 | V2 availability banner, V1-vs-V2 doc split | https://opencode.ai/docs/skills (fetched 2026-11-01) | DOCUMENTED |
| E2 | V1 discovery paths, frontmatter, name regex, `skill({name})`, `<available_skills>`, `permission.skill`, `tools.skill:false` | https://opencode.ai/docs/skills (fetched 2026-11-01) | DOCUMENTED |
| E3 | V2 paths, nesting/depth/hidden rules, `skills` array, HTTP catalogs, precedence order, ID-vs-name, load steps, `autoinvoke`, permission effects, troubleshooting | https://opencode.ai/v2/docs/skills/ (fetched 2026-11-01) | DOCUMENTED |
| E4 | V1→V2 rename note, `permissions` array, skill resource = Skill ID, last-match-wins, base policy, agent append order, approvals, policies `tool.use` | https://opencode.ai/v2/docs/permissions/ (fetched 2026-11-01) | DOCUMENTED |
| E5 | Real agent file using V2 `permissions` list | `/home/himanshu/.config/opencode/agents/web-researcher.md:4-13` | VERIFIED-IN-SOURCE |
| E6 | Claude-only `allowed-tools` present in an OpenCode skill | `/home/himanshu/.config/opencode/skills/copilot-git-commit/SKILL.md:5` | VERIFIED-IN-SOURCE |
| E7 | Plugin reads `activation/SKILL.md` and injects body into system prompt | `/home/himanshu/.config/opencode/plugins/godmode.js:60-92` | VERIFIED-IN-SOURCE |
| E8 | "Policies are not enforced in OpenCode 1"; only `provider.use` | https://opencode.ai/docs/policies (fetched 2026-11-01) | DOCUMENTED |
| E9 | Local config: V2 MCP shape, no `permissions`, no `skills` array | `/home/himanshu/.config/opencode/opencode.json:1-30` | VERIFIED-IN-SOURCE |
| E10 | >400 installed skills (glob truncated at 400) | `/home/himanshu/.config/opencode/skills/*/SKILL.md` | VERIFIED-IN-SOURCE |
| E11 | Flat duplicate skill | `/home/himanshu/.config/opencode/skills/godmode-activation/SKILL.md:1-4` | VERIFIED-IN-SOURCE |
| E12 | Grouped duplicate skill | `/home/himanshu/.config/opencode/skills/godmode/activation/SKILL.md:1-4` | VERIFIED-IN-SOURCE |
| E13 | Claude-compat global skills | `/home/himanshu/.claude/skills/` → `diagnose-crash`, `omarchy` | VERIFIED-IN-SOURCE |
| E14 | Agent-compat global skills (same two) | `/home/himanshu/.agents/skills/` → `diagnose-crash`, `omarchy` | VERIFIED-IN-SOURCE |

### Fetch failures / gaps
- `https://opencode.ai/docs/skills` and `/v2/docs/skills/` both render fine, but
  the V1 page is what the nav serves at `/docs/skills` — easy to read the wrong
  contract by accident. Flagged in Section 0.
- No OpenCode source checkout was available, so nothing here is
  "read the implementation". Every OpenCode claim is either docs or a local
  config/skill file.
- Symlink behaviour: docs dark, no runtime probe possible → **UNKNOWN**.
