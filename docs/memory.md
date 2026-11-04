# Memory

Isolated per user/profile/entity in SQLite. Controlled creation, retrieval, update and
deletion with ownership. Scopes: `user`, `memory`, `agent`, `profile`, `hive`, etc.

## API (MemoryStore)

- `save(key, value, scope)`, `search(query, scope)`, `list(scope)`, `list_all()`,
  `delete(key, scope)`.

## Retrieval into context

The runtime `MemoryStore` is one shared table separated only by a free-form `scope` string the model
chooses (default `global`), so scope alone is not a trusted identity binding. Per-turn recall is
therefore deny-first: `retrieved_memory` confines `recall_terms` to the host-controlled scopes
(`global` plus the current session id) and returns nothing when none are granted. See
[MEMORY_INTEGRATION.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/MEMORY_INTEGRATION.md) for the
ranking, budget, and isolation detail. The user/profile/entity guarantee in the tables above is the
stronger, store-enforced isolation (see [store.md](file:///home/himanshu/Desktop/VIKRAM-CLI/docs/store.md)).

## Learning

`LearningManager` proposes memory/skill/policy changes from a conversation;
approval-gated (auto / approval-required / suggest-only / disabled). Dedup via
`dedup_memory`. Audit log recorded.

## Evolution

`EvolutionEngine` (SQLite at `~/.vikram/evolution.db`) records experiences, reflects
into proposals, auto-applies low-risk memory additions, skill evolution, optimization
candidates. Hive learning records collaboration lessons.

## Files

- `crates/vikram-runtime/src/memory.rs`
- `crates/vikram-runtime/src/learning.rs`
- `crates/vikram-evolution/src/*`

## Tests

Evolution lifecycle tests (12 scenarios) in `crates/vikram-evolution/tests/lifecycle.rs`.