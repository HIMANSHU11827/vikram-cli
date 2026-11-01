# Permissions and trust

A skill's declared permissions are a **request**, never a grant. Trust is determined by where the
skill lives, not by what it says.

## Trust levels

| Level | Meaning |
| - | - |
| `Builtin` | Shipped with VIKRAM. Highest trust. |
| `ProjectTrusted` | Project root skill, project marked trusted. |
| `UserTrusted` | User root skill. |
| `Verified` | Imported skill under a trusted source prefix. |
| `Community` | Default for project skills without project_trusted, imported skills, external. |
| `Untrusted` | Matches a deny rule. |

## How `level_for` decides (R3)

Ordered steps, first match wins:

1. `Scope::Builtin` → `Builtin`.
2. Deny rule matches → `Untrusted`.
3. `Scope::User` → `UserTrusted`.
4. `Scope::Project` + `project_trusted` → `ProjectTrusted`.
5. `Scope::Project` → `Community`.
6. `Scope::Imported` under a `trusted_sources` canonicalized prefix → `Verified`.
7. `Scope::Imported` → `Community`.
8. `Scope::External` → `Community`.

`compat` (claude/codex/etc.) is deliberately excluded: format is not a trust signal, location is.

## Deny rules

`TrustPolicy` holds a deny list. Each entry matches in order:

1. Exact source id (`<scope>:<compat>` lowercased, e.g. `project:claude`).
2. Canonicalized absolute directory path.
3. Bare skill name.

First match wins. A deny entry that matches nothing is not an error.

## Permission requests

`TrustPolicy::permission_request(name, perm)` builds a `PermissionRequest` for a declared permission.
The policy layer (MandatoryFloor) evaluates it. A skill can never bypass a deny.

## Operator controls

- `TrustPolicy::load()` / `load_in(state_dir)` — read policy from disk.
- `TrustPolicy::save()` / `save_in(state_dir)` — persist.
- Policy file lives in the state dir.

## Non-negotiables

- Skill text always enters the model as `Trust::External`, regardless of root.
- Declared permissions are requests, never grants.
- A skill cannot raise its own trust by editing its frontmatter.
- `Verified` is reachable only via Imported + trusted_sources.
