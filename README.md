# VIKRAM

A terminal coding agent written in Rust. One binary (`vikram`) that the operator
drives directly: interactive TUI, headless chat, and operator commands.

Status: pre-release. `0.1.0`, `edition = "2024"`, `rust-version = 1.86`.

## Crates

| Crate | Role |
|---|---|
| `vikram-contracts` | Stable typed contracts shared by every subsystem |
| `vikram-kernel` | The single execution architecture: ordered operations, effects, journal, permission gates |
| `vikram-policy` | Deny-first permission evaluation with a non-widenable mandatory floor |
| `vikram-store` | Durable journal, session metadata and agent profiles on SQLite (`rusqlite`, bundled) |
| `vikram-runtime` | Host integration: sessions, agents, background tasks, providers, tools, MCP and plugins |
| `vikram-skills` | Skill discovery, loading, activation, installation and history |
| `vikram-tui` | Terminal interface: state, rendering (ratatui) and input handling |
| `vikram-cli` | The `vikram` command: terminal interface, headless chat, operator commands |
| `vikram-mcp` | Model Context Protocol server |

## Build

```sh
cargo build --release
./target/release/vikram
```

Requires Rust 1.86 or newer. SQLite is compiled in via `rusqlite/bundled`, so no
system `libsqlite3` is needed.

## Docs

- `docs/skills/` — skills system: discovery and precedence, activation, permissions,
  trust model, bundles, installation, CLI reference, testing
- `docs/development/` — testing strategy, implementation notes
- `docs/research/` — competitor and architecture research that informed the design
- `docs/decisions/` — architecture decision records

## License

MIT OR Apache-2.0
