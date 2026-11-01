# `vikram skills` CLI reference

The CLI never reimplements discovery, parsing, validation, or trust logic. Every subcommand calls into
`vikram-skills` (`Catalog`, `Loader`, `Manager`, `Installer`, `BundleIndex`, `History`, `TrustPolicy`). Failures
return `Result<(), String>` and exit 1 with `vikram: <message>`.

## Subcommands

| Command | Purpose |
| - | - |
| `vikram skills` (no subcommand) | `list` |
| `list [--json] [--all] [--scope S] [--search Q]` | list skills; `--scope` is builtin/user/project/imported/external |
| `search <q>` | substring search over name/description/tags/categories |
| `show <name> [--lines 40]` | show one skill's metadata and body (first N lines) |
| `validate [<path>]` | validate a skill dir or the whole catalog; exit 1 on Error-level diagnostics |
| `create <name> --description [--from]` | scaffold a new skill (directory derived from validated name) |
| `edit <name> [--body <file>]` | open `$EDITOR`; refuses when unset unless `--body` given |
| `delete <name> [--yes]` | delete (refuses without `--yes` on a non-TTY) |
| `enable <name>` / `disable <name>` | toggle enabled state (idempotent, skill need not exist) |
| `duplicate <name> <as>` | copy a skill under a new name |
| `export <name> <dest>` | directory archive + `EXPORT.json` (no zip dependency) |
| `import <path>` | import from a directory; name taken from frontmatter |
| `install <source> [--git --rev --subdir --yes]` | install from a source; prints source and rev, refuses without `--yes` |
| `bundles` / `bundle show <name>` | list / inspect bundles |
| `history <name>` | show change history |
| `rollback <name> [--yes]` | roll back to the previous snapshot |
| `doctor` | health check of the skills dir; exit 1 when missing/unwritable |

## Conventions

- `--json` emits a dedicated `SkillRow` DTO, never crate internals.
- `create` derives the directory name from the validated `name`, so VIKRAM can never create a name/dir mismatch.
- `edit` refuses to open `$EDITOR` when the loader truncated the body, to avoid silently rewriting a skill down to the
  24 KB cap.
- `delete` and `edit` require the skill to exist; `enable`/`disable` do not (D8).
- `install` from git pins via `--branch` and sets `GIT_TERMINAL_PROMPT=0`; raw SHAs are refused with git's own
  message (D7).
- `doctor` lists only project dirs that exist (the crate returns the full preferred-path list even for absent dirs).

## Exit codes

0 on success; 1 on any error (missing description, invalid name, non-TTY refusal, missing skills dir, etc.). Errors
print as `vikram: <message>`.

## Environment

- `VIKRAM_HOME` — root for all user state. Defaults to `~/.vikram`.
- `EDITOR` — used by `edit` when `--body` is not given.
