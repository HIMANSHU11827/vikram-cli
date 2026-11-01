# Trust Policy File

The trust policy controls which skill sources are trusted and which are denied. It is stored as JSON in the
state directory.

## File location

`<state_dir>/trust-policy.json` (see `paths::state_dir()`). Loaded via `TrustPolicy::load()` or
`TrustPolicy::load_in(state_dir)`.

## File format

```json
{
  "project_trusted": false,
  "trusted_sources": ["/home/user/.agents/skills"],
  "deny": ["project:claude", "/tmp/untrusted", "suspicious-skill"]
}
```

| Field | Type | Default | Description |
| - | - | - | - |
| `project_trusted` | bool | `false` | Whether project-scoped skills are auto-trusted |
| `trusted_sources` | array of strings | `[]` | Canonicalized directory prefixes whose Imported skills get `Verified` |
| `deny` | array of strings | `[]` | Deny rules (source id, path, or name) |

## Deny rule matching order

1. **Source id** — `<scope>:<compat>` lowercased, e.g. `project:claude`, `user:native`.
2. **Canonicalized absolute path** — the skill's directory path.
3. **Bare skill name** — matches any skill with that name.

First match wins. A deny entry that matches nothing is not an error.

## Saving

`TrustPolicy::save()` writes to the default state dir. `save_in(state_dir)` writes to a specific dir. Both use
atomic write (temp file + rename).

## Permission requests

`TrustPolicy::permission_request(name, perm)` returns a `PermissionRequest` with the skill's trust level and
whether the permission is granted. Declared permissions in `SKILL.md` are requests evaluated against this policy —
never automatic grants.

## Operator controls

- Edit `trust-policy.json` directly, or use `vikram skills` commands.
- `vikram skills doctor` shows the current trust configuration.
- `project_trusted` should only be set to `true` in environments where the project directory is fully trusted.
