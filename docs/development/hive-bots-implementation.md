# HIVE & BOTS — Implementation Plan

> Status update (2026-11-03): a unified Ticket System has since been implemented
> (see `docs/tickets.md` and `README.md`), providing the shared engine for Hive
> teams, independent bots and sub-agents with conversations, participants,
> permissions, assignments, reports and recovery.

Mission: complete the HIVE (teams) and BOTS (agents) right-panel system inside the existing
native Rust TUI, wired to the real runtime. No mocks, no duplicates, no hardcoded names.

## Audit findings (source of truth)

- Runtime: `RuntimeHandle` (observation + explicit controls only), `AgentManager`/`ManagedAgent`,
  `TaskManager`, `SessionRuntime`, turn loop (`turn.rs`), `EventBus` (tokio broadcast, FIFO).
- Persistence: SQLite `Store` — `sessions`, `agents`, `journal` tables. No teams table.
- TUI: `App` view model folds `RuntimeEvent`s; `layout.rs` already collapses a BOTS panel;
  `panels.rs::bots_panel` renders agents with a global plasma pulse in the title (spec-forbidden)
  and maps Failed/Stopped agents to "Paused" (dishonest).
- No HIVE/team concept exists anywhere. `AgentRemoved` event and agent cancel token are dead;
  queued agent continuations lose their task id; `vikram agents` claims seeding that never happens.

## Decisions

1. A team owns a dedicated real session (its conversation). Team "working" = that session's turn
   is busy AND the team is not paused. Indicator visibility derives from runtime state only.
2. Team pause suspends the turn loop between steps (real); team stop cancels the session token
   (real). No timers, no simulation.
3. Per-team animation frames advance only while that team works; hidden otherwise.
4. BOTS statuses: active → Thinking/Working per the configured provider thinking flag;
   Paused overrides; Completed → Finished; Failed/Stopped shown honestly (never Finished).
5. Panel: one bordered side panel, HIVE above BOTS, each section hidden when empty, whole panel
   hidden when both empty, Ctrl+B toggles, independent windowed scrolling, mouse hit regions.
6. Management: `/team` and `/bot` slash commands + CLI subcommands → real `RuntimeHandle`
   methods → store. The right panel itself stays a selector/overview (no buttons).

## Change chain (dependency order)

1. `vikram-contracts`: `TeamId`.
2. `vikram-store`: `teams` + `team_members` tables, `TeamRow`, CRUD.
3. `vikram-runtime/types.rs`: `TeamSnapshot`, `RuntimeEvent::TeamChanged/TeamRemoved`,
   `StateSnapshot.teams`.
4. `vikram-runtime/session.rs`: `paused` flag.
5. `vikram-runtime/hive.rs` (new): `ManagedTeam`, `TeamManager`.
6. `vikram-runtime/runtime.rs`: team load/create/rename/delete/members/control, agent
   create/update/remove, team unlink on session close, snapshot includes teams.
7. `vikram-runtime/turn.rs`: observe agent cancel token, honor session pause, keep task id on
   queued continuations.
8. `vikram-tui`: app state/events/commands, layout (`side` region), `side_panel` widget,
   focus/keys/mouse, theme glyph.
9. `vikram-cli`: `execute()` mapping, slash commands, mouse hits, `teams`/`agents` subcommands,
   fix the false seeding message.
10. Tests at every layer; then fmt/check/clippy/test; then live pty verification with a local
    mock SSE provider.

## Acceptance mapping

Every item in the mission's §32 checklist is covered by a test or a verification step listed in
the final report.
