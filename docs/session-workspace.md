# Session, Workspace, Project & File Management

This document describes what was actually implemented for VIKRAM's session and workspace
management system, how it maps onto the existing architecture, and the deliberate resolutions
where the requested design conflicted with VIKRAM's security model.

## Architecture

VIKRAM already had one authoritative store (`vikram-store`, SQLite, WAL, append-only journal for
conversations) and one runtime (`vikram-runtime`). This feature **extends both**; it introduces no
second session manager, no second database, and no JSONL conversation files.

- `vikram-store` — new tables + migrations + CRUD (metadata only; files stay on disk).
- `vikram-runtime::workspace` — `WorkspaceManager`: the filesystem side (lazy session workspaces,
  boundary enforcement, artifact indexing, checkpoints/restore).
- `vikram-policy` — `MandatoryFloor::extra_write_roots` so session workspaces are writable without
  weakening the deny-first floor.
- `vikram-cli` — `session`, `project`, `workspace`, `files` operator commands over the same store.

## Storage layout

```
~/.vikram/
├── vikram.db                 # sessions, projects, session_workspaces, artifacts, file_changes
├── sessions/<session_id>/
│   ├── workspace/            # lazily created; session-owned files live here
│   └── checkpoints/<ts>-<name>   # pre-change copies for restore
```

The database remains authoritative for metadata and conversation history (the journal). Files are
never stored in SQLite; artifacts index the original path as the source of truth.

## Database schema (additions)

New columns on `sessions` (via `ensure_column` migration, old rows preserved):

- `status TEXT NOT NULL DEFAULT 'active'` — `active` | `archived`
- `project_id TEXT` — FK to `projects.id`

New tables:

- `projects(id, name, path UNIQUE, status, created_ms, updated_ms)` — `status` is `active` or
  `missing` (refreshed from disk, never deleted automatically).
- `session_workspaces(session PK, mode, workspace_path, project_id, created_ms, updated_ms)` —
  `mode` is `session` or `project`.
- `artifacts(id, session, project_id, agent_id, name, path UNIQUE, kind, mime, size, checksum,
  source, created_ms, updated_ms)`.
- `file_changes(id, session, agent_id, path, operation, checksum_before, checksum_after, size,
  checkpoint_path, ts_ms)`.

Indexes: `artifacts(session)`, `artifacts(path)` (unique), `file_changes(session)`,
`file_changes(path)`, `projects(path)` (unique).

## Session lifecycle

Implemented: create, rename, archive, duplicate (metadata + journal copy is **not** performed —
duplicate copies metadata only and is labelled as such), delete (cascades journal, workspaces,
artifacts, file_changes — never touches project files), search (title/cwd LIKE; conversation search
scans the journal payloads, no FTS5 duplication), list active vs. list-all, show.

Session IDs come from the existing `SessionId` newtype (`ses-…`). Titles default to `Session N`
and can be renamed; auto-titling from the first user message is **not** implemented (see
limitations).

## Default workspace behaviour (deliberate resolution)

The requested default was `cwd = sessions/<id>/workspace`. VIKRAM's mandatory policy floor vetoes
any write outside the operator's `workspace_root`, and changing the default cwd to the home
directory would break every existing workflow that edits the current project. Resolution:

- A session's cwd stays `workspace_root` (unchanged, backwards compatible).
- Each session additionally owns `~/.vikram/sessions/<id>/workspace`, created **lazily** — a
  text-only conversation creates nothing.
- The floor gained `extra_write_roots` (host-declared, fixed at boot to the sessions root), so the
  agent *can* write into its session workspace while writes anywhere else outside
  `workspace_root` remain vetoed. The floor stays deny-first; this widens nothing the operator
  does not already own.
- `WorkspaceManager::resolve()` maps session-relative generated-file paths into the session
  workspace (or the project root when a project is selected).

## Projects

`select_project` registers a directory (internal or external — external paths are never copied or
moved), associates it with the session, and records mode `project`. `disconnect_project` returns
the session to its own workspace. Projects are remembered across restarts and never silently
replaced: selecting a new project is an explicit operator action. `refresh_project_status` marks
directories that disappeared as `missing` (metadata kept for reconnection). Multiple sessions can
share one project.

## Artifacts

`register_artifact` indexes a file (size, FNV-1a checksum, kind from extension, guessed MIME,
source message/tool call, session/project/agent attribution) without copying it. Removing an
artifact record never deletes the file. Listing supports per-session and across-all-sessions views.

## Checkpoints and recovery

`record_file_change` stores the pre-change bytes (up to 5 MB; larger files are indexed but not
checkpointed) into `sessions/<id>/checkpoints/` and logs before/after checksums. `restore_file`
copies the latest checkpoint back. Limitations, stated honestly:

- Checkpoints cover files the manager was asked to track; they are not a shadow-git of the whole
  project.
- External changes made outside VIKRAM between checkpoints are not merged.
- Binary files are supported (byte copies).

## Security boundaries

- `is_within`/`verify_within` canonicalize paths (resolving symlinks) and refuse anything outside
  the session's workspace or its project — a symlink pointing outside is not an escape hatch.
- Path traversal (`../`) is neutralized by canonicalization before comparison.
- Deleting a session removes metadata and its home-directory session folder; project files are
  never deleted.
- The only new writable root is VIKRAM's own sessions directory, fixed at boot in
  `RuntimeInner::policy()`, never from rules or model content.

## CLI

```
vikram session list|new|show|rename|archive|duplicate|rm
vikram project list|select <session> <path>|show <session>|disconnect <session>
vikram workspace show <session>|changes <session>|restore <session> <path>
vikram files list [session]|rm <artifact-id>
```

## Tests

- `crates/vikram-store/tests/sessions.rs` — lifecycle, search, duplicate, project CRUD and
  association, workspace rows, artifact upsert/remove, file-change scoping, delete cascade,
  id-type separation (7 tests).
- `crates/vikram-runtime/src/workspace.rs` tests — lazy workspace creation, project
  select/disconnect, boundary refusal including symlink escape, artifact registration/removal with
  file survival, checkpoint restore, restore-without-checkpoint error (8 tests).

All pass alongside the existing suite (`cargo test -p vikram-store -p vikram-runtime
-p vikram-policy`).

## Migration

Additive only. `migrate()` creates the new tables with `CREATE TABLE IF NOT EXISTS` and adds the
new `sessions` columns through `ensure_column`, so an existing `vikram.db` upgrades in place with
no data loss and no rewrite of journal history.

## Not implemented (honest list)

- Automatic session titling from the first user message.
- TUI panels for files/projects (the runtime handle `workspace()` and store methods are exposed;
  the TUI wiring is the remaining step).
- FTS5 conversation search (journal LIKE scan is used instead; no duplicate authoritative copy).
- Worktree-based isolation (existing delegation/git tooling covers the use case; no worktree is
  created per session, as required).
- Managed copies of generated media (original path stays the source of truth).
- GUI integration (VIKRAM has no GUI in this checkout).

## Reused, not duplicated

`Store`/`SqliteJournal`, `SessionId`/`ProjectId` newtypes, `SessionRuntime`/`SessionSnapshot`
(extended with `project_id`/`status`), the policy floor, the CLI command pattern, and the
`~/.vikram` home convention.