# Claude Code Skills — Complete Analysis

Compiled: 2026-11-01. Sources: official Claude Code docs, Anthropic platform docs,
and a real OpenCode/Claude skill install on this machine.

Labels used throughout:
- **DOCUMENTED** — stated in fetched official docs (URL in Evidence index).
- **VERIFIED-IN-SOURCE** — the actual file at the cited local path was read.
- **INFERRED** — reasoning from documented facts; not stated verbatim anywhere.
- **UNKNOWN** — not documented and not observable here.

---

## 1. The unit of packaging

A skill is a **directory** containing `SKILL.md` plus optional supporting files.
`SKILL.md` = YAML frontmatter + Markdown body. **DOCUMENTED** (E1).

```
my-skill/
├── SKILL.md          (required — overview + navigation)
├── reference.md      (loaded when needed)
├── examples.md       (loaded when needed)
└── scripts/
    └── helper.py     (executed, not loaded)
```

The docs advise: "Keep `SKILL.md` under 500 lines. Move detailed reference
material to separate files." **DOCUMENTED** (E1)

A legacy form also exists: a single Markdown file in `.claude/commands/`.
Custom commands have been merged into skills — "A file at
`.claude/commands/deploy.md` and a skill at `.claude/skills/deploy/SKILL.md`
both create `/deploy` and work the same way." **DOCUMENTED** (E1)

Consequence for VIKRAM-CLI: one skill = one directory. Never ship a bare
`foo.md` at the skills root if you want supporting files.

---

## 2. Where skills live

| Location | Path | Loads in |
|---|---|---|
| Enterprise | `.claude/skills/<name>/SKILL.md` in the managed settings dir (e.g. `/etc/claude-code/`) | All users on deployed machines |
| Personal | `~/.claude/skills/<name>/SKILL.md` | All your projects, this machine |
| Project | `.claude/skills/<name>/SKILL.md` | Sessions in this repo (commit it to share) |
| Nested | `<subdir>/.claude/skills/<name>/SKILL.md` | Sessions started in/below `<subdir>`; else lazily |
| Additional dir | `.claude/skills/<name>/SKILL.md` in an `--add-dir` path | That session |
| Plugin | `<plugin>/skills/<name>/SKILL.md` | Wherever the plugin is enabled |
| claude.ai account | Synced into `~/.claude/skills/synced/` | Sessions signed in with that account |

**DOCUMENTED** (E1)

Two reserved folder names that silently do not load: `synced` (used for
claude.ai downloads) and `anthropic-skills` (plus the whole
`anthropic-skills:*` namespace). **DOCUMENTED** (E1)

---

## 3. Frontmatter reference

Frontmatter is parsed **only** if the opening `---` is the file's first line:
"Claude Code reads the frontmatter only when the opening `---` is the file's
first line. Otherwise it treats the whole file, `---` markers included, as
skill content." **DOCUMENTED** (E1)

"All fields are optional. Only `description` is recommended." An unrecognised
field is **ignored without an error**. Booleans accept `yes/no/on/off/1/0` in
any case. **DOCUMENTED** (E1)

| Field | Purpose |
|---|---|
| `name` | Command name in `/` menu; defaults to directory name |
| `description` | What + when; drives automatic matching |
| `when_to_use` | Extra trigger phrases; appended to `description` |
| `argument-hint` | Autocomplete hint, e.g. `[issue-number]` |
| `arguments` | Named positional args for `$name` substitution (string or list) |
| `disable-model-invocation` | `true` = only the human can invoke |
| `user-invocable` | `false` = only Claude can invoke |
| `allowed-tools` | Pre-approve tools for the invoking turn |
| `disallowed-tools` | Remove tools from the pool while active |
| `model` | Model override for the turn (or `inherit`) |
| `effort` | `low`…`max`, for the turn |
| `context` | `fork` = run in a subagent |
| `agent` | Subagent type when `context: fork` |
| `background` | With `fork`; `false` = wait for result |
| `hooks` | Hooks registered on invoke, kept for the session |
| `paths` | Glob gate on automatic activation |
| `shell` | `bash` (default) or `powershell` for `!` commands |
| `metadata` | Free-form map, ignored by Claude Code |
| `license` / `compatibility` | Agent Skills spec fields, accepted but not acted on |

**DOCUMENTED** (E1). The platform page lists only `name` + `description` as
required in the generic Agent Skills spec, with `name` ≤ 64 chars, lowercase
alphanumerics and hyphens, and neither field allowed to contain XML tags or the
reserved words `anthropic` / `claude` (E2).

**Divergence worth knowing:** the spec is stricter than the Claude Code loader.
Claude Code only *refuses* names containing zero-width spaces, control
characters, private-use glyphs, bidi characters, `<`/`>`, or odd whitespace —
it does not enforce the spec's 64-char/lowercase rule. **DOCUMENTED** (E1)

