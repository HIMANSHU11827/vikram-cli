# CLI — Operator Commands

`vikram` with no subcommand launches the TUI. Subcommands work on the same durable store
(no second source of truth).

## Command groups

sessions, project, workspace, files, config, doctor, allow, tools, mcp, skills,
users, profiles, entities, hive, tickets.

## Key commands

- `vikram chat "..."` — headless single message (forces unattended posture).
- `vikram sessions ...` — session management.
- `vikram tickets ...` — unified ticket management.
- `vikram hive run <id> <msg>` — run a Hive conversation.
- `vikram config set <key> <value>` — configure.

## Voice

Local only. Capture, ASR (wavo, `whisper-base.en`) and TTS (piper, else espeak-ng)
all run on this machine; nothing is sent to a provider. Voice I/O is opt-in per
invocation — without `--enable` no device is opened.

```sh
# What is available, with a reason for every stage that is not ready.
vikram voice status

# One utterance, printed. Nothing is sent to a model.
vikram voice listen --enable

# Speak a string through the local player.
vikram voice say --enable "build finished"

# One utterance → a normal turn → the reply spoken.
vikram voice chat --enable

# Persist a setting (validated before it is written).
vikram voice config set asr_language en   # or: voice listen --language en
vikram voice config set input_device "alsa_input.pci-0000_00_1f.3.analog-stereo"  # name from: pactl list short sources
vikram voice config set output_device "alsa_output.pci-0000_00_1f.3.analog-stereo"
vikram voice config set floor_ratio 3.0
vikram voice config show      # path + JSON
vikram voice config path      # path only, for scripts
```

In the TUI: `/voice` toggles hands-free mode (one worker owns the mic until you
turn it off), `Ctrl+G` dictates one utterance into the prompt without sending it.

Flags on `listen`/`chat` (`--language`, `--input-device`, `--output-device`,
`--threshold`, `--min-speech`, `--floor-ratio`) override the saved config for one
run and are written back afterwards, so the next run needs no flags.
`--asr-model`, `--max-record-ms` and `--voice` apply to that run only.

Config file: `$VIKRAM_HOME/voice.json`, else `~/.vikram/voice.json`.
Full engine and flag reference: [`voice-plan.md`](voice-plan.md) §3.

## Files

- `crates/vikram-cli/src/main.rs`
- `crates/vikram-cli/src/{chat,doctor,skills,terminal,tools}.rs`

## Notes

`runtime()` uses `.expect()` (panic on failure). MCP serve is real (`vikram mcp serve`
starts a JSON-RPC stdio server behind a deny-first `approved_tools` gate — see
`crates/vikram-mcp/src/server.rs`). Doctor terminal size reads env.