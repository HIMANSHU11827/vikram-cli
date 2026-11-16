# TUI — Terminal Interface

ratatui + crossterm. State, rendering, input and hit-testing live here. The UI subscribes to the
runtime event bus; changing a view never stops work.

## Layout

`layout(area, show_hive, show_bots, input_lines, show_notice)` in `src/layout.rs` returns the
regions, top to bottom:

| Region | Height | What it is |
|---|---|---|
| session tabs | 1 | One tab per live session, `+` starts another |
| main | fills | Conversation transcript, plus the HIVE and BOTS panes when shown |
| notice | 1, only when there is one | The newest notice, directly above the composer |
| input | 1–8 | The composer, grows with the draft |
| shortcuts | 1 | Keys that apply right now, for the focused pane |

The side panes collapse instead of squeezing the transcript: BOTS needs 88 columns
(`BOTS_MIN_WIDTH`) and 16 rows (`BOTS_COLLAPSE_HEIGHT`). The notice row is refused entirely on a
terminal too short to spare it — `area.height.saturating_sub(fixed + 1) >= 3` — rather than
shaving the conversation.

## The notice row

Every action that has nothing to show answers here instead of returning silently: an empty
`/allow` pressed with Enter says `` `allow` has nothing to forget yet — it needs a row first ``.
The line carries the level, the text and a rule, so a warning is not mistaken for chatter.

Notices expire. `App::live_notice()` returns the newest one only while it is younger than
`NOTICE_TTL_MS` (8 s; errors get 24 s because they are the only record of why something failed).
When it ages out the row goes back to the conversation. The ticker redraws every 120 ms, so the row
disappears without the operator touching anything.

## The composer

`Tab` flips the send mode; the border title always says which mode is armed:

```
┏❯ queue …      Enter sends, the turn starts now
┏❯ steer …      Enter is not the trigger; Ctrl+Enter steers the running turn
```

With work in flight the title also carries the pending text: `┏❯ steer <first 48 chars> …` for a
steer, `┏❯ queue <text or "N pending"> …` when turns are queued behind the current one. The mode
and the payload are visible at the same time, so a message cannot be sent in the wrong mode by
accident.

`Enter` sends, `Shift+Enter` inserts a newline, `Ctrl+Enter` steers. There is no `/steer` command:
steering is a property of the composer, not a verb.

## Slash commands

Typing `/` opens the popup; `Enter` on a highlighted row runs it. There is one table
(`src/commands.rs`, `COMMANDS`) behind the popup, the palette, `/help` and the dispatcher, so a
command can never be offered by the type-ahead and then answered with "unknown command".

Three outcomes:

- **Picker** — the command cannot be answered in one word, so it opens the chooser for its kind.
- **Info** — the answer is a read-only panel of what the runtime already knows.
- **Action** — it does exactly one thing and closes.

| Command | Aliases | What it does |
|---|---|---|
| `/model` | | Choose the model this session runs on |
| `/permissions` | | Approval posture, policy and saved rules |
| `/allow` | | Saved allow-list rules |
| `/provider` | | Choose the endpoint this session runs on |
| `/sessions` | `/resume`, `/switch` | Switch to another conversation |
| `/rename` | | Rename this conversation |
| `/close` | | Close this conversation |
| `/tasks` | | Background tasks; choosing one stops it |
| `/queue` | `/pending` | Pending steer/queue messages: edit, delete, reorder, flip |
| `/approvals` | | What is waiting for your answer |
| `/memory` | | What the model has remembered |
| `/bot` | `/bots`, `/agents`, `/agent` | Bots in this session |
| `/hive` | `/hives`, `/team`, `/teams` | HIVE chats and their members |
| `/skills` | | Skills discovered on this machine |
| `/tools` | | Tools this runtime can call |
| `/plugins` | | Plugins installed and loaded |
| `/mcp` | | MCP servers; choosing one flips its visibility |
| `/files` | | Reference a workspace file |
| `/status` | | Runtime and session status |
| `/usage` | | Token usage for this session |
| `/context` | | How much of the context window is in use |
| `/compact` | `/summarise`, `/summarize` | Fold this conversation's older turns into a summary now |
| `/settings` | `/prefs`, `/preferences` | Interface preferences that apply now |
| `/keys` | `/keymap`, `/shortcuts` | Every key this interface listens for |
| `/doctor` | | Check local prerequisites |
| `/diff` | | Run git diff in the workspace |
| `/test` | | Run the test suite |
| `/build` | | Build the workspace |
| `/clear` | | Clear this conversation |
| `/new` | | Start a new session |
| `/version` | | Which build this is |
| `/help` | `/?` | Browse every command |
| `/exit` | `/quit` | Quit VIKRAM |
| `/schedule` | `/cron` | Scheduled jobs and the daemon that runs them |
| `/copy` | `/copy-last`, `/copylast` | Copy the last assistant reply to the clipboard |

Near misses resolve: `/session` finds `/sessions`, `/ski` finds `/skills`.

`!` runs a shell command; the suggestions are `git status`, `git diff`, `git diff --stat`,
`git log --oneline -20`, `git branch --show-current`, `cargo build`, `cargo test`,
`cargo clippy --all-targets`, `cargo fmt --check` and `ls`. `@` completes a workspace path.

### Read-only panels wear no fake buttons

