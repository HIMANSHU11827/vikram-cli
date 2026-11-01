# Configuration — VIkRAM_HOME and config paths

## `VIKRAM_HOME`

Every path in the skills system resolves relative to `VIKRAM_HOME`. The resolution logic is in
`crates/vikram-runtime/src/config.rs`:

1. If the `VIKRAM_HOME` environment variable is set, use it.
2. Otherwise, use `~/.vikram` (or the platform equivalent).

`config::vikram_home()` returns a `PathBuf`. All skill roots, state directories, and bundle paths derive from it.

## Directory layout under `VIKRAM_HOME`

```
$VIKRAM_HOME/
├── skills/              # User skills (Scope::User)
│   ├── <name>/          # Native format
│   ├── agents/          # Agent Skills standard format
│   ├── claude/          # Claude Code format
│   ├── opencode/        # OpenCode format
│   ├── hermes/          # Hermes format
│   └── imported/        # Imported skills (Scope::Imported)
├── bundles/             # Bundle definitions
├── state/               # Global state (disabled list, trust policy)
└── history/             # Per-skill history (hashes, manifests)
```

## Project roots

Project skills are discovered by walking from the project root down to the current working directory. The project
root is found by marker files (`.git`, `Cargo.toml`, `package.json`, etc.). In each directory, the walker probes:

- `<dir>/.agents/skills`
- `<dir>/.claude/skills`
- `<dir>/.opencode/skills`

## State directory

Per-root state lives in `<root>/.vikram/` (for project roots) or `<VIKRAM_HOME>/state/` (for user roots). State
includes:

- `disabled.json` — set of disabled skill IDs
- `trust-policy.json` — deny rules and trusted sources

## Environment variables

| Variable | Purpose |
| - | - |
| `VIKRAM_HOME` | Override the home directory (default: `~/.vikram`) |
| `EDITOR` | Editor for `vikram skills edit` (no default) |
| `GIT_TERMINAL_PROMPT` | Set to `0` by the installer to prevent interactive git prompts |

## Test isolation

Tests that touch `VIKRAM_HOME` must serialize on a `static Mutex<()>` because the variable is process-global.
The crate provides `DiscoveryOptions::with_state_dir(dir)` for test isolation.
