# Migration — from the naive skills.rs to vikram-skills

## What changed

The old `crates/vikram-runtime/src/skills.rs` was a 230-line read-only directory scanner:

- `Skill { name, description, instructions, path, loaded }` with `description` from the first line of `SKILL.md`.
- `SkillRegistry::scan(&Path)` — read-only, no validation, no trust, no activation.
- Skills were registered as tools (`skill.list`, `skill.load`) but never injected into the prompt.

The new `vikram-skills` crate replaces this with a full platform: discovery, validation, trust, activation,
install, bundle, history, and a native Skill tool.

## Backward compatibility

`vikram-runtime/src/skills.rs` is now a thin adapter. The public types are unchanged:

- `Skill { name, description, instructions, path, loaded }` — same fields.
- `SkillRegistry::scan`, `scan_dir`, `default_dir`, `skills_dir`, `default_skills_dir`, `from_default_dir`,
  `for_workspace`, `merge`, `count`, `get`, `get_skill`, `load_skill`, `list`, `skills`, `find`, `load`,
  `search`, `read_resource` — all retained.

Callers that used `SkillRegistry::scan(&Path)` continue to work. The registry now wraps `Catalog` + `Loader`
internally.

## What callers must update

1. **Skill text always enters as `Trust::External`.** The old code did not set trust. Any code that reads skill
   instructions must wrap them in `Trust::External`.

2. **Skill tools resolve `$VIKRAM_HOME` via `config::vikram_home()`.** The `ToolContext` has one field; skill
   tools use `SkillRegistry::for_workspace(&ctx.workspace_root)`.

3. **Catalogue injection is per-turn.** `turn.rs` builds a fresh `SkillRegistry` per turn and injects metadata
   only (budgeted). Bodies are loaded on demand via `skill.load`.

4. **No install/edit/delete tools.** `MandatoryFloor` blocks writes outside the workspace root. Management is
   host-side: CLI commands or runtime methods.

## Migration checklist

- [ ] Replace `SkillRegistry::scan` calls with `SkillRegistry::for_workspace` where possible.
- [ ] Wrap all skill instruction text in `Trust::External`.
- [ ] Update tool registrations: `skill.list`/`skill.load` still work; add `skill.search`/`skill.read`.
- [ ] Remove any code that auto-executes skill scripts.
- [ ] Update tests to use `DiscoveryOptions::with_state_dir` for isolation.
- [ ] Run `cargo test -p vikram-runtime` to verify.
