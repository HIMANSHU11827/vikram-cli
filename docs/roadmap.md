# VIKRAM Roadmap

This roadmap is written so **anyone can pick up a task and ship it**. Each item has a
scope, files to touch, acceptance criteria and (where useful) a starting point.

Status legend: ✅ done · 🟡 in progress · ⬜ not started · 🔬 research.

---

## 0. Vision

VIKRAM should run **anywhere a developer works**: Linux, macOS, Windows, Termux/Android,
inside Docker containers and VMs, from any shell (bash, zsh, fish, cmd, PowerShell,
Termux sh), over SSH, in CI. Same binary, same behaviour, platform-aware degradation
where a feature genuinely cannot run (e.g. desktop automation on a headless server).

---

## 1. Current state (2026-11-03)

- Builds on Linux/macOS (primary). Windows compiles for most code paths but has gaps
  (see §3). Termux works with caveats.
- 10 crates, ~15k LOC, ~900 tests, all passing.
- Unified Ticket System (Hive/bots/sub-agents) implemented.
- Cross-platform gaps documented in §3.

---

## 2. Platform support matrix

| Platform | Status | Notes |
|---|---|---|
| Linux (x86_64, aarch64) | ✅ Primary | Full support, bubblewrap sandbox |
| macOS (x86_64, aarch64) | ✅ | Seatbelt sandbox, browser paths |
| Windows 10/11 (x86_64) | 🟡 Partial | PTY shell default, skills home, LSP URIs, plugin runners, computer module |
| Termux / Android | 🟡 Partial | Works via HOME/sh; desktop automation unsupported; PATH helpers |
| Docker (linux) | ⬜ | Should work; needs image + docs |
| VMs (any guest OS) | ⬜ | Depends on guest OS support |
| WSL | 🟡 | Treated as Linux; sandbox may need tuning |

---

## 3. Cross-platform work items (pick these up)

### P0 — make Windows actually work

- [x] **PTY default shell on Windows.** `crates/vikram-runtime/src/pty.rs:76-80`. ✅ Implemented.
  Default to `%COMSPEC%` (cmd.exe) or `powershell.exe` when `sh` is absent. Acceptance:
  `vikram` PTY session opens on a clean Windows install without Git Bash.
- [x] **Skills home resolution.** `crates/vikram-skills/src/paths.rs:18-32`. ✅ Implemented (USERPROFILE fallback).
  Add `USERPROFILE` fallback (mirror `config.rs::vikram_home`). Acceptance: skills
  discovered under `%USERPROFILE%\.vikram` on Windows.
- [x] **LSP file URIs on Windows.** `crates/vikram-runtime/src/lsp.rs:254-266`. ✅ Implemented.
  Convert `\` → `/`, prepend `/` for drive letters (`file:///C:/...`). Acceptance: LSP
  server receives valid URIs and navigation works.
- [x] **`which()` Windows-aware.** `crates/vikram-runtime/src/computer.rs:83-92` and
  `plugins.rs`. Append extensions from `PATHEXT`. Acceptance: helper detection works for
  `.exe`/`.cmd`/`.bat`.
- [x] **Plugin runners on Windows.** `crates/vikram-runtime/src/plugins.rs:531-551`. ✅ Implemented.
  Resolve `python`/`python3`, allow `.cmd`/`.bat`. Acceptance: a Python plugin runs on
  Windows.
- [x] **Computer module platform status.** `crates/vikram-runtime/src/computer.rs`. ✅ Implemented.
  Report "unsupported on this platform" on Windows/Termux instead of apt hints.
  Acceptance: `computer status` is honest on non-Linux.

### P1 — quality & coverage

- [ ] **Shell tool shell selection.** Already uses `cmd /C` on Windows, `sh -c` elsewhere.
  Add fish/zsh awareness? Keep simple: respect `$SHELL` on Unix, `%COMSPEC%` on Windows.
- [x] **Docker image + docs.** Add `Dockerfile` and `docs/docker.md`. ✅ Scaffolding done (image build pending network).
- [ ] **CI matrix.** GitHub Actions on ubuntu/macos/windows (and maybe termux via qemu). (pending)
  Acceptance: `cargo test --workspace` green on all three.
- [ ] **Windows sandbox (Job Objects).** Optional isolation. Acceptance: documented.
- [x] **Termux docs.** `docs/termux.md` with install steps (`pkg install rust clang`). ✅ Done.
- [ ] **PowerShell/cmd examples** in docs for Windows users.

### P2 — polish

- [ ] Cross-platform path tests.
- [ ] Avoid `/tmp` assumptions; use `std::env::temp_dir()`.
- [ ] Windows console VT handling / truecolor detection in TUI.

---

## 4. Feature roadmap (from audit)

See `docs/audit/DEVELOPMENT_ROADMAP.md` for the full prioritized P0-P3 list (with
complexity), and `docs/audit/VIKRAM-AZ-AUDIT.md` §19/§21 for the dependency graph.
Highlights:

**P0 (critical):** kernel trust propagation, MCP serve implementation, MCP stdio,
silent-error fixes, OS sandbox, verification loop, durable mid-turn resume.

**P1 (high):** consolidate team model, unify learning, wire evolution, code indexing,
ACP client+server, scheduling/triggers, local model support, git integration,
test discovery/execution, build validation, checkpoint/undo, code review agent.

**P2 (medium):** agent-to-agent messaging, independent review agent, memory
consolidation, semantic memory, hybrid search, knowledge graph, A2A.

**P3 (low):** polish, configurability, dead-code removal, property/fuzz/concurrency/
security/E2E tests.

**P4 (research):** crash-recovery state machine, Wasm components, VRP protocol, TUF,
deterministic token meter, taint labels.

---

## 5. How to contribute

1. Pick an item from §3 or §4.
2. Create a branch: `git checkout -b feat/<short-name>`.
3. Follow existing patterns in the relevant crate.
4. Add tests (unit in crate `#[cfg(test)]`, integration in `tests/`).
5. Run: `cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace`.
6. Open a PR. Keep changes focused; one item per PR.

### Good first issues

- P0 cross-platform items in §3 (PTY shell, skills home, LSP URIs, which()).
- Docs: improve any `docs/*.md`.
- Tests: add Windows-path unit tests behind `#[cfg(windows)]`.

### Conventions

- Rust 2024, rust-version 1.86.
- No comments unless necessary.
- Deny-first permissions; never widen the floor.
- Backend enforces security, not UI.
- One Ticket System (no separate engines).

---

## 6. Definition of done

An item is done when:

- Code compiles on Linux, macOS and Windows (`cargo build --workspace`).
- Tests pass (`cargo test --workspace`).
- clippy clean (warnings acceptable, no errors).
- Docs updated (README + relevant docs/*).
- Cross-platform behaviour verified or explicitly degraded with a clear message.

---

## 7. Owners / contact

Repository: https://github.com/HIMANSHU11827/vikram-cli
License: MIT OR Apache-2.0