# VIKRAM — what is still open

The transcript work is built, tested and installed. This file holds only what remains.

## Checks that decide "done"

```bash
cd /home/himanshu/Desktop/VIKRAM-CLI
cargo check --workspace --all-targets   # zero warnings
cargo test  --workspace                 # zero failed
cargo install --path crates/vikram-cli --force
```

## Remaining

1. **The approval panel covers the transcript.** While a tool waits for a yes/no, the panel draws
   over the rows above it. The rows should stay readable behind the question.
2. **Activity rows sit flush left.** The mockup indents them by two spaces. Deferred on purpose:
   the indent touches `push_reasoning`, `push_speech`, `push_tool_row` and their tests at once.
3. **Activities are in memory only.** A session reloaded from the store shows the messages but no
   tool rows, because `SessionView.payloads` is never persisted.
4. **No `web.search` cache.** The same query twice in one turn hits the network twice.
5. **`LICENSE-MIT` / `LICENSE-APACHE` are missing** although `Cargo.toml` declares
   `MIT OR Apache-2.0`.
6. **`browser.*` and `computer.*` are untested here.** They need headless Chromium and the desktop
   helpers; the rest of the tool set is covered by real-execution tests in
   `crates/vikram-runtime/src/tools.rs` (`mod tests`, the block after the LSP tests).

## Not a bug

- Two `✓ Wrote` rows for one request: the model called `fs.write` twice with different call ids.
  The renderer keys rows by call id, so both are real calls.

## Fixed in the last pass

- `/permissions` changes the posture and the choice now survives a restart
  (`RuntimeHandle::set_mode` writes `$VIKRAM_HOME/config.json`).
- `terminal` opens a PTY under the name the caller gave it, so `open` then `write` no longer
  answers `unknown session`.
