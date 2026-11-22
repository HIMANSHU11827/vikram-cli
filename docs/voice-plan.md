# Voice plan — from text runtime to voice-first

Status: 2026-11-21; §3 rewritten 2026-11-22 to describe the shipped interface.
Scope: how Vikram gets ears and a voice on Linux, in what order, what proves each
phase, and what we honestly cannot promise.

This plan is subordinate to `docs/jarvis-class.md`. The film JARVIS is concept only;
"voice-first" here means a real, working voice loop with honest limits, not a
hologram claim.

## 0. What existed when this plan was written (2026-11-21)

> Superseded by §3 for anything about the shipped code. Kept because the
> starting point is what justifies the order of the phases.

- No audio dependency anywhere in the workspace (verified: `grep -r 'cpal\|rodio\|whisper\|tts' Cargo.toml crates/*/Cargo.toml` — zero hits).
- `computer.*` helpers already shell out to host binaries (`wl-paste`, `xclip`, `notify-send`) with graceful missing-helper degradation — the established pattern this plan reuses.
- Runtime is text-in/text-out: `RuntimeHandle::send`, `Block::Text/Image`, the gateway delivers text + ledgered verbs.
- `vikram-terminals` + ACP PTY exists; the daemon's event-trigger work has
  landed (`on <trigger>` schedules, quiet hours — `196f731`).

## 1. The pipeline (mirroring the Alexa-class stages from jarvis-class.md §1.4)

```
microphone → VAD → wake word → ASR → transcript → RuntimeHandle::send → reply
                                                                          ↓
                                              TTS ← text normalization ← reply text
                                                    ↓
                                              speaker
```

Two hard requirements no plan can dodge:

1. **Everything degrades honestly.** No mic, no speaker, no model file, no
   network — each stage reports "unavailable: <reason>" and Vikram stays a
   text runtime. This is the `missing_tool_message` pattern, not an exception.
2. **Nothing runs that the operator didn't permit.** Mic capture, speaker
   output and wake-word listening are all permission-gated actions, deny-first,
   like `computer.clipboard_write` today.

## 2. Stage-by-stage choices (Linux first; macOS/Windows later)

### 2.1 Audio I/O

| Need | Primary | Fallback | Notes |
|---|---|---|---|
| Mic capture + playback | `cpal` crate | `parec`/`paplay` (PulseAudio/PipeWire) helpers | cpal is the standard Rust audio I/O crate; verify version at add time. Helper path matches computer.* conventions and works where cpal's ALSA/Pulse backends don't. |
| Format | 16 kHz mono s16le in, 24 kHz+ mono out | — | ASR models are trained on 16 kHz; TTS accepts 22.05–24 kHz. Resample once at the boundary, never per-stage. |

### 2.2 VAD (voice activity detection)

| Option | Mechanism | Honest verdict |
|---|---|---|
| Energy/RMS gate | RMS threshold + hangover in `vikram-voice` | Zero deps, works badly in noise. Phase-1 acceptable, must be labeled heuristic. |
| Silero VAD via `ort` | ONNX runtime, ~2 MB model | Real VAD, needs model file + `ort` dep (verify version). Phase 2. |
| webrtc-audio-processing | C library binding | Heavy; skip unless ort fails. |

### 2.3 Wake word

Honest position: **no reliable open wake-word engine exists for Rust**. Options:

- **Push-to-talk first** (default): a keybind or `vikram voice listen` command.
  Zero false-accepts, zero model files. This is the Phase-1 answer.
- **OpenWakeWord** (Python, ONNX): real but Python-side; could be shelled out
  like a helper binary. Phase 3 candidate, marked experimental.
- **Porcupine** (Picovoice): excellent, but requires an access key — a secret,
  and a vendor dependency. Documented as an option, not built-in.

### 2.4 ASR

| Option | Mechanism | Verdict |
|---|---|---|
| wavo 0.6 (pure Rust) | Local, wgpu (Vulkan, CPU fallback built in), GGUF weights | **Chosen engine.** No C/C++, no ONNX runtime; `whisper-base.en-Q8_0.gguf` (~80 MB) pulled once into `<home>/.vikram/models` on the first utterance. |
| Voxtype (if installed) | External CLI fast path | Optional accelerator, never required — Vikram works without it, and `voxtype_fast_path = false` turns it off. |
| `whisper-cli` binary | Shelled out on a temp WAV | Last-resort helper when the built-in engine cannot run; no accuracy gain. |
| Cloud ASR (provider APIs) | Network | Optional provider seam — but voice must work offline; never the only path. |

