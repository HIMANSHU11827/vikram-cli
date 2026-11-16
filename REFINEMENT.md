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

## 2026-11-16 — batches 11-17 (commits 1b6dd60…38573c9)

- Pane focus: clicking bots pane/transcript focuses it — p/s/r and transcript pane keys reachable (verified live: click BOTS title, `p` paused a bot).
- Click targets added: suggestion rows accept, autocomplete candidates accept, history/reverse-search match rows accept, approval request rows select.
- /tasks stop cancels the task's session turn, not just the label; bogus id warns.
- Picker `with_note` legends stripped (dead text — notes only render on empty lists); Background error note became a footnote row instead of vanishing.
- Title migration marks version before classifying — operator renames can't be re-judged.
- CLI: entities scaffold-before-save + rm cleans dir; profiles new verifies user; hive/tickets send verify parent; project disconnect verifies session.
- Docs: tui.md mouse contract updated for all new hit regions.

## Open / next

- In flight: daemon transaction atomicity agent; computer.rs/pty.rs/terminal-tool audit agent.
- DeliveryLedger in vikram-gateway is dead code — outbound sends have no retry; wire it or remove it (architectural decision).
- Context estimator anchor not cleared after in-place transcript pruning (context.rs).
- Shell timeout kills only the direct child, not the process group (grandchildren survive).
- `cron add` create+update is two writes (main.rs ~2834).
- Store: dozens of UPDATE/DELETE helpers return Ok on zero rows — systemic; CLI checks are the stopgap.

- Picker `with_note` legends on non-empty lists are dead text (footer prints the same verbs) — decide per call site: strip legends, keep empty-state reasons.
- Dead `Focus::Conversation`/`Focus::Bots` — handlers unreachable; either wire a focus key or remove handlers + fix docs/tui.md:160-161 (bots pane keys are documented but unreachable).
- Bots pane chips tag/soul/new-chat/mode have no keyboard path (mouse-only).
- /tasks "stop" only flips a label — TaskState::Cancelled is never consumed; needs real cancel plumbing or honest relabel.
- /permissions Enter opens Mode picker for ANY row incl. hint/rules — decide intended behavior.
- Overlay::Autocomplete candidates, history-search match rows, suggestion popup rows, approval request rows draw as selectable but have no click hits.
- CLI: sessions/users/profiles/entities/hive/tickets rm print success on bogus ids (agent covering), chat blocks on stdin with no message + TTY, `entities new` saves row before scaffolding, `entities rm` orphans scaffold dir, `profiles new` accepts nonexistent user, `hive send`/`tickets send` don't verify parent exists, `project disconnect` same, `cron edit` two-write non-atomic, gateway DeliveryLedger dead code (sends have no retry), daemon settle/claim non-atomic writes, session_fts not copied on fork (agent covering), title migration can reclassify manual titles, steps/context-anchor minor counter drift.
- vikram-store: dozens of UPDATE/DELETE helpers return Ok on zero rows (systemic; CLI-layer checks are the stopgap).
- Unverified live: two pending approvals reachable end-to-end (unit-tested only).

## Status update (end of pass 2)

Items above marked "agent covering"/dead handlers/cosmetic stop/picker notes/click hits are
now DONE (see batches 11-17). Still genuinely open: bots pane chip keyboard path, live
verification of two pending approvals, `vikram chat` stdin-blocking with no message, DeliveryLedger
wiring decision, context estimator anchor drift, shell process-group kill, `cron add` two-write,
systemic zero-row Ok returns in vikram-store.

## 2026-11-16 — batches 18-21 (commits 564f7d4…4bad80b, subagent-driven)

- daemon: claim/settle/settle_cancelled/skip_run/adopt_orphans atomic; Once-job disable on refusal; enable persists next_run; Delivery::Rerouted for fallback posts; heartbeat consecutive streaks + stale stamp cleanup; store errors propagate to tick log.
- runtime: PTY sandbox binds workspace_root not model cwd (was full-host RW escape); duplicated inner_shell removed; wrapped PTY gets cwd; verify/test_run/build_check honour sandbox; shared wait_with_timeout kills process group + deadline drain (no more hangs or orphaned grandchildren); spawn failure ≠ timeout; verify max_attempts honest 1; TestRun pnpm; dead git commit arm removed.
- computer/process: ydotool hex click codes (Wayland clicks were silent no-ops); screenshot/ocr path confinement via canonical_resource; scroll "at the pointer"; process kill records pid, rejects pid<=0.
- tui: wheel scrolls only under cursor & swallowed by modals; arrow keys scroll at selection boundary; PgUp/PgDn = viewport page; selection maps columns→chars (wide glyphs) and can't start on chrome.

## Still open

