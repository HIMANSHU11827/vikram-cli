# 🧠 VIKRAM — Terminal AI Runtime

> A multi-user, multi-entity AI harness in Rust. One binary. Deny-first permissions.
> Durable SQLite state. Hive teams, bots, sub-agents and a unified Ticket System.

[![Rust](https://img.shields.io/badge/rust-1.86%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Edition](https://img.shields.io/badge/edition-2024-blue)]()
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green)]()
[![Status](https://img.shields.io/badge/status-pre--release-yellow)]()
[![Crates](https://img.shields.io/badge/crates-10-informational)]()

---

## Table of Contents

- [What is VIKRAM](#what-is-vikram)
- [Features](#features)
- [Quick Start](#quick-start)
- [Architecture](#architecture)
- [Permission Model](#permission-model)
- [Unified Ticket System](#unified-ticket-system)
- [Crates](#crates)
- [CLI Reference](#cli-reference)
- [Configuration](#configuration)
- [Documentation](#documentation)
- [Testing](#testing)
- [License](#license)

---

## What is VIKRAM

VIKRAM is a terminal AI runtime. A single binary (`vikram`) drives an interactive TUI,
headless chat, and a full set of operator commands. It is **multi-user and multi-entity**:
humans, a main orchestrator (Vikram), delegated sub-agents, dedicated bots and Hive
collaborative teams all share one runtime, one permission model and one durable store.

- **Journal-first** execution: append-only event log, idempotent effects, crash-safe.
- **Deny-first** permissions with a non-widenable mandatory floor.
- **Durable** SQLite state — crashes do not lose work.
- **Replaceable seams** (provider, policy, journal, tools) behind typed contracts.

---

## Features

| Area | Highlights |
|---|---|
| 🧠 **Runtime** | Turn loop, operation/effect kernel, cancellation, token meter, event bus |
| 🔐 **Security** | Deny-first policy, mandatory floor, 7 posture modes, durable allow-list, posture resolution |
| 💾 **Persistence** | SQLite (WAL), sessions, agents, teams, hives, tickets, journal, users, profiles, entities, projects, artifacts, file changes |
| 🎫 **Tickets** | One unified engine for Hive teams, bots and sub-agents — conversations, participants, permissions, assignments, reports, recovery |
| 🐝 **Hive** | Collaborative agent teams with shared conversation, parallel/sequential/specialized strategies |
| 🤖 **Bots** | Independent AI workers with their own Tickets and isolation |
| 👶 **Sub-agents** | Parent-managed workers with isolated execution and authorized reports |
| 🧩 **Skills** | Discovery, loading, activation, bundles, installation, history, rollback |
| 🔌 **MCP** | Client discovery/connections/tools + server surface |
| 🧠 **Memory** | Per user/profile/entity SQLite KV store |
| 📚 **Context** | Permission-aware markdown loader with mtime invalidation, token budgeting, dedup |
| 🌐 **Browser/PTY/Computer** | Headless Chromium, persistent PTYs, desktop automation |
| 🔧 **LSP/Search** | Language server pool, web search |
| 🧬 **Self-improvement** | Learning pipeline + evolution engine (experiences, reflection, skills, optimization) |
| 🖥️ **TUI** | ratatui interface, responsive layout, interactive input, widgets |

---

## Quick Start

### Build

```sh
cargo build --release
./target/release/vikram
```

Requires **Rust 1.86+**. SQLite is compiled in (`rusqlite/bundled`) — no system `libsqlite3`.

### First run

```sh
vikram                      # interactive TUI
vikram chat "explain this repo"
vikram doctor               # check prerequisites
```

### Operator commands

```sh
vikram sessions list
vikram agents
vikram entities list
vikram hive list
vikram tickets list
vikram config show
```

---

## Architecture

```
User (TUI / CLI / Chat)
  └─► Runtime::boot (store, memory, provider, MCP, plugins, skills, evolution)
        └─► SessionRuntime::begin_turn
              └─► Turn loop
                    ├─ inject skills / identity / preamble
                    ├─ stream provider (OpenAI-compatible SSE)
                    ├─ run tools under policy gate (+ approvals)
                    └─ journal results, repeat
        └─► Finish (usage, events, dequeue)
  └─► Shutdown (cancel all)
```

**Crate layers (no circular deps):**

```
vikram-contracts  (IDs, messages, policy, journal, trust, versioning, tickets)
   ▲
   ├── vikram-kernel   (operations, effects, grants, cancellation, token meter)
   ├── vikram-policy   (deny-first, floor, rules, posture)
   └── vikram-store    (SQLite durable state)
         ▲
         └── vikram-runtime  (host, providers, tools, MCP, plugins, skills, memory, context, learning, workspace, sandbox, tickets)
               ▲
               ├── vikram-tui   (ratatui interface)
               └── vikram-cli   (entry point + operator commands)
vikram-skills / vikram-mcp / vikram-evolution  (supporting crates)
```

**Core properties:**

- **Journal-first**: every effect is journalled before it is applied; an effect id already
  present means the action is skipped rather than repeated (crash-safe).
- **Auditable**: the journal is the record of what happened and what policy said.
- **Safe by construction**: permission is evaluated in the kernel against the trust of the
  content that produced the request.

---

## Permission Model

**Deny-first.** A `Deny` outranks every `Allow`; no rule means no permission. A mandatory
floor cannot be widened by config, plugins or substituted policy implementations:

- mutations outside the workspace root are always denied;
- untrusted content cannot authorize `grant.*` / `policy.*` / `self.*` verbs;
- delegation is cut off beyond `max_delegation_depth` (3).

**Postures:** `ask` (default) · `edit-ok` · `plan` (read-only) · `smart` ·
`unattended` · `yolo` (stops prompting, never overrides the floor) · `allow-list`.

---

## Unified Ticket System

One engine for all participant types. A Ticket is one persistent unit of work keeping the
original request, participants, messages, assignments, progress, execution history, reports,
follow-ups and results.

| Behaviour | Hive member | Independent bot | Sub-agent |
|---|---|---|---|
| Shared team chat | ✅ | ❌ | ❌ (default) |
| Read others' shared messages | ✅ | ❌ | ❌ |
| Direct collaboration | ✅ | ❌ | ❌ (default) |
| Work assignment | by coordinator | owns its own | by parent |
| Own execution context | ✅ | ✅ | ✅ |
| Submit reports | ✅ | ✅ | ✅ (to parent) |
| Parent coordination | team coordinator | — | parent agent |

**Communication patterns:**

- **Hive shared conversation** — authorized members read/participate in the shared team chat.
- **Independent bot conversation** — bot + authorized users only; no auto cross-bot access.
- **Sub-agent private execution + parent reports** — isolated contexts; parent receives
  authorized progress/reports; siblings cannot read each other's private messages.

**Strategies (Hive):** `parallel` · `sequential` · `specialized` (recorded on Ticket/Assignment).

**Persistence:** tickets, participants, messages, assignments and events live in SQLite and
survive restarts, session termination and agent failures. Follow-ups continue the existing
Ticket. Discord is an optional external surface; the canonical Ticket stays in Vikram.

📖 Full reference: [`docs/tickets.md`](docs/tickets.md)

---

## Crates

| Crate | Role |
|---|---|
| `vikram-contracts` | Stable typed contracts (IDs, messages, policy, journal, trust, versioning, tickets) |
| `vikram-kernel` | Single execution architecture: ordered operations, effects, journal, grants, cancellation |
| `vikram-policy` | Deny-first permission evaluation with non-widenable mandatory floor |
| `vikram-store` | Durable SQLite state (sessions, agents, teams, hives, tickets, journal, users, profiles, entities, projects, artifacts, file changes) |
| `vikram-runtime` | Host: sessions, agents, tasks, teams, hives, tickets, providers, tools, MCP, plugins, skills, memory, context, learning, workspace, sandbox |
| `vikram-skills` | Skill discovery, loading, activation, installation, bundles, history |
| `vikram-tui` | Terminal interface (ratatui): state, rendering, input, widgets, theme, layout |
| `vikram-cli` | The `vikram` command: TUI, headless chat, operator commands |
| `vikram-mcp` | Model Context Protocol server |
| `vikram-evolution` | Self-evolution engine: experiences, reflection, memory, skills, optimization, hive learning |

---

## CLI Reference

```sh
# Sessions / projects / workspaces
vikram sessions list|new|show|rename|archive|duplicate|rm
vikram project   list|select|show|disconnect
vikram workspace show|changes|restore
vikram files     list|rm

# Entities / agents / teams / hives
vikram agents
vikram entities  list|new|rm
vikram hive      list|new|show|add-participant|remove-participant|send|run|rm

# Tickets (unified)
vikram tickets   list|show|new|add-participant|send|assign|complete|close|reopen|rm

# Users / profiles
vikram users     list|new|rm
vikram profiles  list|new|rm

# Config / tools / skills / mcp
vikram config    show|init|set|path
vikram tools     [list]
vikram skills    ...
vikram mcp       list|status|serve
vikram allow     list|add|rm
vikram doctor
```

---

## Configuration

Config lives in `~/.vikram/config.json`:

```json
{
  "model": "...",
  "base_url": "...",
  "api_key_env": "OPENAI_API_KEY",
  "context_window": 128000,
  "workspace_root": "/work",
  "max_steps": 24,
  "max_output_tokens": 4096,
  "ascii_fallback": false,
  "thinking": false,
  "mode": "ask",
  "security": { "disable_yolo": false, "disable_auto_approve": false, "disable_always_allow": false }
}
```

Override via flags: `--model`, `--base-url`, `--workspace`, `--mode`, `--ascii`.

---

## Documentation

| Doc | Description |
|---|---|
| [`HARNESS.md`](HARNESS.md) | Architecture overview |
| [`docs/tickets.md`](docs/tickets.md) | Unified Ticket System (Hive, bots, sub-agents) |
| [`docs/session-workspace.md`](docs/session-workspace.md) | Sessions, workspaces, projects, artifacts, checkpoints |
| [`docs/evolution.md`](docs/evolution.md) | Self-evolution engine |
| [`docs/audit/VIKRAM-AZ-AUDIT.md`](docs/audit/VIKRAM-AZ-AUDIT.md) | Full A–Z audit |
| [`docs/skills/`](docs/skills/) | Skills system reference |
| [`docs/development/`](docs/development/) | Testing strategy, implementation notes |
| [`docs/research/`](docs/research/) | Competitor and architecture research |
| [`docs/decisions/`](docs/decisions/) | Architecture decision records |
| [`docs/mcp/`](docs/mcp/) | MCP overview, architecture, audit |

---

## Testing

```sh
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets
```

Suites: contracts · kernel · policy · store (sessions + tickets) · runtime (incl. ticket
workflows) · skills · tui · cli · mcp · evolution.

---

## License

MIT OR Apache-2.0