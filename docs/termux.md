# Termux / Android

VIKRAM runs on Termux (Android) as a normal Linux-like environment. No Android-specific
build is required; the standard Rust toolchain produces a working binary.

## Install prerequisites

```sh
pkg update
pkg install rust clang git
# Optional, for browser automation:
pkg install chromium
```

Rust 1.86+ is required. SQLite is bundled (`rusqlite/bundled`), so no system
`libsqlite3` is needed.

## Build

```sh
git clone https://github.com/HIMANSHU11827/vikram-cli.git
cd vikram-cli
cargo build --release
./target/release/vikram
```

## Runtime paths

- `VIKRAM_HOME` defaults to `$HOME/.vikram` (`/data/data/com.termux/files/home/.vikram`).
- Override with `export VIKRAM_HOME=/path/to/state`.
- Skills are discovered under `$VIKRAM_HOME/skills`.

## What works

- TUI, headless `chat`, all operator commands.
- Sessions, tickets, agents, hive, memory, skills, MCP client, journal.
- PTY sessions (default shell `sh`).
- File tools, shell tools, web fetch/search (network permitting).
- SQLite durable state (WAL).

## What does not work (by design)

- **Desktop automation** (`computer` module): requires X11/Wayland helpers (xdotool,
  scrot, grim, …) which Termux does not provide. `vikram computer status` reports
  "unsupported on this platform".
- **OS-level sandbox**: bubblewrap is unavailable; sandbox passes through (policy still
  gates everything).
- **Browser automation** unless Chromium is installed via `pkg install chromium`.

## Notes

- `/tmp` may be limited; Termux sets `TMPDIR`, which `std::env::temp_dir()` honors.
- WAL journaling requires a same-host filesystem (fine on Termux's internal storage;
  avoid running state on shared/network mounts).
- No Android-specific `cfg` gates are needed; Termux is treated as Linux for our purposes.
- Desktop/phone notifications are not implemented (future work).