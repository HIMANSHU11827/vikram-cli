# Unified Ticket System — Hive, Bots and Sub-Agents

VIKRAM uses one common Ticket System for all work. A Ticket is one persistent unit of work
that keeps the original request, participants, messages, assignments, progress, execution
history, reports, follow-ups and results. Hive teams, independent bots and sub-agents all
use the same engine with different collaboration and visibility rules.

## Why one engine

Three different working models (Hive collaboration, independent bots, parent-managed
sub-agents) must not become three separate Ticket engines. The engine understands
owner, participant identities, participant types, permissions, assignments, parent
relationships, conversation references, execution history, status and reports. Different
participant types receive different access and collaboration behaviour through data and
permissions, not through separate code paths.

## Domain model

### Ticket

- `id` (TicketId), `owner_key`, `owner_kind`
- `title`, `description`, `request` (original user request)
- `kind`: `general` | `hive` | `bot` | `subagent`
- `status`: `open` | `in_progress` | `blocked` | `completed` | `failed` | `cancelled` | `reopened`
- `parent_ticket` (optional, links child to parent)
- `hive_id` (optional, links to a Hive)
- `session_id` (optional)
- `strategy`: `single` | `parallel` | `sequential` | `specialized`
- `result`, `failure_reason`
- `created_ms`, `updated_ms`, `closed_ms`

### Participant

A row in `ticket_participants`:

- `ticket`, `participant_key` (e.g. `user:...`, `entity:...`, `bot:...`, `hive:...`)
- `participant_kind`: `user` | `agent` | `bot` | `hive_member` | `subagent`
- `entity_id`, `user_id` (optional links)
- `role` (coordinator, member, owner, assignee, ...)
- `can_read`, `can_write`, `can_admin`
- `joined_ms`

### Message

A row in `ticket_messages`:

- `id`, `ticket`, `author_key`, `author_kind`
- `content`
- `visibility`: `shared` | `private` | `internal`
- `ts_ms`, `task_ref` (optional link to an assignment)

### Assignment

A row in `assignments`, linked to a parent Ticket:

- `id`, `ticket`, `parent_assignment`
- `assignee_key`, `assignee_kind`, `entity_id`
- `title`, `description`
- `status`: `pending` | `assigned` | `in_progress` | `completed` | `failed` | `cancelled` | `blocked`
- `strategy`, `result`, `error`
- `created_ms`, `updated_ms`, `completed_ms`

### Event

A row in `ticket_events`: audit/history entries (status changes, participant added/removed,
messages, assignment created/completed/failed).

## Visibility rules (backend-enforced)

- `shared`: every participant with `can_read` may read.
- `private`: author, owner and admins may read. (For sub-agent tickets the parent is the
  owner and may read; other sub-agents may not.)
- `internal`: only owner and admins may read.

Posting requires `can_write` (or ownership). Posting `internal` requires `can_admin` (or ownership).
Reading requires participation (or ownership/admin). Enforcement lives in `TicketManager`,
not in the UI.

## Participant models

### Hive — collaborative agent teams

Hive members share their authorized team conversation. A Hive ticket is created with all
members as participants with shared read/write permission and a chosen collaboration
strategy (`parallel`, `sequential`, `specialized`). Members can post shared messages, read
each other's shared messages, discuss approaches, share findings and coordinate. Private
notes stay private to their author (+ owner/admins).

### Independent bots

A bot owns its own Tickets. It receives, creates (when authorized), owns, updates,
executes, records progress, produces reports and completes/fails its Ticket. Bots do NOT
automatically collaborate or access each other's private Tickets. Cross-access requires
explicit authorization (adding the other party as a participant).

### Sub-agents — parent-managed workers

A parent agent owns the main Ticket and creates Assignments for sub-agents. Each sub-agent
receives its own Assignment and works in its own execution context. Sub-agents do not
automatically read each other's private messages. The parent receives authorized progress
and reports (assignment completion posts a shared `[report]` message to the parent Ticket).
Assignment completion does not automatically complete the main Ticket.

## Persistence and recovery

All ticket state lives in SQLite (`tickets`, `ticket_participants`, `ticket_messages`,
`assignments`, `ticket_events`). It survives application restart, session termination and
agent failures. Follow-ups continue the appropriate existing Ticket (reopen if needed).
The existing journal idempotency substrate prevents duplicate execution on recovery.

## Integration points

- `vikram_contracts`: IDs + enums.
- `vikram_store::Store`: durable CRUD.
- `vikram_runtime::TicketManager`: business logic + visibility enforcement.
- `vikram_runtime::RuntimeHandle`: `ticket_*` methods and `ticket_create_hive`,
  `ticket_create_bot`, `ticket_create_subagent_assignment`.
- `vikram_cli`: `vikram tickets ...` operator commands.

## Testing

Store-level (`crates/vikram-store/tests/tickets.rs`): 6 tests — create/get/list, status
lifecycle, participants + messages, assignments + events, cascade delete, child tickets.

Runtime-level (`crates/vikram-runtime/tests/tickets.rs`): 5 tests — bot independence and
privacy, Hive shared conversation, sub-agent isolation + parent report, persistence across
restart, unauthorized participant denial.

## CLI reference

See README.md "Unified Ticket System" section.

## Remaining limitations

- Collaboration strategies are recorded as metadata; automatic multi-agent orchestration
  execution is driven through the runtime handle rather than a fully autonomous scheduler.
- No dedicated per-ticket execution-attempt retry state machine beyond journal idempotency.
- No UI or external Discord gateway (per product decision).