Observed real-world frontmatter is minimal — `name` + `description` only:

```yaml
---
name: omarchy
description: >
  REQUIRED for end-user customization of Linux desktop, window manager, or system config.
  ...
---
```
**VERIFIED-IN-SOURCE** (E3)

---

## 4. Discovery and precedence

Discovery:
- Project skills load from `.claude/skills/` in the start directory **and every
  parent up to the repository root**. **DOCUMENTED** (E1)
- Subdirectory skills "don't load at startup. They load the first time Claude
  reads or edits a file in that subdirectory" and then stay for the session.
  **DOCUMENTED** (E1)
- Symlinked skill folders are supported at enterprise/personal/project level and
  the skill "loads the skill once even if several locations point at the same
  target." **DOCUMENTED** (E1)

Name collisions — resolution order **DOCUMENTED** (E1):

| Conflict | Winner |
|---|---|
| enterprise vs personal vs project | "Enterprise over personal, and personal over project." |
| yours vs bundled skill | Yours, but **not** the bundled aliases |
| yours vs built-in command | Yours, but not its aliases |
| skill vs `.claude/commands/` file | The skill |
| root skill vs nested skill, same name | Both load; `/deploy` = root, `/apps/web:deploy` = nested |
| yours vs added-directory skill | Yours; the other runs under `/<dir>:<name>` |
| plugin skill vs any local skill | Both load (plugin is namespaced `/plugin:skill`) |
| synced claude.ai skill vs anything | The other one wins the short name |

Nested/added-dir variants are directory-qualified with `:` separators —
`/apps/web:deploy`, `/cli:verify`. **DOCUMENTED** (E1)

---

## 5. Automatic vs explicit activation

Default: both paths are live. Two fields restrict it. **DOCUMENTED** (E1)

| Frontmatter | You invoke | Claude invokes | Loaded into context |
|---|---|---|---|
| (default) | yes | yes | description always; body on invoke |
| `disable-model-invocation: true` | yes | **no** | description **not** in context |
| `user-invocable: false` | **no** | yes | description always |

If Claude tries a `disable-model-invocation` skill anyway, "Claude Code blocks
the call and instructs it not to reproduce the deploy steps another way."
**DOCUMENTED** (E1)

Stacking: at the terminal you can put several skills at the start of one
message — "Claude Code expands the first skill plus up to five more stacked
after it." Through `-p`/SDK only the first loads. **DOCUMENTED** (E1)

Settings-level visibility override, `skillOverrides` (E1):

| Value | Listed to Claude | In `/` menu |
|---|---|---|
| `"on"` | name + description | yes |
| `"name-only"` | name only | yes |
| `"user-invocable-only"` | hidden | yes |
| `"off"` | hidden | hidden |

`"off"` also hides the skill from Remote Control and Agent SDK command lists.
Plugin skills are **not** affected by `skillOverrides`. **DOCUMENTED** (E1)

---

## 6. Progressive disclosure

The three-level model from the platform docs **DOCUMENTED** (E2):

| Level | When loaded | Token cost | Content |
|---|---|---|---|
| 1 Metadata | always at startup | ~100 tokens/skill | `name`, `description` |
| 2 Instructions | on trigger | < 5k tokens | `SKILL.md` body |
| 3+ Resources | as needed | 0 until read | bundled files; scripts run in bash |

"Level 1: Metadata (always loaded) … Claude loads this metadata at startup and
includes it in the system prompt." "Level 2: Instructions (loaded when
triggered) … Claude reads `SKILL.md` from the filesystem using bash."
"Level 3 … scripts run through bash, and only their output enters context."
**DOCUMENTED** (E2)

In Claude Code specifically, the metadata listing is budgeted: "The budget
scales at 1% of the model's context window." When it overflows, "Claude Code
drops descriptions starting with the skills you invoke least." Combined
`description` + `when_to_use` is truncated at **1,536 characters**.
Knobs: `skillListingBudgetFraction`, `SLASH_COMMAND_TOOL_CHAR_BUDGET`,
`skillListingMaxDescChars`. **DOCUMENTED** (E1)

Real example of level 3 in the wild — a skill pointing at a sibling file:

> "read [`reporting.md`](reporting.md) before offering to file anything."
**VERIFIED-IN-SOURCE** (E4), with `reporting.md` present in the same directory
**VERIFIED-IN-SOURCE** (E5)

`INFERRED`: this is why the "keep SKILL.md under 500 lines" rule is a product
behaviour, not style advice — level 2 is a per-turn recurring cost.

---

## 7. Argument handling

Substitutions **DOCUMENTED** (E1):

