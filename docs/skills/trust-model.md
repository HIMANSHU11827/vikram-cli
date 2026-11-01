# VIKRAM skill trust model

Trust is a property of **where a skill came from**, never of what it claims to be. A skill's frontmatter cannot
grant itself permissions; declared permissions are requests evaluated by policy.

## Trust levels

| Level | Meaning | Typical source |
| - | - | - |
| `Builtin` | Ships inside the VIKRAM binary | `builtin::catalog()` |
| `UserTrusted` | Installed by the operator into their own skills dir | `~/.vikram/skills`, `~/.agents/skills`, … |
| `ProjectTrusted` | Lives in the project tree **and** `project_trusted = true` | `<project>/.vikram/skills` |
| `Verified` | Imported from an explicitly trusted source | `<home>/skills/imported` + `trusted_sources` |
| `Community` | Everything else that is not denied | repo skills, unknown sources |
| `Untrusted` | Matched by `deny` | any |

`compat` (Native / Claude / OpenCode / Hermes) is deliberately **not** an input to `level_for`. Format is not a
trust signal; location is. A `.claude/skills` directory and an `.agents/skills` directory in the same workspace get
the same level. (R3 in `docs/skills/skills-api-contract.md`.)

## How `level_for` decides (R3, in order)

1. `Scope::Builtin` → `Builtin`.
2. If the deny list matches (source id `<scope>:<compat>` lowercased, then canonicalized absolute dir, then bare
   skill name) → `Untrusted`. First match wins; a deny entry matching nothing is not an error.
3. `Scope::User` → `UserTrusted`.
4. `Scope::Project` + `project_trusted == true` → `ProjectTrusted`.
5. `Scope::Project` otherwise → `Community`.
6. `Scope::Imported` + dir under a `trusted_sources` prefix (canonicalized) → `Verified`.
7. `Scope::Imported` otherwise → `Community`.
8. `Scope::External` → `Community`.

`Verified` is reachable only via step 6. `deny` wins over every scope, including `Builtin`-adjacent project skills.

## What a skill can never do, regardless of level

- It cannot execute itself. Nothing in `vikram-skills` runs skill content ("it parses, validates, copies and
  describes it"). Scripts run only through the shell tool, under the same approval policy as any other command.
- It cannot become a permission grant. A skill's `allowed-tools` / `vikram.permissions` are requests; the policy
  layer evaluates them. `Trust::External` is the floor for skill text entering the model.
- It cannot widen its own trust by editing its frontmatter. `Manager::install` derives the directory name from the
  validated `name`, so a skill can never be installed under a mismatched identity.
- It cannot be silently overwritten. `Manager` refuses to clobber an existing directory (install aborts if the
  destination exists).
- It cannot carry its permissions across turns. Activation is per-turn; "Do not carry skills across turns unless
  re-mentioned."

## Declared permissions are requests

`TrustPolicy::permission_request(name, perm)` and `permission_requests(name, perms)` build `PermissionRequest`
values for the policy layer. The skill declares; the policy decides. A skill at `Community` level that asks for
`shell.exec` is denied by default policy — the operator must explicitly allow it.

## Repo skills are not trusted the way home skills are

Mirroring Codex's `TrustedSkillRoots`: only `<home>/skills` and `$HOME/.agents/skills` are in the trusted set.
Repo-layer skills (`<project>/.agents/skills`, `<project>/.claude/skills`, …) get `Community` unless
`project_trusted` is set. This is a deliberate asymmetry: a skill checked into a repository is reviewed by you, not
by the system.

## Operator controls

- `project_trusted: bool` — whether project-tree skills get `ProjectTrusted`.
- `trusted_sources: Vec<String>` — canonicalized directory prefixes whose imported skills become `Verified`.
- `deny: Vec<String>` — source ids, absolute paths, or bare names that become `Untrusted`.

State lives at `<root>/.vikram/trust.json` (`TrustPolicy::load` / `save`). Default is all-false / empty.
