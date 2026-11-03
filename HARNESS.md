# HARNESS.md — Vikram CLI Architecture

## Overview

Vikram CLI is a terminal AI runtime built as a Rust workspace. It is a multi-user, multi-entity AI harness supporting Main Agents (Vikram), delegated Agents (sub-agents), dedicated Bots, and Hive group chats.

## Crates

- `vikram-contracts`: stable typed contracts (IDs, messages, policy, journal, trust, versioning, tickets).
- `vikram-kernel`: single execution architecture (operation loop, effect gate, grants, cancellation, token meter).
- `vikram-policy`: deny-first permission evaluation with mandatory floor.
- `vikram-store`: SQLite durable state (sessions, agents, teams, journal, users, profiles, entities, active identity, hive sessions/participants/messages, tickets, ticket participants/messages/assignments/events).
- `vikram-runtime`: central process state, sessions, agents, teams, providers, tools, memory, skills, MCP, plugins, context loader, hive manager, ticket manager, sandbox.
- `vikram-skills`: skill discovery, loading, activation, installation.
- `vikram-tui`: Ratatui terminal interface.
- `vikram-cli`: Clap CLI entry point.
- `vikram-mcp`: MCP server.
- `vikram-evolution`: self-evolution engine (experiences, reflection, memory, skills, optimization).

## Core Entities

- **User**: human using the CLI. Multiple users, isolated data.
- **Profile**: working context of a user. Multiple per user.
- **Main Agent (Vikram)**: primary orchestrator. Owns SOUL/INSTRUCTIONS/MEMORY/TOOLS.md.
- **Agent**: delegated sub-agent worker. Owns its own context files.
- **Bot**: dedicated-purpose AI assistant. Owns its own context files.
- **Hive**: group chat of agents/bots/mixed. Not an entity. Supports sequential and parallel execution.
- **Team**: reusable group configuration.
- **Ticket**: one persistent unit of work shared by Hive teams, independent bots and sub-agents.
  Carries owner, participants, permissions, conversation, assignments, progress, history, reports.

Every entity keeps identity, type, title, role, specialization, description and status as separate
fields. `name` is the individual identity; `title` is a customizable responsibility ("Lead Coder",
"Security Reviewer"); `role` is the type baseline; `specialization` lists expertise (comma-separated);
`status` is active/inactive/unavailable. A title change never changes identity or type. Titles are
descriptive metadata, never permissions — permissions are enforced independently by the runtime.

## Context Loading

Centralized `ContextLoader` (`vikram-runtime/src/context.rs`) resolves and loads only relevant, authorized Markdown files per request with caching, mtime-based invalidation, token budgeting, and duplicate prevention.

## Memory

Isolated per user/profile/entity. SQLite-backed (`MemoryStore`). Controlled creation/retrieval/update with ownership.

## Storage

SQLite for runtime state; Markdown for identity/instructions/preferences. Transactions for concurrent updates.

## Permissions

Deny-first policy + mandatory floor + posture modes. Runtime-enforced.

## Execution Flow

User -> Vikram -> (direct | agent delegation | bot invocation | hive) -> result -> user.

## CLI Commands

sessions, agents, config, doctor, allow, tools, mcp, skills, users, profiles, entities, hive, tickets.
