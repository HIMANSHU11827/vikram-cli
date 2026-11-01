# Compatibility Fixtures and Integration Tests

How VIKRAM tests skill compatibility across formats and runtimes.

## Fixture location

`crates/vikram-skills/tests/fixtures/` — each subdirectory is a complete skill directory:

```
tests/fixtures/
├── native-valid/           # minimal valid native skill
├── native-mismatch/        # name != directory (warning)
├── agents-format/          # .agents/skills/ compat
├── claude-format/          # .claude/skills/ compat
├── opencode-format/        # .opencode/skills/ compat
├── hermes-format/          # .hermes/skills/ compat
├── codex-format/           # Codex-style (agents/openai.yaml)
├── missing-frontmatter/    # no --- fence
├── malformed-yaml/         # invalid YAML
├── duplicate-name/         # two skills with same name
├── path-escape/            # symlink escape attempt
└── oversized-description/  # description > 1024 chars
```

## Integration test pattern

```rust
#[test]
fn discovers_native_skill() {
    let tmp = tempfile::tempdir().unwrap();
    // copy fixture to tmp
    let opts = DiscoveryOptions::roots_only()
        .with_root(Scope::Project, Compat::Native, tmp.path().join("skills"));
    let catalog = Catalog::discover(&opts);
    assert_eq!(catalog.list().len(), 1);
}
```

## Running

```bash
cargo test -p vikram-skills --test '*'   # all integration tests
cargo test -p vikram-skills              # unit + integration
```

## What is tested

- Discovery across all six compat formats.
- Name/directory mismatch produces a warning, not an error.
- Missing frontmatter is reported, not silently dropped.
- Duplicate names are both kept (no merge).
- Path escape via symlink is refused.
- Trust level mapping per scope/compat.
- Deny rules (source id, path, name).
- Budget truncation reports omitted count.
- Enable/disable persistence across reloads.
- Bundle resolution with missing members.
- History recording and rollback.

## Adding a new fixture

1. Create a directory under `tests/fixtures/`.
2. Add a valid `SKILL.md` (and optional `agents/openai.yaml`, `scripts/`, `references/`).
3. Add a test in `tests/` that loads the fixture and asserts the expected behavior.
4. Run `cargo test -p vikram-skills` to verify.
