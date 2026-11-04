# Checkpoint and Recovery

Long-running agent work has to survive a crash, a restart, or a wrong turn, without silently
discarding a user instruction, a code change, or the task state. Three layers make that true.

## 1. The transcript — journal replay

Every message, tool call, and result is appended to the per-session journal
([`vikram-store`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-store/src/lib.rs)) before
any model work depends on it. Compaction folds a *view*, never the record, so the originals are
always durable. On boot the runtime hydrates each session by replaying its journal through
`project_from_journal`, rebuilding exactly the folded transcript the model last saw
([`runtime.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/runtime.rs)).
A restart does not lose the conversation, and it does not re-inflate it past the window either.

## 2. File edits — checkpoints

A workspace mutation is checkpointed so the agent's edit is never the only copy. The mechanism is
[`WorkspaceManager`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/workspace.rs):

- `record_file_change(session, agent, path, operation, before)` writes a `file_changes` row and, when
  there is prior content within `CHECKPOINT_MAX_BYTES`, spools the original bytes to
  `home/sessions/<session>/checkpoints/`.
- `restore_file(session, path)` brings back the most recent checkpointed content for that path, or
  errors if there was never one.

This is now wired into the live edit path. The `fs.write`, `fs.edit`, and `fs.delete` tools
([`tools/fs.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/tools/fs.rs))
capture the pre-change bytes and record the change after a successful mutation, tagging it `create`,
`modify`, or `delete`. A create has nothing earlier to recover (no checkpoint copy), but is still
recorded for audit; an overwrite or a delete keeps the bytes it replaced, so recovery returns the
pre-edit content rather than the discarded work. Recording is best-effort and scoped to the session —
outside a session (a direct CLI tool call) there is no conversation to recover into, so it is a
no-op — and it never fails the tool that already succeeded.

## 3. Cost and state — durable accounting

Each turn's token cost lands in the `usage` table (see
[TOKEN_MANAGEMENT.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/TOKEN_MANAGEMENT.md)), and task
state that outlives a single conversation lives in the tickets/assignments tables. A restart resumes
with the ledger and the task graph intact, not a blank slate.

## Guarantees

- Recovery is scoped per session: one conversation restores only its own checkpoints and reads only
  its own spooled output.
- Nothing on the recovery path overwrites current work implicitly — `restore_file` is an explicit
  operation, and a missing checkpoint is reported rather than guessed at.
- Deletion cascades: removing a session removes its journal, checkpoints' records, spool, and usage
  rows together, so no orphaned context lingers where another identity could reach it.
