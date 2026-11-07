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

1. **Activity rows sit flush left.** The mockup indents them by two spaces. Deferred on purpose:
   the indent touches `push_reasoning`, `push_speech`, `push_tool_row` and their tests at once.
2. **Browser tests are live now** (`crates/vikram-runtime/tests/browser_live.rs`: load/eval, trusted
   click, typing, PNG screenshot, failure bound) against the installed Chromium; `computer.*` still
   needs the desktop helpers.
3. **Gateway: 27 of 27 platforms ported, 27 runnable.** Telegram, Discord, Slack, WhatsApp
   (bridge), WhatsApp Cloud, Matrix, Mattermost, Signal, BlueBubbles, ntfy, Twilio SMS, Feishu,
   LINE, DingTalk, Google Chat, Teams, QQ Bot, Photon, Weixin, WeCom, SimpleX, IRC, A2A and email have
   real adapters with mock-tested wire behaviour (`docs/gateway-platforms.md` documents every transport). `vikram gateway` runs all
   twenty-seven end-to-end: Telegram, Discord, Signal, ntfy, Weixin, SimpleX, IRC, Matrix, Mattermost, WhatsApp (bridge and Cloud), WeCom, Email, Photon,
   Yuanbao, Buzz, Raft (wake-only), DingTalk (stream), Feishu, LINE, Slack, QQ Bot, Google Chat,
   Teams, BlueBubbles, Twilio SMS and A2A.
   Every ported platform is now wired. Per-adapter media/rich-message features remain the long
   tail (each module header lists its omissions).
4. **Outbound media** is implemented for Telegram, Discord, Matrix, Slack, Feishu, WhatsApp (bridge
   and Cloud), Mattermost, Email, Signal, BlueBubbles, Photon, LINE, WeCom, DingTalk, QQ Bot,
   Weixin, Teams and Yuanbao; the other adapters refuse truthfully with the trait default
   (A2A artifacts, Buzz CLI uploads, Google Chat user-OAuth uploads, IRC/ntfy/raft/SMS have no
   media path in the reference).
5. **Reactions** are implemented for Telegram, Discord, Slack and Matrix (`supports_reactions`,
   `add_reaction`, `remove_reaction`; Matrix redacts its own reaction event, Slack normalises names).
   **Polls** ship for the WhatsApp bridge (`send_poll`, 2–12 options, limits) and **native draft
   streaming** for Telegram (`sendMessageDraft`) and Slack (`chat.startStream`/`chat.appendStream`).
   **Rich cards** ship for Slack (Block Kit), Feishu (interactive), Teams (adaptive), Google Chat
   (`cardsV2`) and Discord (embeds + content mirror); **SimpleX** media sends the reference's
   `filePath` items (image/file/voice). Feishu reactions cache the reaction id for deletion.
6. **Benchmarks and comparison**: `crates/vikram-bench` measures 13 real operations, and
   `docs/comparison.md` records the parity table with an evidence legend. The benchmark exposed a
   quadratic chunker; it now tracks frame length incrementally (64 KiB: ~5 ms → ~0.26 ms, linear).
8. **Browser, computer and TUI**: live Chromium CDP tests, live computer tests (screenshot 1920×1080
   via grim, OCR reads rendered text, honest refusal when xdotool/wmctrl are absent) and a shared
   activity-row indent used by all renderers.
7. **MAPEK execution no longer fakes success**: an action only reports `success` after a verified
   store effect (re-queuing failed reflection jobs); everything else is persisted as an operator
   review proposal and reported as not remediated. Self-adaptation records every unapplied action.

9. **Inbound media reaches the model**: `MessageEvent.media_urls` are fetched through a guarded
   `media_fetch` (http(s) only, private/loopback/link-local hosts refused, magic-byte image sniffing,
   4 items × 10 MiB caps), inlined as `ImageSource::Data` with `Trust::External`, and submitted via
   `submit_message`; failures fall back to the text turn.

## Not a bug

- Two `✓ Wrote` rows for one request: the model called `fs.write` twice with different call ids.
  The renderer keys rows by call id, so both are real calls.

## Fixed in the last pass

- **Image input is in the turn loop.** `ImageSource` (`url` or inline base64) and `Block::Image`
  carry pictures; `Message::user_with_images` builds a user turn; the provider maps them into
  OpenAI content parts (`image_url`); the token estimator charges a fixed image cost; the TUI draws
  `[image: …]` placeholders. Text-only turns keep the plain-string wire shape.
- **`RuntimeInner::metrics()` self-deadlocked.** It locked the `skills` mutex twice inside one
  struct literal, so the first temporary guard was still alive on the second lock. Every runtime
  boot hung for 60s+ and the whole test suite never finished. Fixed by taking one guard.
- **`LspClient::drop` could block forever.** It wrote `shutdown`/`exit` straight to the server's
  stdin; a wedged server (or a grandchild holding the pipe) made the write block. Frames now go to
  a dedicated writer thread and teardown reaps the child with a 2s deadline.
- **`computer::run_raw` could block forever.** `wait_with_output` waits for EOF on stdout, which a
  leftover grandchild can hold open. Output is now drained on detached threads with a deadline.
- **One test deadlocked the suite.** `memory_nudge_due_uses_session_message_count` took the
  process-global `VIKRAM_HOME` lock twice in one body; every sibling test queued behind it.
- **Read-only tool calls in one batch now run concurrently** (`turn.rs`), with a regression test
  that asserts two calls are in flight at once. Calls with side effects, approvals, missing or
  disabled tools stay sequential.
- **Tool rows survive a reload.** The TUI used to show only messages after a restart, because tool
  rows were built exclusively from live events. `replay_activity` now reconstructs one row per call
  (and its receipt) from the persisted transcript, marking calls with no result as still running.
- **`vikram chat -i` is a real multi-turn prompt.** Each line is one turn on the same session,
  with `/new`, `/session <id>`, `/mode <name>`, `/help` and `/exit`, on the same runtime and turn
  loop as the one-shot path.
- **`web.search` has a short TTL cache** (128 entries, 10 min), so the same query twice in a turn
  no longer hits the network twice.
- **The approval panel no longer covers the transcript.** Rows are reserved below the transcript
  for the question, and the transcript is laid out above them (`ui.rs` + `approval_panel_height`),
  with a render test asserting both are visible.
- **`LICENSE-MIT` / `LICENSE-APACHE` added**, matching the declared `MIT OR Apache-2.0`.
- **Clippy is clean:** `cargo clippy --workspace --all-targets` emits zero warnings.
- `/permissions` changes the posture and the choice now survives a restart
  (`RuntimeHandle::set_mode` writes `$VIKRAM_HOME/config.json`).
- `terminal` opens a PTY under the name the caller gave it, so `open` then `write` no longer
  answers `unknown session`.
