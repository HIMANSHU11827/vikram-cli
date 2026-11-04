# Context Compaction

Compaction keeps a long conversation inside the model window without losing the thread. It is a
*projection over the journal*, not a destructive edit: the append-only journal always holds every
message, and the working transcript is a folded view. Code:
[`compaction.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/compaction.rs).

## When it fires

`should_compact` folds when the transcript exceeds the trigger, computed from the
[`ContextBudget`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/context_engine.rs)
usable window. The trigger is `context.trigger_percent` of usable input, unless
`context.auto_compact_limit` is set (a fixed-cost ceiling that wins over the percentage). A few
guards:

- A transcript shorter than `keep_recent + min_fold` is left alone — folding would not shrink it.
- **Zero usable budget is a trigger, not silence.** When a prompt and tool set already fill the
  window the transcript has no room at all; folding is the only move that can still shrink the
  request, so it runs rather than staying quiet.

`auto_compact` gates the automatic path; a manual `/compact` always works.

## Folding safely

`compact(messages, policy, estimator)` folds the older span into one summary message and keeps the
`keep_recent` tail verbatim. The fold boundary is chosen by `drop_oldest_consistent` so it never
separates a `Block::ToolCall` from its `Block::ToolResult`; `pairing_is_consistent` asserts the
result stays coherent. The summary carries `COMPACTION_SUMMARY_KEY`, `COMPACTION_THROUGH_KEY`
(highest folded seq) and `COMPACTION_FOLDED_KEY`, and is recognized by `is_compaction_summary`.

The digest is structured, not a truncated blob: it buckets the folded span into Live objective,
Decisions, Work completed, Relevant files, Errors and blocked, Next steps, plus a tool-call tally.
When it must shrink, `trim_least_important` drops the lowest-priority buckets first — so a decision
or an open blocker survives longer than incidental file lists. Nothing that would silently discard
a user instruction, a code change, or the task state: those buckets are kept and the originals stay
durable in the journal.

`prune_transcript` and `active_context_tokens` support the lighter, non-summary trimming and the
"what does the live view actually cost" measurement used for metering.

## Rebuilding after a restart

`project_from_journal(records)` replays the durable journal back into the same folded view,
honoring the compaction markers so a reopened session shows what the model last saw — not a raw
re-read of every line. The runtime hydrates sessions through this on boot
([`runtime.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/runtime.rs)).

## Boundaries that cross compaction

`digest_tool_output(content, ceiling)` is how an oversized tool result is folded in place, and
`delegation_brief` / `delegation_answer` reuse the same structured-digest machinery to bound what
crosses a delegation boundary. See
[CONTEXT_MANAGEMENT.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/CONTEXT_MANAGEMENT.md).
