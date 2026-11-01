# Scopes — where a skill lives and what that means

A skill's **scope** is the category it was discovered in. Scope determines trust level, precedence in listings,
and which CLI operations are allowed. Scope is not a field in `SKILL.md`; it is derived from the directory the
skill was found in.

## The five scopes

| Scope | Where it lives | Trust level (default) | Precedence (lower = first) |
| - | - | - | - |
| `Builtin` | Embedded in the binary (`builtin::catalog()`) | `Builtin` | 0 |
| `Project` | `.agents/skills`, `.claude/skills`, etc. under the project root | `ProjectTrusted` if project trusted, else `Community` | 1 |
| `User` | `<VIKRAM_HOME>/skills`, `<VIKRAM_HOME>/skills/agents`, `claude`, `opencode`, `hermes` | `UserTrusted` | 2 |
| `Imported` | `<VIKRAM_HOME>/skills/imported` | `Verified` if under a trusted source, else `Community` | 3 |
| `External` | Any path passed explicitly (CLI `--path`, `Manager::at`) | `Community` | 4 |

## Discovery roots

`paths::user_skill_dirs()` returns `(Scope, Compat, PathBuf)` triples in this order:

1. `<home>/skills` — `Scope::User`, `Compat::Native`
2. `<home>/skills/agents` — `Scope::User`, `Compat::Agents`
3. `<home>/skills/claude` — `Scope::User`, `Compat::Claude`
4. `<home>/skills/opencode` — `Scope::User`, `Compat::OpenCode`
5. `<home>/skills/hermes` — `Scope::User`, `Compat::Hermes`
6. `<home>/skills/imported` — `Scope::Imported`, `Compat::Native`

Project roots come from `paths::project_skill_dirs(&project_root)`, which walks every directory from the project
root down to the current working directory and probes `<dir>/.agents/skills` (and the compat subdirectories).

## Scope vs compat

- **Scope** = trust category (where it lives). Five values.
- **Compat** = format dialect. `Native`, `Agents`, `Claude`, `OpenCode`, `Hermes`, `Codex`. Determines which
  frontmatter fields are expected and how the body is parsed.

A skill in `<home>/skills/claude` has scope `User` and compat `Claude`. A skill in `<project>/.agents/skills` has
scope `Project` and compat `Agents`.

## Scope and precedence

When two skills share a name, both are listed (no merge). The listing order is by `(scope_rank, name, path)`:

- `Builtin` = 0, `Project` = 1, `User` = 2, `Imported` = 3, `External` = 4.

Lower rank sorts first. A project skill with the same name as a user skill appears first in the catalogue.

## Scope and the CLI

- `vikram skills list --scope <builtin|user|project|imported|external>` filters by scope.
- `vikram skills create` always creates in the user root (scope `User`).
- `vikram skills import` creates in the imported root (scope `Imported`).
- `vikram skills install` (git) creates in the user root (scope `User`).

## Scope and the trust model

Trust level is derived from scope + location + policy. See `docs/skills/permissions.md` for the full `level_for`
mapping. The key invariant: scope determines the *starting* trust, policy can only downgrade (deny), never upgrade.
