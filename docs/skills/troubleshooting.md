# Troubleshooting

## Common problems

### "no skills found"

- Check that `<VIKRAM_HOME>/skills/` exists and contains skill directories.
- Each skill directory must contain a `SKILL.md` file.
- Run `vikram skills doctor` to check the home directory and scan results.

### "skill not found" after install

- The skill may be disabled. Run `vikram skills enable <name>`.
- The skill may be in a different scope. Run `vikram skills list --all`.
- The catalogue may be stale. Restart the session (catalogue is built per-turn).

### "permission denied" on skill files

- Check file permissions on the skill directory.
- On Linux, ensure the user has read access to `<VIKRAM_HOME>/skills/`.

### "budget exceeded" / skill body truncated

- The skill body exceeds `instruction_bytes` (default 24,000). Shorten the body or split into resources.
- The catalogue exceeds `catalog_bytes` (default 16,000). Reduce the number of skills or shorten descriptions.

### "name does not match directory" warning

- The `name` field in `SKILL.md` differs from the directory name. Either rename the directory or fix the field.
- This is a warning, not an error. The skill still loads.
- `vikram skills create` derives the directory name from the `name` field, so this cannot happen for created skills.

### "missing frontmatter" error

- The `SKILL.md` file must start with `---` on its own line.
- Check for BOM or whitespace before the opening delimiter.

### Git install fails

- Ensure `git` is installed and on `PATH`.
- The installer sets `GIT_TERMINAL_PROMPT=0` to prevent interactive prompts.
- Raw SHAs are refused; use `--branch` instead.
- Network access is required for git installs.

### TUI skills browser shows "no skills found"

- The browser reads from `std::env::current_dir()`. Run VIKRAM from the project root.
- Check that the skills directory exists and is readable.

## Debugging

- `vikram skills doctor` — checks home directory, scan results, trust policy, bundles.
- `vikram skills validate <path>` — validates a skill directory and prints diagnostics.
- `vikram skills list --json` — machine-readable skill list.
- Set `VIKRAM_HOME` to a temp directory for isolated testing.
- `RUST_LOG=debug` for verbose runtime logging (if supported by the build).
