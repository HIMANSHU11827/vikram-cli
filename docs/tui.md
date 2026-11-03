# TUI — Terminal Interface

ratatui + crossterm. State, rendering and input handling. Subscribes to the runtime
event bus; never stops work as a side effect of changing views.

## Features

- Session list, conversation transcript with foldable activity rows, live turn clock.
- Agent/Hive side panels, scrolling, mouse hit regions.
- Approval prompts, user questions, notices.
- ASCII fallback (`--ascii`).

## Files

- `crates/vikram-tui/src/app.rs`, `commands.rs`, `input.rs`, `terminal.rs`
- `crates/vikram-tui/src/widgets/*`

## Notes

Hardcoded collapse thresholds (88 width, 16 height) and RGB palette. UTF-8 byte slicing
risk in input/tools (tracked).