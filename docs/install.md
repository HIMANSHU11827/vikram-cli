# Installing VIKRAM

VIKRAM builds from source on Linux, macOS, Windows, and Android (Termux) with a stock Rust
toolchain. There are no prebuilt binaries yet — `cargo install` from this repository is the
supported path today.

## Requirements (all platforms)

- **Rust 1.89+** — `rustup` is the easiest way
- **git** — to clone the repository
- **SQLite** — bundled (`rusqlite/bundled`); nothing to install
- **TLS** — `ring` via rustls; no OpenSSL needed
- ~2 GB free disk for the build, ~200 MB for the binary

Optional, for full features:

- **Chromium** — browser automation tools (`browser.*`)
- **tesseract** — OCR in computer-use tools
- **xdotool / scrot / grim** — desktop automation (Linux only)
- **bubblewrap** — OS-level sandbox (Linux; absent → pass-through with a notice)

---

## Linux

```sh
# Debian/Ubuntu
sudo apt install build-essential pkg-config git curl
# Fedora
sudo dnf install gcc make pkgconf-pkg-config git curl
# Arch
sudo pacman -S base-devel git curl

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Build and install
git clone https://github.com/HIMANSHU11827/vikram-cli.git
cd vikram-cli
cargo install --path crates/vikram-cli --locked
vikram doctor     # verify the setup
vikram            # open the TUI
```

`cargo build --release` also works if you prefer a local binary at
`target/release/vikram`.

## macOS

```sh
# Xcode command line tools provide clang/linker
xcode-select --install
brew install git rustup
rustup default stable

git clone https://github.com/HIMANSHU11827/vikram-cli.git
cd vikram-cli
cargo install --path crates/vikram-cli --locked
vikram doctor
vikram
```

Desktop automation uses `sandbox-exec` (Seatbelt) where available; bubblewrap does not exist on
macOS and is never required — the sandbox degrades gracefully and says so.

## Windows

VIKRAM builds on Windows with MSVC (the default `x86_64-pc-windows-msvc` target).

1. Install **Visual Studio Build Tools** with the "Desktop development with C++" workload
   (provides `link.exe` and the Windows SDK).
2. Install **Rust** — <https://rustup.rs> — accepting the MSVC toolchain.
3. Install **Git for Windows**.

```powershell
git clone https://github.com/HIMANSHU11827/vikram-cli.git
cd vikram-cli
cargo install --path crates/vikram-cli --locked
vikram doctor
vikram
```

Notes:

- The TUI needs a terminal that supports ANSI — Windows Terminal, `wezterm`, `alacritty`, or
  the new Windows console all work. Legacy `cmd.exe` conhost works but is cramped.
- Shell tools run through `cmd.exe /C` on Windows; POSIX shell commands need Git Bash or WSL.
- PTY sessions use ConPTY — persistent interactive shells work.
- Desktop automation (`computer.*`) is not supported on Windows; `vikram doctor` reports it as
  unavailable rather than failing.
- The daemon service manager has a `ScheduledTask` backend; `vikram cron` works without it.

## Android (Termux)

Termux is a full Linux userland on Android's Bionic libc. VIKRAM targets
`aarch64-linux-android` natively — do **not** use the musl target (Android's loader rejects it).

```sh
pkg update
pkg install rust git clang
# Optional: pkg install chromium   (browser tools)

git clone https://github.com/HIMANSHU11827/vikram-cli.git
cd vikram-cli
cargo install --path crates/vikram-cli --locked   # or: cargo build --release
vikram doctor
vikram
```

Termux specifics:

- **Build on-device works** but is slow on phones with little RAM; cross-compiling on a PC with
  the Android NDK (`cargo build --target aarch64-linux-android`) and copying the binary is
  faster for low-end devices.
- **`/tmp` may not exist** — VIKRAM uses `TMPDIR`, which Termux sets correctly.
- **PTY sessions** work (Android exposes `/dev/ptmx`); the default shell is `sh`.
- **No bubblewrap/landlock** — the sandbox reports pass-through; the permission policy still
  gates everything.
- **No desktop automation** — no X11/Wayland helpers exist on Android.
- **No systemd** — `vikram cron` runs fine as a foreground/scheduled job; the service-manager
  integration reports unsupported rather than failing.
- Doctor hints use `pkg install …` on Android.

---

## After install

```sh
vikram doctor    # checks tools, config, paths — fix what it reports
vikram config init   # or set VIKRAM_* env vars / ~/.vikram/config.json
vikram           # TUI
vikram chat "hello"  # one-shot headless turn
```

State lives in `~/.vikram/` (override with `VIKRAM_HOME`). SQLite is WAL-mode; keep it on local
storage, not a network mount.

## Troubleshooting

| Symptom | Likely cause / fix |
|---|---|
| `cargo build` fails on `aws-lc-sys`/`ring` | You're on an old checkout — the TLS stack moved to `rustls-no-provider` + `ring`; `git pull` and rebuild |
| TUI renders garbled | Terminal too small or no UTF-8 — try `--ascii`, widen the window |
| `openpty:` errors on Android | Rare on modern Android; ensure Termux is updated (`pkg upgrade`) |
| `vikram doctor` reports missing tools | Install the named package; hints are per-OS (`apt`/`brew`/`pkg`/`choco`) |
| Network calls fail | Check `HTTPS_PROXY`/`HTTP_PROXY`; rustls needs no OS cert config |
