# Store — Durable SQLite State

`vikram-store` is the single durable substrate. SQLite (bundled, WAL, foreign keys on).

## Tables

sessions, agents, teams, team_members, journal, users, profiles, entities,
active_identity, hive_sessions, hive_participants, hive_messages, projects,
session_workspaces, artifacts, file_changes, tickets, ticket_participants,
ticket_messages, assignments, ticket_events.

## Key rows

`SessionRow`, `AgentRow`, `TeamRow`, `UserRow`, `ProfileRow`, `EntityRow`,
`ActiveIdentity`, `HiveRow`, `HiveParticipantRow`, `HiveMessageRow`,
`ProjectRow`, `SessionWorkspaceRow`, `ArtifactRow`, `FileChangeRow`,
`TicketRow`, `TicketParticipantRow`, `TicketMessageRow`, `AssignmentRow`,
`TicketEventRow`.

## Journal

Append-only per session: seq, ts_ms, kind, effect (idempotency key), payload.
`SqliteJournal` implements `JournalSink`. `replay_messages` rehydrates a session.

## Concurrency

Single process, one `Arc<Mutex<Connection>>`. Transactions for multi-table writes.

## Lifecycle

`Store::open(path)`, `open_in_memory()`. Migrations in `migrate()` (CREATE IF NOT EXISTS
+ `ensure_column` for additive columns). Delete cascades are transactional.

## Files

- `crates/vikram-store/src/lib.rs`

## Tests

`crates/vikram-store/tests/sessions.rs`, `tickets.rs` (6 + 6 = 12 tests).