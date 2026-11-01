# Runtime integration

How `vikram-runtime` uses `vikram-skills`. Nothing here executes skill content; the runtime only reads, injects, and
describes.

## The adapter (`crates/vikram-runtime/src/skills.rs`)

`SkillRegistry` wraps `Catalog` + `Loader`:

- `SkillRegistry::for_workspace(&Path)` builds a catalog from user + project roots for that workspace.
- `list()` / `skills()` return `Vec<Skill>` with `instructions` empty (metadata only).
- `get(name)` / `find(name)` resolve the precedence winner; `load(name)` / `load_skill(name)` read the body on
  demand via `Loader::load`.
- `search(query)` returns metadata-only matches.
- `read_resource(name, rel)` loads then reads the resource.
- `scan`, `scan_dir`, `default_dir`, `from_default_dir`, `merge`, `count`, `get_skill` retained for backward
  compatibility with the old naive registry's callers.

`Skill` struct fields are unchanged (`name`, `description`, `instructions`, `path`, `loaded`), so
`lib.rs:36` (`pub use skills::{Skill, SkillRegistry}`) and `runtime.rs:29` keep compiling.

## Skill tools (`crates/vikram-runtime/src/tools.rs`)

Four tools, all `ActionKind::Read`, `Trust::Model`, `SideEffect::None`, registered in `with_builtins`:

| Tool | Args | What it does |
| - | - | - |
| `skill.list` | (none) | list metadata for all skills |
| `skill.load` | `name` | load and return the body (clipped at `MAX_TOOL_OUTPUT`) |
| `skill.search` | `query` | metadata-only search |
| `skill.read` | `name`, `path` | read one resource file (path-escape checked) |

All resolve `$VIKRAM_HOME` via `SkillRegistry::for_workspace(&ctx.workspace_root)` because `ToolContext` carries
only `workspace_root`. There are no install/edit/delete tools: `MandatoryFloor` blocks writes outside the workspace
root, so management is host-side (runtime method or CLI).

## Turn injection (`crates/vikram-runtime/src/turn.rs`)

Before each `CompletionRequest`:

1. Build `SkillRegistry::for_workspace(&inner.workspace_root)`.
2. Call `catalog_listing()` (budgeted metadata only).
3. If non-empty, prepend a `Message::text(Role::System, catalog, Trust::External)` to the request messages, then
   extend with `session.messages()`.

Skill bodies are never loaded here. This is the only place skills enter the prompt.

## Events, metrics, snapshot (deferred)

`RuntimeEvent::SkillsChanged`, skill counters in `Metrics`, and a `skills` field on `StateSnapshot` are not yet
implemented. They require exhaustive-match updates in `crates/vikram-tui/src/app.rs` (two match sites:
`App::on_event` and `handle_overlay`), which is why they are deferred until the TUI agent runs.

## What the runtime must never do

- Never execute skill content. Scripts run only through the shell tool.
- Never treat a skill's declared permissions as grants.
- Never load every skill body into context.
- Never silently overwrite an existing skill directory.
- Never auto-create Rudra/Karma/Verifier — they are bots, not sub-agents.
