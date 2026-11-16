# VIKRAM

**A personal AI runtime for the terminal.** One Rust binary that runs an interactive TUI, a
headless chat, a multi-platform message gateway, and a scheduled-job daemon — all over one
deny-first permission model and one durable SQLite store.

VIKRAM is built to be a presence, not a tool: a personal assistant that remembers context across
sessions, delegates work to bots and agent teams, answers from your own machines' messaging apps,
and runs scheduled jobs while you're away. It is honest by design — it reports what it actually
did, asks before it acts, and never claims a success it didn't earn.

[![Rust](https://img.shields.io/badge/rust-1.89%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Edition](https://img.shields.io/badge/edition-2024-blue)]()
[![Status](https://img.shields.io/badge/status-pre--release-yellow)]()
[![Tests](https://img.shields.io/badge/tests-2000%2B-brightgreen)]()

---

## Why VIKRAM

Most terminal AI tools are a prompt box with tools bolted on. VIKRAM is a runtime:

- **Journal-first execution** — every effect is recorded before it is applied; a crash mid-turn
  resumes from the journal instead of repeating or losing work.
- **Deny-first permissions** — a `Deny` outranks every `Allow`, and a mandatory security floor
  cannot be widened by config, plugins, or policy files. Untrusted content can never grant itself
  `grant.*`, `policy.*`, or `self.*` verbs.
- **Real multi-entity work** — hives (agent teams), bots (persistent workers), and sub-agents
  share one Ticket engine: conversations, assignments, reports, and recovery all live in SQLite.
- **Beyond the terminal** — a gateway answers Telegram, Discord, Slack, WhatsApp, email, IRC,
  matrix and ~30 more platforms, behind per-sender authorization. A daemon runs cron-scheduled
  agent jobs with retries, delivery tracking, and heartbeats.
- **Honest UI** — every drawn control has a handler, every key printed has an effect, and panels
  show live data or say why they can't.

---

## Quick start

```sh
cargo build --release
./target/release/vikram
```

Requires **Rust 1.86+**. SQLite is bundled (`rusqlite/bundled`) — no system dependencies.
Full per-OS install guide (Linux, macOS, Windows, Termux): [`docs/install.md`](docs/install.md)

```sh
vikram                    # interactive TUI
vikram chat "explain this repo"
vikram chat --interactive # REPL-style session
vikram doctor             # check prerequisites and config
```

First run creates `~/.vikram/` with config, database, and journal. Point it at a model with
`--model`, `--base-url`, or `VIKRAM_*` env vars — any OpenAI-compatible endpoint works.

---

## The terminal interface

`vikram` opens a responsive TUI: session tabs, transcript, optional BOTS and HIVE panes, a notice
row, and a composer with an explicit send mode (`Tab` flips between *queue* and *steer* — the
border title always says which is armed).

- **35 slash commands** behind one table — popup, `/help`, and palette can't disagree.
- **Mouse = keyboard** — every chip, tab, picker row, suggestion, approval request, and pane
  records its hit region while drawing; a click runs the same handler the chord does.
- **Honest panels** — read-only views show no fake verbs; empty states explain themselves;
  pickers scroll, filter, and answer clicks.
- **Approvals that stay reachable** — pending permission requests queue up and each is answerable
  by key or click; `a`/`d`/`p` chords and `allow` rules persist what you decide.

Key map and hit-region contract: [`docs/tui.md`](docs/tui.md)

---

## Architecture

```
User (TUI / headless chat / gateway platforms / daemon jobs)
  └─► Runtime::boot ── store · memory · provider · MCP · plugins · skills · evolution
        └─► Turn loop ── skills/identity injection → provider stream → tools under policy → journal
  └─► Shutdown (cancel all, flush journal)
```

Crate layers, acyclic:

```
vikram-contracts   IDs, messages, policy types, journal, trust, tickets
vikram-policy      deny-first evaluation + non-widenable mandatory floor
vikram-store       SQLite durable state (WAL)
vikram-runtime     host: sessions, agents, providers, tools, MCP, plugins, skills, memory,
                   context engine, learning, workspace, sandbox, tickets
vikram-gateway     ~30 platform adapters + access control + ingress
vikram-daemon      scheduled jobs: claim/settle transactions, retries, delivery, heartbeats
vikram-evolution   experience → reflection → proposals learning pipeline
vikram-tui         ratatui interface
vikram-cli         the `vikram` binary: TUI, chat, operator commands, gateway, daemon, mcp serve
supporting         vikram-skills · vikram-mcp · vikram-sandbox · vikram-terminals ·
                   vikram-codeindex · vikram-acp · vikram-bench
```

---

## Security model

- **Deny-first policy** with posture modes: `ask` (default), `edit-ok`, `plan` (read-only),
  `smart`, `unattended`, `yolo` (stops prompting, never overrides the floor), `allow-list`.
- **Mandatory floor** — mutations outside the workspace root are always denied; delegation depth
  is capped; shell commands that smuggle redirection/substitution past an allow rule are caught
  by segment-level matching.
- **Gateway authorization** — every inbound platform message passes `authorize_source`
  (default-deny) before it can reach the runtime. Allow senders via `GATEWAY_ALLOWED_USERS`,
  `GATEWAY_ALLOW_ALL_USERS=true`, per-platform `<PLATFORM>_ALLOWED_USERS`, or pairing approvals.
- **Sandboxed execution** — shell/PTY/verify tools honour `ctx.sandbox` (bubblewrap/seatbelt
  where available); timeouts kill the whole process group.
- **Plugin isolation** — plugin ids are validated and canonicalized under the plugin root;
  `plugin.uninstall` cannot escape it.

📖 [`docs/security.md`](docs/security.md) · [`docs/permissions.md`](docs/permissions.md) ·
[`docs/gateway-platforms.md`](docs/gateway-platforms.md)

---

## Entities and work

| Concept | What it is |
|---|---|
| **Session** | One conversation with full history, workspace, and journal |
| **Bot** | Persistent worker with its own context files and Tickets |
| **Sub-agent** | Delegated worker under a parent, isolated execution, authorized reports |
| **Hive** | Team chat of agents/bots with shared authorized context |
| **Ticket** | One durable unit of work shared across all participant types |
| **Entity** | User/profile-scoped named thing (agent, bot, org, project…) with scaffolded context |

Tickets carry conversations, participants (with `can_read`/`can_write` enforced), assignments,
reports, follow-ups, and recovery — in SQLite, surviving restarts.

---

## Gateway and daemon

**Gateway** (`vikram gateway`) connects the runtime to messaging platforms — Telegram, Discord,
Slack, WhatsApp (Baileys + Cloud API), email, IRC, matrix, Signal, Google Chat, Teams, and more.
Webhook signatures are verified per platform; `/a2a` requires `A2A_TOKEN`. Inbound senders are
authorized centrally; replies route back to the originating platform.

**Daemon** (`vikram cron`) runs scheduled agent jobs — prompts or shell commands on cron
expressions, with retries, per-job delivery targets (session report, webhook, none), skip-on-
overdue semantics, and heartbeat monitoring. All scheduler writes are transactional.

---

## CLI surface

```sh
vikram                  # TUI
vikram chat "…"         # one-shot headless turn
vikram sessions|project|workspace|files …
vikram agents|entities|users|profiles …
vikram hive|tickets …   # teams and unified work items
vikram bots …           # bot lifecycle
vikram cron …           # scheduled jobs (add/list/edit/remove/run)
vikram gateway …        # platform gateway
vikram plugins|skills|mcp|tools …
vikram config|allow|doctor|learning …
```

Every mutating verb verifies the target exists before claiming success.

---

## Configuration

`~/.vikram/config.json`:

```json
{
  "model": "…", "base_url": "…", "provider": "openai",
  "api_key_env": "OPENAI_API_KEY",
  "context_window": 128000, "workspace_root": "/work",
  "max_steps": 24, "max_output_tokens": 4096,
  "mode": "ask", "thinking": false, "ascii_fallback": false
}
```

Flags: `--model --base-url --workspace --mode --ascii`. See [`docs/cli.md`](docs/cli.md).

---

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets
```

Rust 2024 edition, MSRV 1.86 (toolchain 1.89+ recommended). Tests live in crate-local
`#[cfg(test)]` modules; the workspace currently passes 2000+ tests.

Project conventions for agents: [`AGENTS.md`](AGENTS.md) · refinement log:
[`REFINEMENT.md`](REFINEMENT.md)

---

## Documentation

| Doc | Contents |
|---|---|
| [`HARNESS.md`](HARNESS.md) | Architecture deep-dive |
| [`docs/tui.md`](docs/tui.md) | TUI layout, keys, mouse contract, commands |
| [`docs/concepts.md`](docs/concepts.md) | Users, profiles, entities, sessions, permissions |
| [`docs/tickets.md`](docs/tickets.md) | Unified Ticket System |
| [`docs/agents.md`](docs/agents.md) | Agents, bots, hives, assignments |
| [`docs/permissions.md`](docs/permissions.md) | Policy model, postures, floor, allow-list |
| [`docs/security.md`](docs/security.md) | Security model and known gaps |
| [`docs/gateway-platforms.md`](docs/gateway-platforms.md) | Platform adapters, auth env, webhook routes |
| [`docs/runtime.md`](docs/runtime.md) | Turn loop and runtime host |
| [`docs/store.md`](docs/store.md) | SQLite schema and durability |
| [`docs/tools.md`](docs/tools.md) | Tool surface |
| [`docs/plugins.md`](docs/plugins.md) · [`docs/skills/`](docs/skills/) | Extension systems |
| [`docs/memory.md`](docs/memory.md) · [`docs/context.md`](docs/context.md) | Memory and context management |
| [`docs/evolution.md`](docs/evolution.md) | Learning pipeline |
| [`docs/cli.md`](docs/cli.md) | Operator command reference |
| [`docs/session-workspace.md`](docs/session-workspace.md) | Workspaces, projects, checkpoints |
| [`docs/roadmap.md`](docs/roadmap.md) | Priorities |
| [`docs/docker.md`](docs/docker.md) · [`docs/kubernetes.md`](docs/kubernetes.md) · [`docs/termux.md`](docs/termux.md) | Deployment guides |

---

## License

MIT OR Apache-2.0
