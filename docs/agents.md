# Agents, Bots & Hive

## Agent (sub-agent)

Delegated worker under a parent agent. Owns identity, title, role, specialization,
description, status and its own context files/memory. Works in its own execution context
and reports back to the parent. Does not automatically collaborate with other sub-agents.

## Bot

Dedicated-purpose AI assistant (e.g. coding bot). Owns its own context files. Works
independently by default; manages its own Tickets. No automatic bot-to-bot access.

## Hive

Group chat of agents/bots/mixed. Not an entity. Members share an authorized team
conversation; supports parallel, sequential and specialized collaboration. Persisted
through `HiveManager` (hive_sessions, hive_participants, hive_messages).

## Team

Reusable group configuration (JSON-backed `TeamManager`). Members are agent ids.

## Assignment

A unit of work assigned to an agent/bot within a Ticket. Linked to the parent Ticket.
Status: pending/assigned/in_progress/completed/failed/cancelled/blocked.

## Ticket

See `docs/tickets.md`. One persistent unit of work shared by all three participant
types through one engine.

## Lifecycle

- Main agent receives request → responds directly / delegates (sub-agent) / invokes bot /
  runs Hive.
- Sub-agent: `RuntimeHandle::spawn_agent(profile, task, session)` creates a dedicated
  session and runs a turn.
- Hive: `RuntimeHandle::hive_create` + `hive_add_participant` + `hive_run`.
- Bot: invoked via entity; owns its own Tickets.

## Files

- `crates/vikram-runtime/src/agents.rs`
- `crates/vikram-runtime/src/teams.rs`
- `crates/vikram-runtime/src/hive_chat.rs`
- `crates/vikram-runtime/src/tickets.rs`
- `crates/vikram-store/src/lib.rs` (agents, teams, hives, tickets tables)