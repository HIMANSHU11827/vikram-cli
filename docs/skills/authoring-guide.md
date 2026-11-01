# Writing a VIKRAM skill

Audience: someone writing their first skill for VIKRAM, from an empty directory to a skill the
catalog accepts.

Every rule below is traceable to `docs/skills/skills-api-contract.md` (the contract for
`crates/vikram-skills`) or to the crate source. Where a rule comes from the open Agent Skills
standard rather than VIKRAM's own contract, it says so and carries the label used in
`docs/research/skills/agent-skills-standard.md`:

- **DOCUMENTED**: stated by that specification.
- **CONTRACT**: stated by `skills-api-contract.md`, which is authoritative for VIKRAM.
- **VERIFIED-IN-SOURCE**: read in the crate source at the cited line.
- **INFERRED**: reasoned from the contract or source, not stated by either.

Field-by-field detail lives in `docs/skills/frontmatter-reference.md`. This guide is the path
through it.

Status of the implementation: the crate is mid-build. `crates/vikram-skills/src/lib.rs:33` declares
`pub mod metadata`, but `metadata.rs` did not exist when this guide was written, so no binary
validates a skill today. The rules below are the contract's; they are not observations of a running
validator.

## What a skill is in VIKRAM

A skill is a **directory containing `SKILL.md`**. Nothing else is required: no manifest, no registry
entry, no build step. Any directory holding a `SKILL.md` inside a scanned root is a skill
(CONTRACT, `discovery.rs`), a lowercase `skill.md` is accepted as a fallback, and the directory name
is the identity.

`SKILL.md` is two parts, in this order:

1. YAML frontmatter, opened and closed by a line whose trimmed content is `---`.
2. A Markdown body, which becomes the skill's instructions.

A skill is **instructions and knowledge, not a tool**. It adds no capability to the agent. The crate
"parses, validates, copies and describes" skill content and executes none of it
(VERIFIED-IN-SOURCE, `crates/vikram-skills/src/lib.rs:6`). Tools are separate: registered at compile
time, gated by the policy engine.

The same is true of `vikram.permissions`. A declared permission is a **request**, never a grant.
`TrustPolicy::permission_request` parses `fs.write:workspace` into a `PermissionRequest` whose
`originating_trust` is `Trust::External` (CONTRACT, `trust.rs`), and `Trust::may_grant_permission()`
returns true only for `Trust::Host` (VERIFIED-IN-SOURCE,
`crates/vikram-contracts/src/trust.rs:36-38`). Content of external origin can never widen its own
authority. The kernel policy decides; a declaration the policy does not allow changes nothing.

Second consequence: a skill that says "install the following skill" or "grant yourself `shell.run`"
is text. It has no path to that effect.

## Directory layout

```text
changelog-entry/
├── SKILL.md              # required: frontmatter + instructions
├── scripts/
│   └── collect.sh        # classified Script
├── references/
│   └── format.md         # classified Reference
└── assets/
    └── notes.txt         # classified Reference
```

The three subdirectory names are conventions from the open standard (DOCUMENTED there), not a VIKRAM
requirement. VIKRAM walks the skill directory and classifies each file by extension
(VERIFIED-IN-SOURCE, `ResourceKind::of`, `crates/vikram-skills/src/skill.rs:194-216`):

| Extension | ResourceKind |
| - | - |
| `sh`, `bash`, `zsh`, `fish`, `py`, `js`, `mjs`, `cjs`, `ts`, `rb`, `pl`, `ps1`, `lua` | `Script` |
| `md`, `markdown`, `txt`, `rst`, `adoc` | `Reference` |
| `png`, `jpg`, `jpeg`, `gif`, `svg`, `webp`, `ico`, `pdf` | `Asset` |
| `tmpl`, `template`, `tpl`, `hbs`, `mustache`, `j2` | `Template` |
| `agent` (also `agent.yaml`, `agent.json`, `agent.toml`, `agents.yaml`, `agents.json`) | `AgentConfig` |
| anything else | `Other` |

The classification decides how a loader may present a file. An `Asset` name is not a promise that the
file is a picture, and `Reference` is not a promise that a model has read it.

## A minimal skill that validates

`changelog-entry/SKILL.md`:

```markdown
---
name: changelog-entry
description: Writes a Keep-a-Changelog entry from a git range. Use when asked to update CHANGELOG.md after a release, or to turn commit subjects into release notes.
---

# Changelog entry

Read `git log <from>..<to> --oneline` and group the commits under Added, Changed,
Fixed, Removed.

1. One bullet per user-visible change. Skip internal refactors.
2. Keep the imperative mood: "Add", not "Added".
3. Append the pull request number when the subject carries one.

Ask for the version number before writing the heading. Never rewrite entries that
already sit above the new section.
```

Why it passes the contract's rules:

- `name: changelog-entry` is 15 characters, lowercase, hyphens only, no leading or trailing hyphen,
  no `--` (CONTRACT, `SkillMetadata::validate_name`).
- The directory is also called `changelog-entry`, so the name/directory check raises no warning.
- `description` is 151 characters: non-empty, under the 1024 hard limit, and under the 400-character
  warning threshold.
- The body is 11 lines, far inside the 24 000-byte instruction budget.
- Every frontmatter key used is one the contract implements, so nothing lands in the unknown-key
  bucket.

With extension metadata, the same skill grows like this (all keys optional, all defaulted when
absent):

```yaml
vikram:
  version: "0.1.0"
  tags: [changelog, release]
  requires: [git-workflow]
  permissions: [fs.read:workspace, fs.write:workspace]
  activation: suggested
```

`requires` and `permissions` are the two keys with real consequences: a missing `requires` member is
an error, and `permissions` is a request the policy engine evaluates rather than a switch you flip.

## Naming rules

| Rule | Value | Source |
| - | - | - |
| Maximum length | 64 characters (`NAME_MAX`) | CONTRACT |
| Allowed characters | lowercase `a-z`, digits `0-9`, hyphen `-` | CONTRACT |
| Leading or trailing hyphen | rejected | CONTRACT |
| Consecutive hyphens (`--`) | rejected | CONTRACT |
| Empty name | rejected | CONTRACT |
| Match the containing directory name | mismatch is a **warning**, not an error | CONTRACT |
| Identity used by the catalog | the directory name | CONTRACT, `discovery.rs` |