Model choice is one config key, `asr_model`: a wavo hub name (`whisper-base.en`
is the default; larger hub models trade latency for accuracy) or a downloaded
`.gguf` path. whisper.cpp `.bin` (GGML) files cannot be loaded by wavo, and the
old tier constants are gone.

### 2.5 NLU

None. Vikram *is* the NLU — the transcript goes straight into
`RuntimeHandle::send` as a normal user turn. This is the architectural advantage
over Alexa-class pipelines: no intent enumeration, no coverage ceiling. The
trade-off is latency and cost per utterance, stated honestly.

### 2.6 TTS

| Option | Mechanism | Verdict |
|---|---|---|
| `piper` binary | Local neural TTS, ONNX voices, ~50–100 MB | Primary. Shell-out helper, consistent with computer.* pattern. Voices are operator-downloaded files. |
| `espeak-ng` | Formant synthesis | Always-available fallback; robotic but zero-download. |
| `say`/SAPI (other OSes) | Platform | Later. |
| Cloud TTS | Provider APIs | Optional seam only. |

### 2.7 Personality in voice

TTS prosody carries personality. Phase 3+ allows per-persona voice profiles
(speed/pitch/voice model) stored beside the entity's persona config — the same
identity the text persona uses, not a second personality.

## 3. Crate layout and interface (shipped)

> **Status (2026-11-22): implemented.** `crates/vikram-voice` holds `asr.rs`,
> `capture.rs`, `player.rs`, `session.rs`, `vad.rs`, `helpers.rs`, `cancel.rs`.
> This section is what the code does today, read off the source. The original
> layout sketch in §3.5 is kept for history only. **What is still missing is
> not listed here** — it is ranked, with line-number evidence and acceptance
> tests, in [`plans/2026-11-22-voice-upgrade.md`](plans/2026-11-22-voice-upgrade.md).

### 3.1 Engines

| Stage | Engine today | Honest limit |
|---|---|---|
| Capture | built-in `cpal` 0.18 with the `pipewire` feature; `parecord`/`arecord` only when cpal sees no device | one stream opened per utterance, not kept warm |
| VAD | energy gate: 10 ms RMS frames, noise floor = 10th percentile of the take, gate = whichever is higher of `noise_floor × floor_ratio` (2.5) and `rms_threshold`, `min_speech_ms` 250, hangover 200 ms | energy only — a door slam or music can open the gate; it cannot separate speech from loud non-steady noise |
| ASR | **wavo 0.6**, pure Rust, GGUF on wgpu. Default hub model `whisper-base.en` (Q8_0, ~80 MB), downloaded once into `<home>/.vikram/models` on the **first utterance** | batch only: no partials, no confidence, no word timestamps used |
| ASR fast path | `voxtype` CLI, used only when it is already on PATH **and** `voxtype_fast_path` is true | optional accelerator; never required, and its absence changes nothing |
| ASR last resort | `whisper-cli` helper on a temp WAV | needs whisper.cpp installed; no accuracy gain over wavo |
| TTS | `piper` when a voice `.onnx` is configured or discovered, otherwise **espeak-ng** — the honest fallback, robotic, and reported as `degraded` in `voice status` | whole file synthesized before a single sample plays; no streaming |
| Playback | built-in `cpal` output; `paplay`/`aplay` only as fallback | abort only — no duck, no resume |

Every stage reports `ready` / `degraded` / `missing` / `disabled` with a reason;
`voice status` prints all five lines without opening a device or reading a model.

### 3.2 CLI — `vikram voice`

```
vikram voice status
vikram voice listen [--enable] [flags]
vikram voice say    [--enable] <text> [--voice PATH]
vikram voice chat   [--enable] [flags] [--voice PATH] [--fresh]
vikram voice config show
vikram voice config path
vikram voice config set <key> <value>
```

Voice I/O is opt-in per invocation: nothing opens a microphone or a speaker
without `--enable` on `listen`, `say` or `chat`.

Flags, and whether they outlive the run:

