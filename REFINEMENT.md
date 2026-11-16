# Refinement log

Continuous pass over VIKRAM vs the "every drawn control answers" standard.
Each entry: what was broken, what changed, how it was verified.

## 2026-11-16 — batch 1 (commit 768edab)

- Ctrl+L now really repaints (UiCommand::Redraw intercepted in event loop, terminal.clear()). Verified live in tmux: notice row shows "screen repainted; history preserved".
- Approvals panel stays open after one answer ("answered — N more waiting"); new unit test `answering_one_of_several_keeps_the_panel_open_for_the_rest`.
- Bots panel `remove` chip now routes through `open_confirm("Delete a bot")`; verified live — dialog appears, Esc leaves bot alive (Bots 2).
- `skills_lines` reads `app.workspace_root` + runtime counters; F8 panel title shows "Skills 1083 · 497 loaded".
- `PromptAction::owner_panel()` reopens the owning picker after a create; verified live: /hive → ctrl+n → name → Enter → Hives panel back with `verifyhive` row + notice.

## 2026-11-16 — batch 2 (commit 4fef530)

- F8 skills panel scrolls (PgUp/PgDn/Up/Down/Home/End + mouse wheel). Root cause: ~1090 lines in a ~30-row panel clipped footer/root/esc-chip forever. Verified live: End shows `/home/himanshu`, "F8 to close", esc chip.
- Picker row clicks carry `row_positions` (visible index); scrolled clicks select the right row. Pseudo-rows get no hit.
- All hit vectors cleared each frame in `ui::draw` — closed overlays/hidden panels leave no phantom clicks.
- Turn-header fold mark no longer double-subtracts; chevron column folds.
- Overlay char answers ignore Ctrl/Alt (Ctrl+A can no longer approve+persist).
- Paste goes to Prompt editor / picker filter; swallowed under other overlays instead of leaking into hidden draft.
- Read-only panels (status/usage/context/doctor/version): Enter dismisses silently, no more warn notice.
- Skill summary/hint/empty + tools `count` pseudo-rows can't trigger real actions (bogus `count` tool entry was persisted disabled).

## 2026-11-16 — batch 3 (commit 6b9ffa5)

- /close prints its notice only after the command succeeds (was double + premature).
- /rename <arg> prefills the rename panel (arg was dropped).
- /bot Enter opens the bot's real chat via OpenBotChat (was setting selected_bot → wrong session for never-chatted bots).

## 2026-11-16 — batch 4 (commit a5d46ba, runtime)

- permission.check evaluates the real policy via runtime.judge and reports allow/deny/approval (was hardcoded "allowed").
- ask_user QUESTION: parsing gated to the ask_user tool (other tools' output could park the turn forever).
- diagnostics.collect declares Execute/LocalMutation (cargo check runs build scripts).
- git show validates commit arg as a revision (was shell-interpolated under Read/None).
- test_run/build_check record the full command as permission resource (was first word only).

## 2026-11-16 — batch 5 (commit 00e8276, runtime)

- Turn loop judges at session.depth() via new judge_at_depth — max_delegation_depth floor veto now fires.
- steps_used counts provider rounds, not tool calls.
- session.search LIKE fallback applies agent/project/since filters and reports empty honestly.

## 2026-11-16 — batch 6 (commit 08396c0, CLI)

- vikram steer / config set print confirmations.
- bots list empty text corrected (nothing seeds bots).
- learning show/reject verify proposal exists; history prints empty-state.

## 2026-11-16 — batch 7 (commit 0d7dceb)

- Editor doc_home/doc_end; Ctrl+Home/End now reach draft ends (were line-level duplicates).
- /keys Ctrl+R description fixed; docs/tui.md approvals row matches real bindings.

## 2026-11-16 — batches 8-10 (commits 0c2e206, e36ea3e, 5f016c1)

- store: session deletes purge session_fts; fork_session copies fts/workspace/usage; recover_interrupted_session propagates errors; delete_bot clears references in one transaction.
- gateway: DeliveryLedger mark_partial/mark_failed state guards; daemon tick errors logged.
- cli: chat picks most-recent session; config set mode enforces availability refusal; mutating verbs verify ids before printing success; cron edit rejects --enable+--disable.
- runtime: hive.delegate really delegates via spawn_agent; browser tool approvals carry action details; ContextLoader truncates to budget + reports omissions.

## Open / next

- Picker `with_note` legends on non-empty lists are dead text (footer prints the same verbs) — decide per call site: strip legends, keep empty-state reasons.
- Dead `Focus::Conversation`/`Focus::Bots` — handlers unreachable; either wire a focus key or remove handlers + fix docs/tui.md:160-161 (bots pane keys are documented but unreachable).
- Bots pane chips tag/soul/new-chat/mode have no keyboard path (mouse-only).
- /tasks "stop" only flips a label — TaskState::Cancelled is never consumed; needs real cancel plumbing or honest relabel.
- /permissions Enter opens Mode picker for ANY row incl. hint/rules — decide intended behavior.
- Overlay::Autocomplete candidates, history-search match rows, suggestion popup rows, approval request rows draw as selectable but have no click hits.
- CLI: sessions/users/profiles/entities/hive/tickets rm print success on bogus ids (agent covering), chat blocks on stdin with no message + TTY, `entities new` saves row before scaffolding, `entities rm` orphans scaffold dir, `profiles new` accepts nonexistent user, `hive send`/`tickets send` don't verify parent exists, `project disconnect` same, `cron edit` two-write non-atomic, gateway DeliveryLedger dead code (sends have no retry), daemon settle/claim non-atomic writes, session_fts not copied on fork (agent covering), title migration can reclassify manual titles, steps/context-anchor minor counter drift.
- vikram-store: dozens of UPDATE/DELETE helpers return Ok on zero rows (systemic; CLI-layer checks are the stopgap).
- Unverified live: two pending approvals reachable end-to-end (unit-tested only).