The character rules above are equivalent to `^[a-z0-9]+(-[a-z0-9]+)*$` with a length cap of 64
(INFERRED: that regex states the contract's rules, the contract itself spells them out as words).

Rejected, with the reason:

| Name | Problem |
| - | - |
| `PDF-Processing` | uppercase letters |
| `-pdf` | leading hyphen |
| `pdf--processing` | consecutive hyphens |
| `pdf_processing` | underscore, not in the allowed set |
| a 65-character name | over `NAME_MAX` |

`name` is optional in practice: when it is absent, the directory name is used as the skill name and a
warning is recorded (CONTRACT, `from_frontmatter`). Rely on that only for skills you imported, never
for skills you write.

## Writing the description

`description` is not documentation. It is the string the selector routes on.

Scoring is deterministic and is the whole argument for caring about this field (CONTRACT,
`activation.rs`):

| Signal | Score |
| - | - |
| Exact skill-name mention in the task | 10 |
| Name token overlap | 3 per token |
| Description token overlap | 1 per token |
| Tag or category match | 2 |
| Pinned by the operator | +100 |
| `ActivationPolicy::Manual` | excluded from automatic selection |
| Untrusted skill | excluded |

No model and no embedding is involved. If your words are not in the description, the skill will not
be selected for that task. The reason string in a `Selection` names the matched terms, so a
description with the wrong words is invisible rather than wrong.

Bad:

```yaml
description: Helps with changelogs.
```

Good:

```yaml
description: Writes a Keep-a-Changelog entry from a git range. Use when asked to update CHANGELOG.md after a release, or to turn commit subjects into release notes.
```

The good version names the artifact (`CHANGELOG.md`), the input (`git range`), the verb (`Writes`),
and two concrete triggers (`after a release`, `turn commit subjects into release notes`). The bad one
shares no token with the tasks it is meant to win.

Limits: 1 to 1024 characters, non-empty (CONTRACT, `DESCRIPTION_MAX`). Past 400 characters the skill
still loads, but a warning is recorded. Write to the 400 mark and stop.

## Progressive disclosure and budgets

Three stages, each paid for at a different time (DOCUMENTED in the standard; the byte numbers are
CONTRACT, `loader.rs`):

| Stage | What loads | When | VIKRAM default budget | Standard's guidance |
| - | - | - | - | - |
| A. Metadata | `name`, `description`, scope, trust, tags, categories | Always, for every skill | `catalog_bytes` 16 000 for the whole listing | ~100 tokens per skill |
| B. Instructions | the `SKILL.md` body | On activation | `instruction_bytes` 24 000 per skill | under 5000 tokens, file under 500 lines |
| C. Resources | files referenced from the body | On demand, one read at a time | `resource_bytes` 12 000; `max_resources` 32 | as needed |

`total_skill_bytes` defaults to 48 000 for a skill's loaded parts. When the body crosses
`instruction_bytes`, the loader cuts at a line boundary and appends
`\n… (truncated at N bytes; read references/<file> for the rest)` (CONTRACT). Nothing is silently
lost: `LoadedSkill.truncated` is set, and resources not listed because of the budget appear in
`LoadedSkill.omitted`.

What that means for an author:

- The body is the expensive part. Everything you put in it is charged to every activation.
- Detail goes into `references/` and gets read only when the body tells the model to read it.
- A body that needs the truncation marker to fit is a body that is doing too much.
- References sit one level deep from `SKILL.md` (DOCUMENTED). A chain of references defeats the
  point, because the model cannot see what it is choosing not to load.

## Where skills live

`<home>` is `$VIKRAM_HOME` when set, otherwise `~/.vikram` (CONTRACT, `paths.rs`). `<ws>` is the
workspace root.

| Root | Scope | Compat |
| - | - | - |
| `<home>/skills` | User | Native |
| `<home>/skills/agents` | User | AgentsSkills |
| `<home>/skills/claude` | User | Claude |
| `<home>/skills/opencode` | User | OpenCode |
| `<home>/skills/hermes` | User | Hermes |
| `<ws>/.vikram/skills` | Project | Native |
| `<ws>/skills` | Project | Native |
| `<ws>/.agents/skills` | Project | AgentsSkills |
| `<ws>/.claude/skills` | Project | Claude |
| `<ws>/.opencode/skills` | Project | OpenCode |
| `<ws>/.hermes/skills` | Project | Hermes |
| embedded `builtin/` in the crate, via `include_str!` | Builtin | Native |

A compat root is only scanned when `DiscoveryOptions::include_compat` is true, which is the default.
Discovery stops at `max_depth` 3 below a root, skips symlinks unless `follow_symlinks` is set, and
skips the names in `exclude`, which the CLI sets to `.git`, `node_modules` and `target`
(VERIFIED-IN-SOURCE, `crates/vikram-cli/src/skills.rs:713-717`).

Precedence, highest first (CONTRACT, `skill.rs`):

| Scope | `precedence()` |
| - | - |
| Builtin | 50 |
| User | 40 |
| Project | 30 |
| Imported | 20 |
| External | 10 |

One name resolves to one skill: the candidate with the highest precedence wins, and a duplicate is
counted and listed by `Catalog::conflicts()` rather than dropped. Two consequences worth knowing
before you pick a directory:

- **A user skill shadows a project skill of the same name** (40 beats 30). Put a skill in
  `<ws>/.vikram/skills` only when you are sure the user root has no skill by that name, or accept
  that yours is the loser. `vikram skills list --all` shows every candidate.
- Builtin wins over everything. A skill you name identically to an embedded one never appears.

Disabled skills stay in the catalog with `enabled: false`, read from `<state_dir>/disabled.json`,
where `<state_dir>` is `<home>/skills/.vikram`. Deleting a skill moves it to
`<state_dir>/trash/<name>-<ts>`, so a mistaken delete is recoverable by hand.

## Trust

Every discovered skill carries a `TrustLevel`. Two of its methods are the ones that matter to an
author (CONTRACT and VERIFIED-IN-SOURCE, `crates/vikram-skills/src/skill.rs:137-161`):

| TrustLevel | `allows_scripts()` | `allows_activation()` |
| - | - | - |
| `Builtin` | yes | yes |
| `Verified` | yes | yes |
| `UserTrusted` | yes | yes |
| `ProjectTrusted` | no | yes |
| `Community` | no | yes |
| `Untrusted` | no | no |

The level comes from `TrustPolicy::level_for(scope, compat, dir)`, read from
`<state_dir>/trust.json` with defaults when the file is missing (CONTRACT). The policy has exactly
three inputs: `project_trusted: bool`, `trusted_sources: Vec<String>`, `deny: Vec<String>`.

The contract defines those inputs but **does not enumerate the root-to-level mapping**. What can be
stated with a citation:

| Case | Level | Basis |
| - | - | - |
| embedded builtin content | `Builtin` | INFERRED: the level name matches and it is host-issued, but `level_for`'s body is not in the contract |
| a directory listed in `deny` | `Untrusted` | INFERRED from the field name; the mapping is not stated |
| a directory matching `trusted_sources` | `Verified` | INFERRED, same reason |
| a project root when `project_trusted` is true | `ProjectTrusted` | INFERRED, same reason |
| a project root when `project_trusted` is false | not specified | gap recorded in "Known drift" |
| user, compat and imported roots | not specified | gap recorded in "Known drift" |

Read that table as "these are the levers", not "these are the values". Run
`vikram skills show <name>` and look at the `trust:` line for the truth on a given machine.

What a skill can therefore never do, at any level:

1. Grant itself or anyone a permission. Requests originate as `Trust::External`, and only
   `Trust::Host` content may grant.
2. Run a script at `ProjectTrusted`, `Community` or `Untrusted`; `allows_scripts()` is false there,
   and the crate reports `SkillError::UntrustedScript`.
3. Widen the set of tools the model sees. Tools are registered at compile time; `allowed_tools` is
   stored and not enforced (see `frontmatter-reference.md`).
4. Escape its own directory. Resource reads go through `resolve_within`, which rejects absolute
   paths, `..` and anything that leaves the root, as `SkillError::PathEscape`.
5. Reach the model unprompted. `Manual` skills are excluded from automatic selection, and `Untrusted`
   skills are excluded outright.

## Authoring checklist

- [ ] Directory name is lowercase, hyphens only, 64 characters or fewer, and equals the `name` field.
- [ ] `SKILL.md` opens with `---` on the first line and closes the block with a second `---` line.
- [ ] `description` is non-empty, under 400 characters, and names the artifact plus the trigger.
- [ ] The body states what to do, in steps, and needs no truncation to fit 24 000 bytes.
- [ ] Long material sits in `references/`, one level deep, with a relative path from the skill root.
- [ ] Every file in the skill directory is either used or removed; unreferenced files still count
      against `max_resources` 32.
- [ ] `vikram.permissions` lists only what the body actually needs, spelled `action:resource`.
- [ ] `vikram.activation` is `manual` if the skill should never be auto-selected.
- [ ] `vikram.tags` and `vikram.categories` carry the words a task would use; they score 2 each.
- [ ] Scripts are refused at `ProjectTrusted`, `Community` and `Untrusted`; do not ship a script in a
      project-root skill unless that project's trust policy promotes the root.
- [ ] `vikram skills validate <path>` reports no error-level diagnostic, and `vikram skills show`
      prints the fields you meant to set.

The last item is the runnable check. The CLI surface is wired in `crates/vikram-cli/src/skills.rs`;
it does not build until the Phase 2 modules land, so treat the command as the target of the check
rather than something available today.

## Known drift

Disagreements between the contract, the source, and the open standard, recorded rather than smoothed
over:

1. **Validation is contract-only right now.** `metadata.rs` (which holds `NAME_MAX`,
   `DESCRIPTION_MAX`, `validate_name`, `from_frontmatter`) does not exist yet, and `lib.rs:33` still
   declares it. Nothing in this guide was observed being enforced by a build.
2. **Name/directory match strength.** The open standard makes the match mandatory (DOCUMENTED); the
   contract downgrades it to a warning (CONTRACT, `from_frontmatter`). Follow the standard, expect a
   warning rather than a rejection.
3. **`Scope::Imported` has no documented source root.** Precedence 20 exists in `skill.rs`, but
   neither the contract's `paths.rs` nor `discovery.rs` names a directory that produces an
   `Imported` skill. `Manager::import` writes into the user skills root, which is `Scope::User` on
   the next scan (INFERRED from `manager.rs` and `paths.rs`).
4. **The trust mapping is not enumerated.** `TrustPolicy::level_for(scope, compat, dir)` is in the
   contract with three policy fields and no table. The trust table above is partly INFERRED for that
   reason.