- DeliveryLedger in vikram-gateway dead code — wire or remove (architectural).
- Context estimator anchor not cleared on in-place prune path (runtime.rs ~1256 replace_messages without anchor=None).
- Bots pane chips keyboard path.
- Store: zero-row Ok returns systemic (CLI checks are stopgap).
- coalesce_overdue/stale_slot still two-write (daemon agent skipped, not in verified list).
- Legacy heartbeat.reported keys orphaned (bounded).
- computer.screenshot ActionKind Execute→Write judgment call — revisit if policy surprises.

## 2026-11-16 — batches 22-30 (commits 2601873…d466170)

- store/daemon leftovers: skip_slot atomic; entities/profiles/hive/tickets/project existence checks; chat TTY stdin refusal; title migration ordering.
- plugins: path-traversal closed at plugin_dir + under_root; execute resolves by manifest id, drains pipes concurrently (no deadlock); :scope documented advisory.
- tickets can_read enforced; memory_files refuses to overwrite unreadable files; snapshot stays frozen.
- consolidation: superseded/expired can't resurrect; winner tuple matches docs; unknown status fails load. dialectic: original case, ',' clause end, cap+stale-key cleanup.
- evolution: autonomy tiers enforced at execution boundary; self_dev/self_config stop fabricating; failed reflection jobs don't retry forever; sanitize covers metadata/tools_used; category stats bounds; dashboard real count; lib.rs docs honest.
- workspace: ensure_session_workspace keeps project mode.
- lsp: publishDiagnostics distinguishes "no report" from "clean".
- skills.rs editor temp file: create_new + 0600.
- policy (fa90b29): command_segments splits on > < ` $( (( { }; deny-first across tiers AND tool overrides; empty/filesystem-root workspace fails closed; within_root normalizes ..
- gateway audit CRITICAL in flight: authorize_source never called — agent wiring it into inbound path + A2A auth + session keys + ingress truncation + media_fetch redirects.

## Still open

- self_awareness/conversational/self_recovery are unwired scaffolding with fabricated content — decide wire-or-stub-mark (LOW operational, HIGH dishonesty if surfaced).
- access.rs allowlist_suppresses_pairing dead field; buzz allowlist asymmetry (LOW).
- DingTalk webhook map unbounded; A2A task map stale (LOW).
- plugins.rs run no confirm — judged acceptable (explicit operator command).
- redact.rs "sk-" over-matches task-/risk- (fail-safe, LOW).
- optimization.rs pareto fallback ignores `passed`; propose_candidate unscrubbed fields (LOW, unwired).
- store.rs zero-row Ok systemic remains the big one.

## 2026-11-16 — batch 31 (commits fa90b29…HEAD)

- CRITICAL gateway: authorize_source wired into inbound path (was dead code — any sender could drive the agent); default-deny, escape hatches documented. BREAKING: no-allowlist deployments must set GATEWAY_ALLOW_ALL_USERS=true.
- /a2a requires A2A_TOKEN bearer; session keys (platform,chat_id); ingress rejects truncated bodies + conflicting Content-Length; media_fetch no redirects (SSRF closed); registry connect fails on missing deps; turn() never sends partial reply; mcp serve evaluates PermissionRequest through policy (Deny + floor apply).
- policy: deny-first real; metachar allow bypass closed; empty-root floor fails closed; within_root .. normalized.
- Remaining LOW: DingTalk/A2A-task map bounds, access.rs dead field, unwired evolution scaffolding honesty (self_awareness/conversational/self_recovery fabricated content — decide wire-or-mark), store zero-row Ok systemic.

## 2026-11-16 — batch 32 (local, uncommitted)

- StoreError::NotFound added; delete_session/rename_session/set_session_title_status now error on zero affected rows instead of phantom Ok. Sweep continues — ~40 mutators remain.
- Termux: TLS fully fixed (preconfigured rustls+ring+webpki, no platform-verifier JNI); aws-lc-rs out; android cfg gaps closed (doctor/proc/service/sandbox/hints); daemon tick-rate + $PREFIX zoneinfo + Windows-safe stop; kill_tree via libc; docs/install.md for 4 OSes. Committed+pushed through c80b326.

## 2026-11-16 — batch 33 (local, uncommitted)

- StoreError::NotFound sweep complete: ~30 keyed mutators now error on zero rows — all session setters, project/bot/user/profile/entity/hive/roster/ticket/assignment updates+deletes.
- CLI guards: hive add-participant checks get_hive; tickets add-participant + assign check get_ticket (no more orphan rows); learning approve propagates skill-create errors and rejects non-pending proposals; select_project verifies session before creating project row.
- Manager-level guards: TicketManager::add_participant/create_assignment and HiveManager::add_participant verify parent exists; set_session_workspace refuses orphan upserts.
- HiveManager::append_message verifies hive exists; learning reject also requires pending status; workspace changes + files rm error on bogus ids.
- tickets.rs create_assignment propagates set_ticket_status errors instead of swallowing.
- daemon: heartbeat delivery records real outcome; prune_runs wired into settle (keep 200/job); claim_begin_run refuses overlap (one run per job); reflection jobs retry to 3 attempts before failing.
- daemon: skip_run sets last_status='skipped' (both skip paths consistent); self_evaluation refuses all-zero benchmarks.
- gateway: transport errors sanitized (no token/URL leak); Slack timestamp freshness; IRC CRLF injection blocked; A2A task store capped 1024; Discord chat_id encoded.
- mcp visibility now enforced (Private blocks tools at registration + run); MCP server no-executor returns error not stub success.
- skills: corrupt disabled.json/trust.json/visibility warn instead of silent reset; install_git subdir symlink escape blocked; frontmatter depth cap 32.
- policy: tool_permissions matches tool family (terminal/process/task verbs); rules.json drops over-broad hand-edited allows.
- codeindex: refs rebuild skipped when nothing changed.
- mcp client: tools/resources/prompts list errors now fail the connection instead of reporting 0 items.
- gateway: whatsapp/weixin/wecom per-chat maps capped at 4096.
- shell.run: destructive-command heuristic marks SideEffect::Destructive (Smart no longer auto-allows rm -rf class commands).
- slack streams map capped 1024; vikram-terminals dep dropped from runtime (dead); terminals/acp marked EXPERIMENTAL.
- policy: verb_prefix now segment-boundary matched (fs.read no longer covers fs.readx); slack streams capped; dead crates marked EXPERIMENTAL + dep dropped.
- sandbox: Landlock no longer selected (no enforcing wrapper — was silent pass-through claiming enforcement); bwrap gets --clearenv + env allowlist (provider keys no longer leak into sandboxed processes).
- dingtalk_stream: sessionWebhook validated as dingtalk.com https before POST (forged-frame SSRF closed).

## 2026-11-16 — batch 34 (local, uncommitted)

- sandbox: Landlock silent pass-through fixed; bwrap --clearenv + env allowlist.
- gateway: dingtalk sessionWebhook host-validated; mattermost/ntfy URL segments encoded.
- store cascade audit (agent): delete_user bypasses entity cleanup, delete_hive misses tickets.hive_id, delete_ticket misses parent_ticket, delete_assignment no reparent, delete_session misses parent_session/forked_from, zero-row removes still Ok — ALL LOGGED, fixes pending.
- daemon audit (agent): cron next_after can return instant <= after on DST fall-back (skip-loop ~1h/yr); job.skipped sticky warning; cancel.*/member.* state keys leak; append-only logs unbounded; DST double-fire hypothesis DISPROVEN.
- gateway round-2 (agent): authorize_source bot-mentions arm doesn't verify mention; ntfy constant user_id; dingtalk endpoint trusted; bluebubbles password-in-URL; qqbot no timestamp freshness.
- store: delete_hive NULLs tickets.hive_id; delete_ticket clears child parent_ticket + cross-ticket task_ref; delete_session clears parent_session/forked_from.
- store: delete_assignment reparents children + detaches task_ref.
- cli: skills enable/disable verify catalog membership first; skill_rename rolls back copy on delete failure.
- audits queued: compaction off-by-one+lossy refold, memory scope isolation (cross-user leak), learning scope mismatch, reflection non-idempotent+bypasses policy, turn H1/H2, session picker rename targets wrong session.
- compaction: replay off-by-one on re-folds fixed; operator bare ToolResult clears unanswered.
- memory tools: scope confined to caller (writes own scope only, reads own+global) — cross-user leak closed.
- evolution: run_reflection auto_apply_memory flag; learning passes policy gate (Disabled no longer bypassed).
- daemon: cron next_after requires instant > after (DST fall-back skip-loop fixed).
- tui: suggestions yield to overlays+modified chords; select() clears suggestions/selection; ApprovalRequested no longer clobbers dirty overlay; upsert_session doesn't steal active tab.
- providers: UTF-8 split-chunk fix (both); anthropic invalid tool-arg JSON refused like openai.
- session: move_queue can reach back position.
- store: delete_user now detaches entity refs (hive_participants/ticket_participants/assignments/active_identity), entities.profile_id for shared entities, ticket_participants.user_id, and tickets.hive_id in both hive cascades.
- daemon: broadcast Lagged no longer fatal in runner turn/wait_for; worker panic now reports Finished + frees slot.
- store: delete_profile hive loop + all sid-loops get tickets.hive_id/lineage detaches (drift vs delete_user fixed).
- learning: memory writes land in session scope (recallable + isolated); reflection proposal/memory keys deterministic per experience (retry-idempotent).
- tui: Overlay::RenameSession carries target SessionId (picker rename can't hit wrong session); anthropic duplicate Finished removed.
