# Memory

Isolated per user/profile/entity in SQLite. Controlled creation, retrieval, update and
deletion with ownership. Scopes: `user`, `memory`, `agent`, `profile`, `hive`, etc.

## API (MemoryStore)

- `save(key, value, scope)`, `search(query, scope)`, `list(scope)`, `list_all()`,
  `delete(key, scope)`.

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