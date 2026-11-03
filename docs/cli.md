# CLI — Operator Commands

`vikram` with no subcommand launches the TUI. Subcommands work on the same durable store
(no second source of truth).

## Command groups

sessions, project, workspace, files, agents, config, doctor, allow, tools, mcp, skills,
users, profiles, entities, hive, tickets.

## Key commands

- `vikram chat "..."` — headless single message (forces unattended posture).
- `vikram sessions ...` — session management.
- `vikram tickets ...` — unified ticket management.
- `vikram hive run <id> <msg>` — run a Hive conversation.
- `vikram config set <key> <value>` — configure.

## Files

- `crates/vikram-cli/src/main.rs`
- `crates/vikram-cli/src/{chat,doctor,skills,terminal,tools}.rs`

## Notes

`runtime()` uses `.expect()` (panic on failure). MCP serve is a stub. Doctor terminal
size reads env.