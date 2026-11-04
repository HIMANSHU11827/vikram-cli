# Core Concepts

VIKRAM is multi-user and multi-entity. These definitions are the vocabulary used across
the codebase, CLI, TUI and documentation.

## User

A human account. Users never share private data. Every other table hangs off the user id,
which is the isolation boundary.

## Profile

One working context of a user. Preferences, memory and conversations belong to the
profile, never the account as a whole. A user may have multiple profiles.

## Main Agent (Vikram)

The primary orchestrator and a personal AI assistant with a real personality — a friend and
family member to the user, virtually present, still in active development. Not a robot and not
just a CLI; a proper AI assistant person, primarily private to the user while supporting
multiple users (family and friends). Owns its own context files (SOUL/INSTRUCTIONS/MEMORY/TOOLS.md).
Remembers and understands the user, helps with many tasks, and never sugarcoats — honest and
direct, like a true friend. Chats politely and clearly. Receives user requests and decides:
respond directly, delegate to a sub-agent, invoke a bot, or run a Hive team.

## Agent (sub-agent)

A delegated worker. Owns its own context files and memory. Works under a parent agent on a
specific task, in its own execution context. Does not automatically collaborate with other
sub-agents; reports back to its parent.

## Bot

A dedicated-purpose AI assistant (e.g. a coding bot). Owns its own context files. Works
independently by default — it does not automatically join teams or access other bots'
Tickets. A bot manages its own Tickets.

## Hive

A group chat of agents/bots/mixed. Hive is **not** an entity itself; it is a
collaborative team. Members share an authorized team conversation and can discuss,
delegate, share findings and coordinate. Supports parallel, sequential and specialized
collaboration strategies.

## Team

A reusable group configuration (members). A Hive may reference a team; teams can be saved
and reused.

## Ticket

One persistent unit of work. Carries the original request, owner, participants, permissions,
conversation (messages), assignments, progress, execution history, reports, follow-ups and
final result. Shared by Hive teams, independent bots and sub-agents through one engine.

## Session

A live conversation. Owns messages (journaled), turn state, tokens, cwd, model, agent
binding, project association and status (active/archived).

## Entity

A stored AI identity: main agent, agent or bot. Keeps `name` (identity), `title`
(responsibility), `role` (type baseline), `specialization`, `description`, `status` as
separate fields. Titles are descriptive metadata, never permissions.

## Permission / Posture

Deny-first policy with a non-widenable mandatory floor. Postures (ask, edit-ok, plan,
smart, unattended, yolo, allow-list) decide what to do with a prompt; the floor is never
overridden.

## Memory

Isolated per user/profile/entity in SQLite. Controlled creation, retrieval and update with
ownership.

## Journal

Append-only, per-session sequence of records (messages, tool calls/results, policy
decisions, approvals, status, compaction, cancellation, activity). The idempotency key
that makes crash recovery safe.