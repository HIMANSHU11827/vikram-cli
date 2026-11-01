# Skill discovery and precedence

Discovery is a single pass over a list of roots. There is no database and no cache beyond the in-memory `Catalog`.

## Roots

Roots come from two functions, both returning `(Scope, Compat, PathBuf)`:

- `paths::user_skill_dirs()` — user-level roots, in order:
  1. `<home>/skills` (Native, `Scope::User`)
  2. `<home>/skills/agents` (Claude compat)
  3. `<home>/skills/claude`
  4. `<home>/skills/opencode`
  5. `<home>/skills/hermes`
  6. `<home>/skills/imported` (Native, `Scope::Imported`) — last, below Project in precedence
- `paths::project_skill_dirs(project_root)` — one root per directory from the project root down to the current
  working directory, each probing `<dir>/.agents/skills`, `<dir>/.claude/skills`, `<dir>/.vikram/skills`.

`DiscoveryOptions::roots_only()` starts empty; `.with_root(scope, compat, root)` adds a root. `.with_state_dir(dir)`
sets where disabled lists and trust policy live. `.with_trust(policy)` injects the policy (default is all-false).

## Walk rules

- `MAX_SCAN_DEPTH = 6` for `Recursive` mode; `DirectChildren` stops at depth 2 (`<root>/<skill>/SKILL.md`).
- Hidden directories are pruned. Symlinks are followed only when `follow_symlinks` is set on `resolve_checked`.
- A root that does not exist is skipped silently (no diagnostic).
- Per root: at most 2000 skill directories and 20000 directory entries.
- Up to 8 roots scanned in parallel, up to 64 skills parsed in parallel.

## Identity and precedence

A skill's identity is its **directory name**, not its frontmatter `name`. The frontmatter `name` must equal the
directory name; a mismatch is a warning (R4) and the skill still loads under the directory name.

When two skills share a directory name across roots, both are kept in `Catalog::entries[name]` as a `Vec`. They are
**not merged**. `resolve(name)` returns the first entry, which is the precedence winner. `list()` returns one per
name (the winner). `conflicts()` returns every name with more than one candidate.

Precedence order (lower sorts first):

| Rank | Scope |
| - | - |
| 0 | `Repo` / `Project` |
| 1 | `User` |
| 2 | `System` |
| 3 | `Admin` |
| 20 | `Imported` |

So a project skill beats a user skill beats a system skill. Sorting is not overwriting: nothing is dropped by rank,
only by `disabled` filtering.

## Disabled filtering

`disabled.json` (at the state dir) is a list of skill names. A disabled skill still appears in the catalog with
`enabled = false` and is excluded from activation. `enable`/`disable` are idempotent and do not require the skill to
exist (D8).

## Diagnostics

Every skill that cannot be parsed produces a `Diagnostic` with a level (`Error`, `Warning`, `Info`) and the path.
`Catalog::diagnostics_at(level)` filters. `Catalog::diagnostics` (the full list) is the audit trail; nothing is
silently dropped.
