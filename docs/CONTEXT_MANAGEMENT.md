# Context Management

How VIKRAM assembles the prompt it actually sends, keeps it inside the window, and makes the
process visible on the existing interface. This is the umbrella; the deep dives are
[TOKEN_MANAGEMENT.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/TOKEN_MANAGEMENT.md),
[CONTEXT_COMPACTION.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/CONTEXT_COMPACTION.md),
[MEMORY_INTEGRATION.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/MEMORY_INTEGRATION.md),
[CHECKPOINT_AND_RECOVERY.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/CHECKPOINT_AND_RECOVERY.md),
and the narrower instruction-file loader in [context.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/context.md).

## Per-request assembly

A turn builds its prompt in a fixed order
([`turn.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/turn.rs)), each part
metered against the [`ContextBudget`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/context_engine.rs):

1. The system line said to the model every turn, in its own message.
2. Project and entity instructions — `AGENTS.md`, `SOUL.md`, and the rest — loaded by the
   `ContextLoader`, capped by `context.project_budget_tokens`; the loader stops once the bundle
   reaches the cap rather than truncating mid-file.
3. Retrieved memory — relevant durable notes, capped by `context.memory_budget_tokens`, and scoped
   so one conversation cannot surface another's (see MEMORY_INTEGRATION).
4. The working transcript — the folded view produced by compaction, kept within the usable window.

The tool schemas are counted as overhead so the budget reflects their real cost, and the request is
only sent once it fits. A `ContextMetered` event publishes what went in and whether the "used"
figure is measured or estimated.

## What the transcript never loses

- **Tool results that are too big.** An oversized result is digested in place (head + tail + a
  pointer) and the full text is spooled to a per-session store. The digest names the handle; the
  model reads any slice back with the `output.read` tool, which can only address the spool of the
  session making the call. See [`spill.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/spill.rs).
- **The journal.** Every message is appended durably; compaction folds a *view*, never deletes the
  record, so re-reads and restart recovery always have the originals.
- **Code changes.** Files the agent writes, edits, or deletes are checkpointed so the pre-change
  bytes survive. See CHECKPOINT_AND_RECOVERY.

## Isolation

Context is confined to the identity that owns it. Each session has its own spool directory, so
`output.read` cannot reach another session's captured output. Delegation passes a *bounded brief*
down and a *bounded answer* back (`context.delegation_budget_tokens`) — a child never inherits the
parent's whole transcript, and a chain deeper than the policy's max delegation depth is refused at
the live gate. Hive participants are given only the most recent shared exchange that fits
`context.hive_budget_tokens`, not the entire hive log.

## The knobs

All of the above are configured under `context.*` and are settable with `vikram config set`
([`main.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-cli/src/main.rs)):
`auto_compact`, `trigger_percent`, `keep_recent`, `auto_compact_limit`, `safety_margin_percent`,
`tool_output_ceiling`, `memory_retrieval`, `memory_budget_tokens`, `project_budget_tokens`,
`delegation_budget_tokens`, `hive_budget_tokens`.

## The inspector

Token and context state is surfaced on the existing TUI status surface — the used/usable figure,
with an estimated reading marked `~` and an assumed model window marked `*` so a guess is never read
as a guarantee — rather than a separate dashboard, keeping it compact. See
[tui.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/tui.md).