| Command | Flag | Effect | Persisted to `voice.json`? |
|---|---|---|---|
| `status` | `--asr-model PATH` | check this model instead of the saved one | no |
| `listen` | `--enable` | open the audio devices for this run | no — `enabled` in the file is untouched |
| `listen` | `--asr-model PATH` | ASR model for this run (path or wavo hub name) | no — use `config set asr_model` to keep it |
| `listen` | `--max-record-ms MS` | recording cap for this run | no — use `config set max_record_ms` |
| `say` | `--voice PATH` | piper `.onnx` voice for this run | no — use `config set tts_voice` |
| `chat` | `--fresh` | start a new session instead of continuing the most recent | n/a (session state) |
| `chat` | `--enable`, `--asr-model`, `--max-record-ms`, `--voice` | same as above, for the spoken turn | no |
| `listen`, `chat` | `--language CODE` | ASR language; `auto` lets wavo detect it | **yes**, written back after a successful transcript |
| `listen`, `chat` | `--input-device NAME` | input device, matched case-insensitively | **yes** |
| `listen`, `chat` | `--output-device NAME` | output device, matched case-insensitively | **yes** |
| `listen`, `chat` | `--threshold LEVEL` | `rms_threshold`, 0–1 | **yes** |
| `listen`, `chat` | `--min-speech MS` | `min_speech_ms` | **yes** |
| `listen`, `chat` | `--floor-ratio R` | `floor_ratio`, 1–20 | **yes** |

Precedence is flag, then `voice.json`, then the built-in default. Only values
that came from a flag are written back; a path that came from
`VIKRAM_ASR_MODEL` or `VIKRAM_TTS_VOICE` is not baked into the file, and
`--enable` is never persisted.

`chat` is one utterance per process: record → normal runtime turn → speak the
reply. The spoken part stops at the last sentence inside 1 200 bytes; the rest
stays on screen.

### 3.3 The config file

`vikram voice config path` prints the one file that holds every tunable:

```
$VIKRAM_HOME/.vikram/voice.json      # when VIKRAM_HOME is set
~/.vikram/voice.json                 # otherwise
```

Models live beside it under `<home>/.vikram/models`. `config show` prints the
path and the JSON; `config set` accepts the JSON name, the kebab spelling a flag
uses, or a dotted path (`voice.input_device`, `asr.language`), validates the
whole struct before saving, and treats `none` as "clear this optional field".
A key the struct does not have is refused with the list of valid keys, so a typo
never silently does nothing.

Keys that matter day to day:

| Key | Default | What it does |
|---|---|---|
| `asr_model` | empty → `whisper-base.en` | a `.gguf` path, or a wavo hub name |
| `models_dir` | empty → `<home>/.vikram/models` | where models are looked up and downloaded |
| `asr_language` | none (auto-detect) | pin the spoken language |
| `voxtype_fast_path` | `true` | use the `voxtype` CLI when it is installed |
| `tts_voice` | none → espeak-ng | piper `.onnx` voice |
| `rate` | 175 | words per minute, 80–450; piper gets `175/rate` as `--length_scale` |
| `volume` | 1.0 | output amplitude, 0–1 |
| `input_device`, `output_device` | none → system default | exact or case-insensitive device name |
| `max_record_ms` | 8 000 | recording cap |
| `rms_threshold` | 0.01 | absolute floor a frame must clear |
| `floor_ratio` | 2.5 | how far above the measured noise floor counts as voiced |
| `min_speech_ms` | 250 | a shorter voiced run is a click, not a word |
| `hangover_ms` | 200 | silence kept after the last voiced frame |
| `sample_rate` | 16 000 | ASR rate; capture resamples to it |

`capture_helper`/`capture_fallback`, `asr_helper`, `tts_helper`/`tts_fallback`,
`player_helper`/`player_fallback`, `asr_timeout_secs`, `enabled` are also
settable; the helper names are the fallback tiers, not the primary engines.

### 3.4 TUI

- `/voice` — hands-free mode. One worker owns the microphone and the model for
  the whole mode, so a session is not reopened per utterance. It deliberately
  never listens while it speaks: a microphone left open would transcribe the
  assistant's own voice as the next turn.
- `Ctrl+G` — one dictation. The transcript is typed into the prompt and never
  auto-sent, so the operator reads what was heard before it becomes a turn.
- The status bar shows `◉ voice` while the mode is on.

### 3.5 Original layout sketch (historical)

