# Memory Integration

Relevant durable memory is pulled into the prompt each turn — within a token budget, ranked for the
current question, and confined to what this conversation is allowed to see. The retrieval path is
[`memory.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/memory.rs) and
[`runtime.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/runtime.rs); the
broader memory subsystem (learning, evolution) is described in
[memory.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/memory.md).

## Retrieval into the context builder

`retrieved_memory(session)` runs once per turn, before the transcript is assembled, and only when
`context.memory_retrieval` is on:

1. Take the most recent user message as the query.
2. Derive overlap terms from it (`retrieval_terms`).
3. Rank stored entries against those terms.
4. Lay the results into a block under `context.memory_budget_tokens`, stopping at entries that would
   not fit. The block is prefixed with an explicit caveat that these are durable notes that may be
   stale and should be verified — retrieved memory informs the model, it does not silently replace
   the live conversation.

## Ranking

`recall_terms(terms, scopes, limit)` scores each candidate on how many terms it hits, with recency
as a tie-breaker inside a rough day band — so a current note that matches many words outranks an old
note that matches one. Matching is done with parameterized `LIKE` clauses; the stored `search` only
answers a whole-phrase question, whereas recall answers "what does this turn overlap with."

## Isolation — the security rule

The runtime `MemoryStore` is a single shared table; every conversation writes into it, separated only
by a `scope` column. **`scope` is free-form text chosen by the model** (it defaults to `global`), so
it is not itself a trusted identity binding.

Because of that, recall is deny-first: `retrieved_memory` passes only the scopes the *host* controls
— `global` plus the current session's own id — and `recall_terms` confines results to exactly the
scopes named, returning nothing when none are given. The model can influence which *terms* are
matched; it cannot widen the *scopes* recalled. This closes the cross-conversation leak where an
unscoped sweep would surface another session's notes into this one's context.

A deliberate consequence: an entity- or profile-wide shared memory that should be recalled across
sessions is not, until those scopes are assigned by the host rather than typed by the model. The
user/profile/entity isolation that the higher-level store enforces through its owned tables is a
separate, stronger guarantee — see [store.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/store.md)
and [security.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/security.md).

## Where it is metered

The memory block is part of the request's overhead accounting; it competes for the same usable
window as the transcript and instructions and is capped before the request is sent
([CONTEXT_MANAGEMENT.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/CONTEXT_MANAGEMENT.md)).