| Variable | Meaning |
|---|---|
| `$ARGUMENTS` | full argument string |
| `$ARGUMENTS[N]` | 0-based positional |
| `$N` | shorthand for the above |
| `$name` | named, from `arguments:` list, mapped by position |
| `${CLAUDE_SESSION_ID}` | session id |
| `${CLAUDE_EFFORT}` | current effort level |
| `${CLAUDE_SKILL_DIR}` | dir holding `SKILL.md` |
| `${CLAUDE_PROJECT_DIR}` | project root |
| `${CLAUDE_PLUGIN_ROOT}` | plugin install dir (plugin skills only) |
| `${CLAUDE_PLUGIN_DATA}` | plugin persistent data dir (plugin skills only) |

Rules that bite in practice:
- Indexed args use shell-style quoting: `/my-skill "hello world" second` →
  `$0` = `hello world`. **DOCUMENTED** (E1)
- "An indexed placeholder with no corresponding argument … stays in the content
  unchanged." A named placeholder with no argument expands to empty.
  **DOCUMENTED** (E1)
- If no placeholder consumes anything, Claude Code appends
  `ARGUMENTS: <value>` so the input is not lost. **DOCUMENTED** (E1)
- Escape a literal `$` with a single backslash: `\$1.00`. **DOCUMENTED** (E1)
- Substituted argument text is **not** re-expanded — `$ARGUMENTS` inside an
  argument stays literal. **DOCUMENTED** (E1)

---

## 8. Tool and permission restrictions

`allowed-tools` "grants permission for the listed tools during the turn that
invokes the skill". Two hard limits: "It does not restrict which tools are
available", and "The grant clears when you send your next message."
**DOCUMENTED** (E1)

`disallowed-tools` removes tools from the pool while the skill is active; it
"can't remove `EndConversation` while any other tool remains." **DOCUMENTED** (E1)

Skill-level permission rules use `Skill(...)` syntax **DOCUMENTED** (E1):

```
Skill(commit)          # exact
Skill(review-pr *)     # prefix + any args
Skill(deploy *)        # deny form
```

`deny` on an unqualified name also blocks the nested qualified variant;
`deny` on an alias also blocks the skill behind it. **DOCUMENTED** (E1)

Disabling wholesale: deny the `Skill` tool itself in `/permissions`.
**DOCUMENTED** (E1)

---

## 9. Subagent behaviour

`context: fork` runs the skill in a **new** subagent whose prompt is the skill
content. Critical caveat: "Despite the name, a skill with `context: fork`
doesn't run in a fork of the current conversation" — the subagent has no
conversation history. **DOCUMENTED** (E1)

- `agent:` picks the type (`Explore`, `Plan`, `general-purpose`, or a custom
  agent from `.claude/agents/`); default `general-purpose`. **DOCUMENTED** (E1)
- `background: true` is the default: you keep working, the result arrives later.
  **DOCUMENTED** (E1)
- A backgrounded fork runs with the narrower tool set applied to background
  subagents. **DOCUMENTED** (E1)
- Its edits land outside session checkpoints — `/rewind` will not restore them.
  **DOCUMENTED** (E1)
- Explicit warning: fork "only makes sense for skills with explicit
  instructions" — a guidelines-only skill gives the subagent no task.
  **DOCUMENTED** (E1)

The inverse direction also exists: a subagent can **preload** skills, in which
case "the full skill content is injected at startup" rather than on invoke.
**DOCUMENTED** (E1)

---

## 10. Plugin distribution

- Plugin skills live at `<plugin>/skills/<name>/SKILL.md` and are namespaced
  `/plugin-name:skill-name`. **DOCUMENTED** (E1)
- A plugin may also have a single root `SKILL.md`, named by frontmatter `name`
  with the plugin directory as fallback. **DOCUMENTED** (E1)
- A personal/project skill folder can *become* a plugin by adding
  `.claude-plugin/plugin.json`, loading as `<name>@skills-dir` with its own
  agents, hooks and MCP servers. In a project this needs the workspace trust
  dialog accepted first, and an admin can block it. **DOCUMENTED** (E1)
- Plugin skills are exempt from `skillOverrides`; manage them via `/plugin`.
  **DOCUMENTED** (E1)

---

## 11. Sharing and lifecycle

Sharing **DOCUMENTED** (E1): commit `.claude/skills/`, publish a plugin, or
deploy via managed settings. `~/.claude/skills/` is **not** read by Cowork or
cloud sessions — those use account-synced skills.

Content lifecycle **DOCUMENTED** (E1):
- Body enters the conversation as **one message** and persists across turns.
- "Claude Code does not re-read the skill file on later turns."
- Re-invoking with identical rendered content adds a "already loaded" note
  instead of a duplicate.
