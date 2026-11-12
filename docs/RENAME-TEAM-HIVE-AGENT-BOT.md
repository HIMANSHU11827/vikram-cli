# Rename: agent profile → bot, team → hive — DONE

Model (`docs/agents.md` + user): **agent** = sub-agent (delegated work, keeps
`AgentId`/`agent-*`/`agents/` entity dir), **bot** = profile with SOUL.md etc.
(`BotId`/`bot-*`/`bots/` table), **hive** = group (roster + chat).

## Completed

- `AgentId`→`BotId` (`bot-` prefix); `AgentId` re-added for sub-agents
- `AgentRow`→`BotRow`, `AgentProfile`→`BotProfile`, `AgentState`→`BotState`,
  `AgentManager`→`BotManager` (`bots.rs` module), `ManagedAgent`→`ManagedBot`,
  `AgentSnapshot`→`BotSnapshot`, `agent_snapshots`→`bot_snapshots`,
  `AgentControl`/`AgentAction`→`BotControl`/`BotAction`
- Store: `agents` table → `bots` (**migration ran on live vikram.db**),
  `save_bot`/`list_bots`/`delete_bot`
- TUI/CLI: `/bot`(+bots/agents/agent), `/hive`(+hives/team/teams),
  `vikram bots`, `app.bots`, `selected_bot`, `AddBot`, all strings say
  bot(s)/hive; `PickerKind::Agent`/`AgentAction` deleted
- Roster (was "team"): `Team`→`HiveRoster`, `TeamManager`→`HiveRosterManager`
  (`hive_roster.rs`), `TeamId`→`HiveRosterId` (`hrost-` prefix),
  `teams.json`→`hive-roster.json` (**legacy teams.json still read**),
  `team_*`→`hive_roster_*` methods, `TeamRequest`→`HiveRosterRequest`,
  `TeamOrder`→`HiveRosterOrder`, daemon messages say hive
- Store roster API: `TeamRow`→`HiveRosterRow`, `teams`→`hive_rosters`,
  `team_members`→`hive_roster_members` (+ `team` col→`roster`) —
  **migrations added in `migrate()`**
- Tools: removed duplicate `team.create/list/delegate`; `hive.*` tools now
  take `{hive, task}` args; `bot.*` tools exist for profiles
- Job mode: `worker.rs` still accepts `"team"` as alias → Workflow (compat)

## Kept intentionally

- `AgentId`/`agent-*`/`agents/` entity dir/`spawn_agent`/sub-agent delegation
- `EntityKind::Agent` vs `EntityKind::Bot` (distinct entity kinds)
- `JobMode`/ticket/daemon "agent" job concepts; `--agent`/`--bot` cron flags
- `agent.*` tools = entity (sub-agent) tools — correct per model
- Microsoft Teams gateway, A2A `agent.json`, `team.delegate` payload alias

## Verified

`cargo check`/`fmt`/`clippy` clean; `cargo test --workspace` all green;
real `vikram.db` migrated (`bots`, `hive_rosters`, `hive_roster_members`
tables present); `vikram bots` + `vikram tools list` show bot.*/hive.* only.
