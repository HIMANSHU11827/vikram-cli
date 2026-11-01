# Testing

## Crate tests

`cargo test -p vikram-skills` runs 190 tests covering:

- Frontmatter parsing: missing fence, malformed YAML, duplicate keys, over-long name/description,
  name/dir mismatch, unknown-key round trip, hash determinism.
- Discovery: cross-scope precedence, disabled filtering, path escape, symlink refusal, budget
  truncation, selection reasons.
- Manager: atomic create collision, delete-to-trash, enable/disable persistence.
- Bundle: missing member.
- Install: git refusal with network off.

## Integration tests

`crates/vikram-skills/tests/discovery_and_trust.rs` contains 8 tests:

- Discovery from a temp root.
- Name/dir mismatch produces a Warning but still loads.
- Duplicate names across scopes produce a conflict.
- Missing frontmatter is reported, not silently dropped.
- Trust level mapping: user, project, external.
- Deny override.

## Runtime tests

`cargo test -p vikram-runtime` runs 143 tests including the `every_builtin_declares_a_permission_shape`
test that verifies every builtin tool declares a permission shape, including `skill.search` and
`skill.read`.

## CLI tests

`cargo test -p vikram-cli --bins` runs 10 tests covering row truncation, JSON output, scope
arguments, and character (not byte) counting.

## TUI tests

`cargo test -p vikram-tui` runs 71 tests including the skills overlay rendering.

## Verification commands

```sh
cargo test -p vikram-skills
cargo test -p vikram-runtime
cargo test -p vikram-cli --bins
cargo test -p vikram-tui
cargo test --workspace
cargo clippy -p vikram-skills --all-targets -- -D warnings
cargo clippy -p vikram-runtime --lib -- -D warnings
cargo clippy -p vikram-tui --lib -- -D warnings
```

## Non-negotiables

- No test is claimed passing without running it and reading the exit code.
- `VIKRAM_HOME` is process-global; tests that touch it serialize on a `static Mutex<()>`.
- No workspace-wide `cargo fmt`.
