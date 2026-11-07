# VIKRAM Roadmap

This roadmap is written so **anyone can pick up a task and ship it**. Each item has a scope,
acceptance criteria and (where useful) a starting point.

Status legend: ✅ done · 🟡 in progress · ⬜ not started · 🔬 research.

---

## 0. Vision

VIKRAM should run **anywhere a developer works**: Linux, macOS, Windows, Termux/Android,
inside Docker containers and VMs, from any shell (bash, zsh, fish, cmd, PowerShell,
Termux sh), over SSH, in CI. Same binary, same behaviour, platform-aware degradation
where a feature genuinely cannot run (e.g. desktop automation on a headless server).

Guiding rules:

- Never fabricate a capability. A refusal with a reason beats a stub that reports success.
- Every claim has a test or a reproducible command. External-framework claims stay `unverified`
  until evidenced in this repository.
- Offline-friendly verification: mock transports for wire behaviour, real local servers for
  end-to-end paths.
- `cargo fmt` clean, **zero** clippy warnings, whole workspace green before a phase closes.

---

## 1. Current state (2026-11-07)

- 16 crates, ~172k lines of Rust, **2,523 tests passing**, clippy clean, `rust-version = 1.89`.
- Builds on Linux/macOS; Windows compiles with the gaps listed in §3; Termux works with caveats.
- Gateway: **27 messaging platforms wired** (Telegram, Discord, Slack, WhatsApp bridge + Cloud,
  Matrix, Mattermost, Signal, BlueBubbles, ntfy, Twilio SMS, Feishu, LINE, DingTalk + Stream,
  Google Chat, Teams, QQ Bot, Photon, Weixin, WeCom, SimpleX, IRC, Yuanbao, Buzz/Nostr, raft,
  A2A, email), with pairing, access control, routing and a delivery ledger.
- Capabilities: **media 20/28 modules**, **rich cards 5**, **reactions 5**, **polls 1**,
  **draft streaming 2**, and inbound images reaching the model through a guarded fetcher.
- Runtime: OpenAI-compatible + native Anthropic Messages providers; sandbox; memory with hybrid
  recall; code index; browser (CDP) and computer-use tools.
- Tooling: `vikram-bench` (13 real operations) and `docs/comparison.md` with an evidence legend.

---

## 2. Platform support matrix

| Platform | Status | Notes |
|---|---|---|
| Linux (x86_64, aarch64) | ✅ Primary | Full support, bubblewrap sandbox |
| macOS (x86_64, aarch64) | ✅ | Seatbelt sandbox, browser paths |
| Windows 10/11 (x86_64) | 🟡 Partial | PTY shell default, skills home, LSP URIs, plugin runners, computer module |
| Termux / Android | 🟡 Partial | Works via HOME/sh; desktop automation unsupported; PATH helpers |
| Docker (linux) | 🟡 | Scaffolding done; image build/docs pending |
| VMs (any guest OS) | ⬜ | Depends on guest OS support |
| WSL | 🟡 | Treated as Linux; sandbox may need tuning |

---

## 3. Cross-platform work items (pick these up)

### P0 — make Windows actually work

- [x] **PTY default shell on Windows.** `crates/vikram-runtime/src/pty.rs`. Default to
  `%COMSPEC%` / `powershell.exe` when `sh` is absent. ✅
- [x] **Skills home resolution.** `crates/vikram-skills/src/paths.rs`. `USERPROFILE` fallback. ✅
- [x] **LSP file URIs on Windows.** `crates/vikram-runtime/src/lsp.rs`. Drive-letter URIs. ✅
- [x] **`which()` Windows-aware.** `crates/vikram-runtime/src/computer.rs`, `plugins.rs`.
  `PATHEXT` extensions. ✅
- [x] **Plugin runners on Windows.** `crates/vikram-runtime/src/plugins.rs`. ✅
- [x] **Computer module platform status.** Honest "unsupported" on Windows/Termux instead of
  Linux apt hints. ✅

### P1 — quality & coverage

- [ ] **Shell tool shell selection.** Respect `$SHELL` on Unix, `%COMSPEC%` on Windows.
- [ ] **CI matrix.** GitHub Actions on ubuntu/macos/windows (Termux optional). Acceptance:
  `cargo test --workspace` green on all three, with browser/computer tests skipped explicitly.
- [ ] **Windows sandbox (Job Objects).** Optional isolation; documented behaviour.
- [ ] **PowerShell/cmd examples** in docs for Windows users.
- [x] **Termux docs.** `docs/termux.md`. ✅
- [x] **Docker scaffolding.** `Dockerfile` present; image build pending network parity.

### P2 — polish

- [ ] Cross-platform path tests.
- [ ] Avoid `/tmp` assumptions; always `std::env::temp_dir()`.
- [ ] Windows console VT handling / truecolor detection in the TUI.

---

## 4. Product roadmap (gateway, runtime, providers)

### P0 — trust and correctness hardening

1. **Inbound media hardening** ⬜ — resolve-and-recheck hosts before connecting (DNS rebinding),
   re-guard redirect targets, enforce the byte cap while streaming, and deliver audio/video/PDF
   instead of skipping them. Tests: mock resolver, redirect-to-private, mid-stream cap, each mime.
