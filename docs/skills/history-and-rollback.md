# History and rollback

Every mutating operation on a skill directory (create, edit, delete, install, import, rollback) is
recorded in a per-skill history log under the state directory. History is the audit trail and the
rollback mechanism.

## Where history lives

- `<state_dir>/history/<skill-name>/` — one directory per skill.
- `History::load(name)` / `History::load_in(state_dir, name)` read the log.
- `History::entries()` returns the recorded manifests in order.
- `History::latest()` / `History::previous()` return the most recent and the one before it.

## Manifest

`Manifest` records: skill name, content hash (`hash_dir`), timestamp (`at_ms`), and the operation
that produced it. `History::record(Manifest)` appends. `record` **merges** into the on-disk log, so
a reload is required to see your own write (deviation D3).

## Hashing

`hash_dir(&Path) -> Result<String>` computes a FNV-1a hash over the directory contents. It is
deterministic: the same bytes produce the same hash regardless of file order. Only file contents
are hashed; metadata (mtime, mode) is not.

## Rollback

- `History::detect_local_changes(dir)` compares the current directory hash to `latest()`.
- `History::snapshot(dir, at_ms)` records a new manifest for the current state.
- `History::rollback(dir)` restores the directory to `previous()`'s hash, if it exists.

CLI: `vikram skills rollback <name> [--yes]` restores the previous version and prints the new hash.

## CLI usage

- `vikram skills history <name>` — print the manifest list.
- `vikram skills rollback <name> [--yes]` — restore previous version.

## Non-negotiables

- Rollback never executes skill content. It only restores files.
- A rollback that finds no previous version is an error, not a no-op.
- Hashing covers canonical content, not `Frontmatter.raw` (see R1 in the contract).
