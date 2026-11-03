# Vikram Overview — Architecture & Capabilities

## What Vikram is

A terminal AI runtime in Rust. Single binary (`vikram`) driving an interactive TUI,
headless chat, and operator commands. Multi-user, multi-entity (main agent, sub-agents,
bots, Hive teams) sharing one runtime, one permission model, one durable store.

## Workspace (10 crates)

vikram-contracts, vikram-kernel, vikram-policy, vikram-store, vikram-runtime,
vikram-skills, vikram-tui, vikram-cli, vikram-mcp, vikram-evolution.

## Core architecture

- **Contracts** (`vikram-contracts`): IDs, messages, policy vocabulary, journal, trust,
  versioning, tickets. Stable seam — components declare contracts; host refuses newer
  majors.
- **Kernel** (`vikram-kernel`): ordered operations over a shared RunContext, effects
  journalled before applied, idempotency via effect ids, grants, cancellation, token
  meter. The only thing that may act on effects.
- **Policy** (`vikram-policy`): deny-first evaluator + non-widenable MandatoryFloor +
  posture resolution + amendment parsing.
- **Store** (`vikram-store`): SQLite (WAL, FKs) durable state; ~20 tables.
- **Runtime** (`vikram-runtime`): host integration (sessions, agents, tasks, teams, hives,
  tickets, providers, tools, MCP, plugins, skills, memory, context, learning, workspace,
  sandbox).
- **Skills** (`vikram-skills`): discovery, loading, activation, bundles, install, history.
- **TUI** (`vikram-tui`): ratatui interface.
- **CLI** (`vikram-cli`): clap entry point.
- **MCP** (`vikram-mcp`): MCP server.
- **Evolution** (`vikram-evolution`): self-evolution (experiences, reflection, memory,
  skills, optimization, hive learning).

## Core entities

- User (human account, isolation boundary).
- Profile (working context of a user).
- Main Agent / Vikram (orchestrator).
- Agent (sub-agent worker).
- Bot (dedicated-purpose assistant).
- Hive (group chat, not an entity).
- Team (reusable group config).
- Ticket (persistent unit of work).
- Session (live conversation).

Each entity keeps name, title, role, specialization, description, status as separate
fields; titles are metadata, never permissions.

## Execution flow

User → Runtime::boot → SessionRuntime::begin_turn → Turn loop (inject skills/identity,
stream provider, run tools under policy gate, journal results, repeat) → Finish (usage,
events, dequeue) → Shutdown.

## Permission model

Deny-first + mandatory floor (workspace root, extra write roots, forbidden-from-untrusted
verbs, max delegation depth 3) + 7 postures + durable allow-list. Enforced in kernel
against originating trust.

## Unified Ticket System

One engine for Hive teams, independent bots, sub-agents: tickets, participants,
messages (shared/private/internal), assignments (linked to parent ticket), events.
Backend-enforced visibility. Strategies: single/parallel/sequential/specialized.
Persisted in SQLite; survives restarts. See docs/tickets.md.

## Memory

Isolated per user/profile/entity in SQLite. Learning (approval-gated proposals) + Evolution
(experiences, reflection, memory, skills, optimization, hive learning).

## Context loader

Permission-aware markdown loader with mtime invalidation, token budgeting, dedup,
entity scaffolding.

## Skills

Discovery (roots, precedence), loading, activation (selectors), trust policy, bundles,
installation (git), history/rollback.

## MCP

Client: discovery, connections, tools. Server: expose approved tools over STDIO (serve
currently a stub).

## Plugins

Manifests, lifecycle, dependencies, integrity.

## TUI

ratatui + crossterm. Sessions, transcript with foldable activity rows + live turn clock,
agent/Hive panels, approvals, questions, notices, ASCII fallback.

## CLI

TUI, chat (headless), sessions, project, workspace, files, agents, config, doctor,
allow, tools, mcp, skills, users, profiles, entities, hive, tickets.

## Cross-platform

Linux (primary, bubblewrap), macOS (Seatbelt), Windows (cmd/PowerShell, PATHEXT),
Termux (sh, HOME-based), Docker/Kubernetes scaffolding. Desktop automation Linux-only.

## Testing

~900 tests across crates. No integration/E2E/property/fuzz/concurrency/security tests
yet. See docs/development/testing-strategy.md.