# Context Loader

Centralized `ContextLoader` resolves and loads only relevant, authorized Markdown files
per request.

## Features

- Caching with mtime-based invalidation.
- Token budgeting.
- Duplicate prevention.
- Permission-aware (only authorized files).
- Scaffold entity context directories.

## Files

- `crates/vikram-runtime/src/context.rs`

## Notes

Budget defaults to 128k tokens. `with_budget` and `invalidate_all` exist but have few
external callers.