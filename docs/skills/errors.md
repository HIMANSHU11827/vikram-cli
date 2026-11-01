# Errors — SkillError and DiagnosticLevel

## `SkillError`

The crate's error type. Derives `Clone`, `PartialEq`, `Eq` (deviation D12). Variants:

| Variant | Cause | `is_content_error()` |
| - | - | - |
| `Io` | File system error (read, write, create dir) | false |
| `Frontmatter` | Missing delimiters, malformed YAML, duplicate keys | true |
| `Validation` | Name too long, description too long, name/dir mismatch | true |
| `NotFound` | Skill not found by name | false |
| `AlreadyExists` | Skill with that name already exists | false |
| `PathEscape` | Path traversal attempt | false |
| `Symlink` | Symlink refused | false |
| `Budget` | Budget exceeded during load | false |
| `Git` | Git operation failed | false |

`is_content_error()` returns `true` for `Frontmatter` and `Validation` errors, `false` for all others. Use this
to distinguish content problems from infrastructure problems.

## `Diagnostic`

A non-fatal finding produced during discovery or validation. Carries:

- `level: DiagnosticLevel`
- `message: String`
- `path: Option<PathBuf>` — the file the diagnostic refers to.

## `DiagnosticLevel`

| Level | Meaning | CLI exit code |
| - | - | - |
| `Error` | Skill is invalid and will not load | 1 |
| `Warning` | Skill loads but has a problem | 0 |
| `Info` | Informational note | 0 |

## Common diagnostics

| Message | Level | Cause | Fix |
| - | - | - | - |
| `missing frontmatter` | Error | File does not start with `---` | Add YAML frontmatter block |
| `name exceeds 64 characters` | Error | `name` field too long | Shorten the name |
| `description exceeds 1024 characters` | Error | `description` field too long | Shorten the description |
| `name does not match directory` | Warning | Directory name differs from `name` field | Rename directory or fix field |
| `unknown key under vikram:` | Warning | Unrecognized key in `vikram:` block | Remove or correct the key |
| `min_vikram newer than crate` | Warning | Skill requires a newer VIKRAM version | Update VIKRAM or remove the constraint |
| `duplicate skill name` | Info | Two skills share the same name | Rename one (both still load) |
