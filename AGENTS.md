# AGENTS.md — Project Development Instructions

## Overview

Vikram CLI is a terminal AI runtime (Rust workspace). This file provides project-wide development instructions. Runtime Agent entity definitions are separate (see `crates/vikram-store` entities table and `agents/` directories).

## Architecture

See HARNESS.md for the full architecture.

## Technology Stack

- Rust 2024 edition, rust-version 1.86
- tokio async runtime
- rusqlite (bundled SQLite)
- ratatui + crossterm TUI
- clap CLI
- OpenAI-compatible providers

## Coding Conventions

- Use existing patterns and module structure.
- No comments unless necessary.
- Tests in each crate's `#[cfg(test)]` modules.
- Run `cargo fmt`, `cargo clippy`, `cargo test --workspace` before finishing.

## Build Commands

```
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets
```

## Testing Commands

```
cargo test --workspace
```

## Security Requirements

- Deny-first permissions; mandatory floor cannot be widened.
- User/profile/entity isolation.
- No secrets in Markdown files.
- Hive participants only get authorized shared context.

## Development Restrictions

- Do not introduce CLAUDE.md.
- Do not duplicate functionality; reuse existing components.
- Markdown files are context resources, not the database.