`/status`, `/usage`, `/context`, `/doctor` and `/version` report state, so their footer carries
`esc close` and nothing else. A panel that offered `delete` or `rename` over data it cannot change
would be a lie the operator only discovers by pressing it. Read-only panels also dismiss on a press
outside them; panels holding a pending answer do not, so a stray click can never commit a rule.

## Keys

Global:

| Key | Does |
|---|---|
| `Ctrl+C` | Close the overlay or popup; cancel the running turn, or quit when idle |
| `Ctrl+V` | Paste the system clipboard into the focused field |
| `Ctrl+N` | New session |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous session |
| `Ctrl+B` | Show or hide the bots pane |
| `Ctrl+P` | Help |
| `Ctrl+L` | Repaint the screen, history preserved |
| `Ctrl+T` | Expand or fold thinking rows |
| `Ctrl+O` | Approvals |
| `Ctrl+R` | Reverse search over this session's input |
| `Ctrl+A` | Open a bot |
| `F1` | Help |
| `F2` | Background tasks |
| `F3` | MCP servers |
| `F4` | Plugins |
| `F5` | Sub-agents |
| `F6` | Context |
| `F7` | Model |
| `F8` | Skills (scrolls: `Up`/`Down`/`PageUp`/`PageDown`/`Home`/`End`, mouse wheel) |
| `?` | Help, when the composer is empty |
| `PageUp` / `PageDown` | Scroll the transcript |
| `Esc` | Leave the focused pane and give the keys back to the composer; cancel when a turn is running |

Composer:

| Key | Does |
|---|---|
| `Enter` | Send |
| `Shift+Enter` | Newline |
| `Ctrl+Enter` | Steer the running turn |
| `Tab` | Flip send mode (queue ↔ steer) |
| `Ctrl+U` | Clear the line |
| `Ctrl+W` | Delete the previous word |
| `Ctrl+Home` / `Ctrl+End` | Start / end of the draft |
| `Home` / `End` | Start / end of the line |
| `Left` / `Right` | Move a character (`Ctrl` moves a word) |
| `Up` / `Down` | Move a line |
| `Backspace` / `Delete` | Delete before / after the cursor |

Bots pane: `Up`/`Down` select, `Enter` open that bot, `Esc`/`Tab` back to the composer, `p` pause,
`s` stop, `r` resume.

Approvals: `Up`/`Down` pick the request (when several wait), `Tab`/`Left`/`Right` move between
the answers, `Enter` gives the selected answer, `a` approve once, `p` approve and remember,
`d` deny, `Esc` close.

Picker: `Esc` close, `Enter` commit the selection, `Up`/`Down`/`PageUp`/`PageDown`/`Home`/`End`
move, `Backspace` clears the filter. `Ctrl+D`/`Ctrl+U` half-page only when the panel's footer does
not bind `d`/`u`; any `Ctrl+<letter>` first consults the footer verb for that letter, so a panel
that prints `ctrl+x close` really answers to it.

Popup: `Up`/`Down` move, `Tab` completes, `Enter` completes and runs a command (a half-typed
`@path` is only completed), `Esc` dismisses.

## Mouse

Every chip and every footer band is a hit target, not decoration. The rectangle a widget paints is
the rectangle the mouse answers to, recorded during draw:

- footer verbs → `app.picker_verbs`, answered through the same `commit_verb` the chords use
- dialog chips → `app.overlay_clicks`
- session tabs → `app.tab_hits` (click the label to switch, the `×` to close)
- the `+` tab → `app.new_session_x`
- bot rows and controls → `app.bot_hits`
- foldable activity rows → `app.thought_hits` / `fold_at`
- picker rows → `app.picker_x` / `picker_rows`
- suggestion rows → `app.suggestion_rows` (accept like Tab)
- autocomplete candidates, history-search matches, approval request rows → `app.overlay_clicks`
- bots pane / transcript bodies → `app.bots_rect` / `app.conversation_rect` (a press focuses the pane)
- overlay bounds → `app.overlay_rect`, used to dismiss a read-only panel on an outside press

Scroll wheel scrolls the transcript, or the picker list when one is open. Clicking inside the
bots pane or the transcript hands it the keyboard (`Esc`/`Tab` or any unhandled key returns to
the composer), which is what makes the pane keys below reachable.

A press that lands on a band whose panel is empty does not vanish: it raises the same notice the
keyboard does.

## Files

- `crates/vikram-tui/src/app.rs` — `App`, snapshot folding, notices, hit rectangles
- `crates/vikram-tui/src/commands.rs` — the one slash-command table
- `crates/vikram-tui/src/input.rs` — keys, popup, picker commits
- `crates/vikram-tui/src/layout.rs` — regions
- `crates/vikram-tui/src/theme.rs` — palette, glyphs, `--ascii` fallback
- `crates/vikram-tui/src/widgets/*` — transcript, composer, panels, picker
- `crates/vikram-cli/src/terminal.rs` — event loop, mouse dispatch, host actions

## Notes

The palette is fixed ink, not a colour scheme: one accent (`255,138,0`) marks focus and the armed
verb, everything else is grayscale. No rounded corners, no fills except the selection band.

Panel geometry is derived, never hardcoded per view: the BOTS pane needs
`BOTS_MIN_WIDTH = 88` columns and `BOTS_COLLAPSE_HEIGHT = 16` rows, and collapses below that.

Standing rule for this crate: if a control is drawn it must work, and the keyboard must be able to
do everything the mouse can. A drawn label with no handler is worse than a missing feature,
because the operator spends a minute finding out.