2. **Delivery ledger coverage for new verbs** ⬜ — route `send_media`, `send_card`, `send_poll`,
   `send_draft`, reactions through the same retry/partial-overflow path as text; record draft ids.
3. **Provider robustness + conformance suite** 🟡 — Anthropic retry loop (`Retry-After`, 529) and a
   shared suite both providers must pass. Anthropic has 32 tests today.
4. **ACP depth** ⬜ — initialize, sessions, streaming updates, cancellation, errors under test.
5. **Codec fuzzing** ⬜ — property tests for `truncate_message`, Yuanbao protobuf, JWT parsing and
   multipart builders: random bytes error, never panic.

### P1 — feature parity depth

6. **Voice notes and multi-attachment sends** ⬜ — `send_voice` (Telegram `sendVoice`, WhatsApp
   `ptt`, Matrix MSC3245) and `send_media_batch` (albums/media groups) with truthful per-item results.
7. **Rich cards for remaining platforms** 🟡 — 5 done (Slack, Feishu, Teams, Google Chat, Discord);
   open: Mattermost attachments, DingTalk AI cards, LINE Flex, WeCom templates, WhatsApp templates,
   Yuanbao.
8. **Reactions for remaining platforms** 🟡 — 5 done (Telegram, Discord, Slack, Matrix, Feishu);
   open: WhatsApp Cloud, Mattermost, Signal, Teams (only with a contract carrying author/timestamp
   where required).
9. **Polls** 🟡 — WhatsApp bridge done; Telegram, Discord, Signal, Feishu open (implement against the
   platform API and label them ported-from-API, not from the vendored reference).
10. **Last media adapters** 🟡 — 20/28 done; open: A2A file parts, Buzz CLI subprocess, Google Chat
    user-OAuth upload, DingTalk stream markdown, with IRC/ntfy/raft/SMS documented as no-API.
11. **A2A streaming and artifacts** ⬜ — `message/stream` as SSE task updates or a tested, documented
    subset; file parts on `message/send` and artifacts.

### P2 — platform and ecosystem

12. **Providers beyond two** 🟡 — OpenAI-compatible + Anthropic done; Gemini, Bedrock (SigV4) and
    native Ollama open, all behind the conformance suite; model capability catalog (context window,
    vision, tools) so the runtime stops guessing.
13. **Evolution actuators** 🟡 — MAPEK only re-queues failed reflection jobs for real; give
    self-adaptation a config control handle and verify post-conditions before reporting success.
14. **Performance regression gate** 🟡 — `vikram-bench` exists (chunker now linear, ~0.26 ms/64 KiB);
    open: CI thresholds, `memory.recall` profiling (~0.72 ms), optional cross-framework harness that
    actually runs other CLIs before any comparative number is published.
15. **TUI/UX polish** 🟡 — shared activity indent done; open: streaming/approval ergonomics,
    accessibility, session search, documented keymap.

### P3 — operations

16. **CI/CD** ⬜ — matrix build, MSRV enforcement, fmt/clippy/test gates, browser/computer tests
    conditionally skipped with reasons, nightly fuzz, release artifacts and checksums.
17. **Documentation** 🟡 — auto-generate the per-adapter fidelity table from module headers; gateway
    deployment/provider runbooks; keep `docs/comparison.md` honest (`unverified` until reproduced).

---

## 5. Explicitly out of scope

- Claiming to beat another framework without reproducible measurements.
- Faking media/card/reaction delivery when a platform lacks the API.
- Hand-rolling cryptographic signing where no vetted crate exists.

---

## 6. How to contribute

1. Pick an unchecked item from §3 or §4.
2. Create a branch: `git checkout -b feat/<short-name>`.
3. Follow existing patterns in the relevant crate.
4. Add tests (unit in crate `#[cfg(test)]`, integration in `tests/`).
5. Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets && cargo test --workspace`.
6. Open a PR. Keep changes focused; one item per PR.

### Good first issues

- Cross-platform items in §3 (shell selection, PowerShell docs, Windows path tests).
- Docs: improve any `docs/*.md`; generate the adapter fidelity table.
- Tests: Windows-path unit tests behind `#[cfg(windows)]`.

### Conventions

- Rust 2024, `rust-version = 1.89`.
- No comments unless necessary.
- Deny-first permissions; never widen the floor.
- Backend enforces security, not UI.
- One Ticket System (no separate engines).

---

## 7. Definition of done

An item is done when:

- Code compiles on Linux, macOS and Windows (`cargo build --workspace`), or the platform gap is
  explicitly documented.
- Tests pass (`cargo test --workspace`), including a new test for the behaviour.
- `cargo clippy --workspace --all-targets` reports **zero warnings**; `cargo fmt --check` is clean.
- Docs updated (README, `CONTINUE.md`, the relevant `docs/*`).
- Cross-platform behaviour verified or explicitly degraded with a clear message.

---

## 8. Owners / contact

Repository: https://github.com/HIMANSHU11827/vikram-cli
License: MIT OR Apache-2.0
