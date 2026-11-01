# State and Configuration

How VIKRAM resolves its home directory, where state lives, and how to isolate state for testing.

## Home directory resolution

`paths::home()` resolves in this order:

1. `VIKRAM_HOME` environment variable (if set and non-empty).
2. `$HOME/.vikram` (default).

All skill directories, state, and configuration are rooted under this path.

## Directory layout

```
$VIKRAM_HOME/
├── skills/              # user skills (native format)
│   ├── imported/        # imported skills (Scope::Imported)
│   └── .vikram/         # state dir for the user root
├── agents/              # .agents/skills compat (Scope::User)
├── claude/              # .claude/skills compat (Scope::User)
├── opencode/            # .opencode/skills compat (Scope::User)
├── hermes/              # .hermes/skills compat (Scope::User)
├── bundles/             # bundle definitions
├── .vikram/             # global state dir
│   ├── disabled.json    # disabled skill names
│   ├── trust-policy.json
│   └── <skill-name>/    # per-skill history
│       └── manifests.json
└── (project dirs)       # .vikram/skills/ in any project
```

## Project skill directories

`paths::project_skill_dirs(&Path)` returns directories under a project root:

- `<project>/.vikram/skills/` (native)
- `<project>/.agents/skills/` (compat)
- `<project>/.claude/skills/` (compat)
- `<project>/.opencode/skills/` (compat)
- `<project>/.hermes/skills/` (compat)

Each is scanned with `Scope::Project`.

## State directory

`paths::state_dir()` returns `<root>/.vikram/` for a given skill root. State includes:

- `disabled.json` — set of disabled skill names
- `trust-policy.json` — trust configuration
- `<skill-name>/manifests.json` — per-skill history

## Environment variables

| Variable | Purpose |
| - | - |
| `VIKRAM_HOME` | Override home directory (used by tests and CI) |
| `EDITOR` | Editor for `vikram skills edit` |
| `GIT_TERMINAL_PROMPT` | Set to `0` to prevent git credential prompts during install |

## Test isolation

Tests that touch `VIKRAM_HOME` must serialize on a `static Mutex<()>` (see `runtime.rs:820-837` pattern). The `DiscoveryOptions::with_state_dir(dir)` and `Manager::at(root)` APIs allow full isolation without env vars.

## Process-global caveat

`VIKRAM_HOME` is process-global. Concurrent tests that set it will interfere. Always use `with_state_dir` or `Manager::at` when possible.
