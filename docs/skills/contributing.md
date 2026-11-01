# Contributing — adding a new skill format or feature

## Adding a new compat format

1. Add a variant to `Compat` in `crates/vikram-skills/src/skill.rs`.
2. Add the directory to `paths::compat_subdirs()` and `paths::user_skill_dirs()`.
3. Update `discovery.rs` to probe the new directory.
4. Update `format-compatibility.md` with the new format's details.
5. Add tests for discovery and parsing of the new format.

## Adding a new frontmatter field

1. Add the field to `VikramExtension` or `SkillMetadata` in `metadata.rs`.
2. Update `frontmatter.rs` to parse and emit the field.
3. Update `frontmatter-reference.md` with the new field.
4. Add validation rules if needed.
5. Add tests for parsing, validation, and round-trip.

## Adding a new CLI subcommand

1. Add a variant to `SkillsAction` in `crates/vikram-cli/src/skills.rs`.
2. Implement the handler function.
3. Add the subcommand to the help text.
4. Update `cli-reference.md`.
5. Add tests for the new subcommand.

## Adding a new doc

1. Create the file in `docs/skills/`.
2. Follow the existing style: tables for comparison, prose for explanation, no banned words.
3. Update `PROGRESS.md` if the doc is part of a phase.
4. Verify with the banned-word scan listed in the authoring guide.

## Code style

- No `unwrap`/`expect`/`panic!`/`todo!`/`unimplemented!` in non-test code (enforced by `deny`).
- Format only touched files; never run `cargo fmt` workspace-wide.
- Run `cargo clippy -p <crate> --all-targets -- -D warnings` before claiming done.
- Run `cargo test -p <crate>` before claiming done.
- No commits unless asked.