- Auto-compaction re-attaches the most recent invocation of each skill, keeping
  "the first 5,000 tokens of each", combined budget 25,000 tokens, filled newest
  first — **older skills can be dropped entirely**.
- Live edit detection: skill dirs are watched; create a *new* top-level skills
  dir and you need `/reload-skills`.
- Permissions do **not** persist with content: an `allowed-tools` grant clears
  on your next message.

`INFERRED`: put the load-bearing instructions near the top of `SKILL.md`,
because compaction truncates from the tail.

---

## 12. Error handling

Documented behaviours **DOCUMENTED** (E1):
- Malformed YAML → "the skill still loads with no fields set"; `/skill-name`
  still works but automatic matching breaks. Diagnose with `--debug`.
- `claude plugin validate .claude/skills` finds unparseable frontmatter.
- A refused name produces a startup notice and a `[names]` debug-log entry.
- `shell: bash` with no bash → invocation fails before any command runs:
  `Skill <name> requires bash (shell: bash in frontmatter) but Git Bash was not found`.
- A failed `!` command "aborts the entire skill invocation, not just its own
  placeholder" with `Shell command failed for pattern "..."`.
- `No shell command in <name> was run` means an unacceptable character in a
  command.
- Unparseable/oversized listing → warning in debug log.

---

## 13. Security posture

Anthropic's stated position **DOCUMENTED** (E2):
- "Use Skills only from trusted sources."
- "Skills that fetch data from external URLs pose particular risk, as fetched
  content might contain malicious instructions."
- Malicious skills can cause "data exfiltration, unauthorized system access".
- Audit `SKILL.md`, scripts and images; "treat like installing software".

Claude Code-specific attack surface **DOCUMENTED** (E1):
- "Workspace trust doesn't gate this field." A **project** skill's
  `allowed-tools` applies even in a `-p` run in an untrusted folder.
- "A skill can grant itself broad tool access, so review the `allowed-tools` of
  skills checked into a repository."
- Mitigations: `allowManagedPermissionRulesOnly` makes Claude Code ignore
  `allowed-tools` in project/personal skills; `disableSkillShellExecution`
  replaces every `!` command with a policy placeholder.
- Synced claude.ai skills get display-text sanitisation: control characters
  removed, and "it also escapes angle brackets so the text can't imitate Claude
  Code's internal formatting." Their `!` commands never run locally.
- Injected `!` commands are permission-checked before running; a deny rule
  aborts the invocation, and outside auto mode any non-allow result aborts too.

`INFERRED`: the angle-bracket escaping is a prompt-injection control aimed at
markup spoofing, not a general sanitiser — it does not stop a skill from simply
telling the model to exfiltrate data. The only real control is review plus
`allowed-tools`/deny rules.

---

## 14. Notes for VIKRAM-CLI

1. If VIKRAM-CLI ships skills, ship **directories**, not lone `.md` files
   (Section 1). **DOCUMENTED** basis.
2. Keep `description` short and front-loaded: 1,536-char cap and a listing
   budget of 1% of context. **DOCUMENTED** (E1)
3. Put side-effecting workflows behind `disable-model-invocation: true`.
   **DOCUMENTED** (E1)
4. Do not rely on `allowed-tools` for *restriction* — it grants, it does not
   bound. Use `disallowed-tools` or settings deny rules. **DOCUMENTED** (E1)
5. A skill's body is a standing per-turn cost after load; references are not.
   **DOCUMENTED** (E1)

---

## Evidence index

| ID | Claim area | Source | Label |
|---|---|---|---|
| E1 | All Claude Code skill mechanics, frontmatter, precedence, fork, lifecycle, security | https://code.claude.com/docs/en/skills (fetched 2026-11-01) | DOCUMENTED |
| E2 | Progressive disclosure levels, spec field rules, cross-surface limits, security guidance | https://platform.claude.com/docs/en/agents-and-tools/agent-skills/overview (fetched 2026-11-01) | DOCUMENTED |
| E3 | Minimal real frontmatter (`name`+`description` only) | `/home/himanshu/.claude/skills/omarchy/SKILL.md` lines 1–12 | VERIFIED-IN-SOURCE |
| E4 | Level-3 reference to a sibling file | `/home/himanshu/.claude/skills/diagnose-crash/SKILL.md:96` | VERIFIED-IN-SOURCE |
| E5 | Supporting file actually present | `/home/himanshu/.claude/skills/diagnose-crash/reporting.md` | VERIFIED-IN-SOURCE |

### Fetch failures / gaps
- No `SKILL.md` under `/home/himanshu/.claude` matched a recursive glob; the
  directory does exist and contains exactly `diagnose-crash` and `omarchy`
  (verified by directory listing). Glob recursion limitation, not absence.
- `UNKNOWN`: whether `~/.claude/skills/*` entries are symlinks — no tool here
  exposes inode/link type.