```
crates/vikram-voice/src/
  lib.rs        — public API surface, VoiceConfig, VoiceStatus
  capture.rs    — mic → frames (cpal; parecord/arecord fallback)
  vad.rs        — energy gate with an adaptive noise floor
  asr.rs        — wavo engine, Voxtype fast path, whisper-cli last resort
  player.rs     — speaker out (cpal; paplay/aplay fallback)
  session.rs    — voice loop: listen → transcribe → send → speak
  helpers.rs    — helper discovery, scratch dirs, WAV write
  cancel.rs     — SIGINT cancel token
```

Public API:

```rust
VoiceSession::start(config) -> Result<VoiceSession>
VoiceSession::listen_once() -> Result<Option<String>>   // None = silence
VoiceSession::speak(text) -> Result<()>
VoiceSession::status() -> VoiceStatus                   // each stage: Ready/Degraded/Missing/Disabled
```

`listen_once` returning `Option` (not `Result<String>`) is deliberate: silence
is a normal outcome, not an error.

Permissions: `voice.listen` and `voice.speak` are tool-permission strings in
the existing deny-first system; the voice session refuses to open the mic or
the speaker without them.

## 4. Phases and acceptance criteria

### Phase 1 — push-to-talk loop (the honest MVP)

- `vikram voice` command: press key → record → VAD-trim → ASR → send → TTS reply.
- Deps: `cpal` + `wavo` (as shipped; the original sketch said `whisper-rs`) +
  piper/espeak helpers. The `whisper-cli` helper remains a last-resort path.
- **Accept:** on a machine with a mic+speaker and downloaded models,
  `vikram voice` completes one utterance→reply round-trip; `vikram voice
  status` reports each stage Ready/Missing with reasons; with no mic the
  command exits with an explicit message and Vikram still works as text.
- **Verify:** integration test with a synthesized WAV fixture through ASR
  (model file present in CI cache or test is `#[ignore]`d with docs), unit
  tests for VAD trimming and session state machine with fake audio sources.

### Phase 2 — always-listening with VAD + Silero

- Continuous capture, Silero VAD segmenting speech, endpointing, then ASR on
  the segment only (not push-to-talk).
- **Accept:** speaking a sentence near the mic produces a turn without a
  keypress; silence for N seconds returns to idle; CPU stays bounded
  (documented budget, e.g. <15% of one core idle).
- **Verify:** fixture-driven tests (pre-recorded speech segments), VAD
  precision/recall numbers recorded in the test output.

### Phase 3 — wake word + barge-in

- OpenWakeWord helper for "vikram"/"hey vikram"; TTS playback interruptible
  by detected speech (barge-in).
- **Accept:** wake word triggers listen without keypress; speaking during TTS
  stops playback within ~300 ms.
- **Verify:** wake-word false-accept rate measured on a test corpus; documented,
  not hidden.

### Phase 4 — voice persona + proactive voice

- Per-entity voice profiles; daemon may speak proactive notifications
  (quiet-hours-aware, from the proactivity work).
- **Accept:** two entities have distinct voices; a proactive alert speaks
  only when policy permits.

## 5. Honest limits (stated up front)

- **Latency.** wavo on `whisper-base.en` ≈ 1–3 s per utterance on CPU (plan-time
  estimate, not measured here); `tiny` ≈ 0.5–1 s. The model also loads per
  process, which adds seconds to a one-shot `voice listen`.
  JARVIS-class sub-second response is not achievable on this hardware with
  local models — cloud ASR could hit it but breaks offline-first. We do not
  claim it.
- **Accurate names/terms.** ASR mangles identifiers; transcripts pass through
  the same correction path as typed input (relation/entity lookup can repair).
- **No always-on guarantee.** Phase 2+ listens continuously only while the
  session/daemon runs; sleep, mute and permission revocation all stop it.
- **Privacy.** Mic audio is processed locally; transcripts follow the same
  session/journal rules as typed input. Audio itself is never persisted unless
  the operator opts into debug recording.
- **Platform.** Linux/PipeWire first. macOS `say`/Windows SAPI are later
  phases; ALSA-only machines get the helper path, not cpal.

## 6. What this plan refuses to do

- No bundled model weights (license + size); downloader + checksum only.
- No cloud-only voice path; offline must work.
- No fake "voice" that is TTS reading canned lines — every spoken reply is a
  real model turn.
- No wake-word claims without measured false-accept numbers.
