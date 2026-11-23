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

- Audio is in-tree: `crates/vikram-voice` depends on `cpal` 0.18 (feature `pipewire`), `hound`, and `wavo` 0.6 (pure-Rust GGUF whisper on wgpu). TTS still shells out to `piper`/`espeak-ng`; capture falls back to `parecord`/`arecord` only when cpal sees no device.
- `computer.*` helpers already shell out to host binaries (`wl-paste`, `xclip`, `notify-send`) with graceful missing-helper degradation — the established pattern this plan reuses.
- Runtime is text-in/text-out: `RuntimeHandle::send`, `Block::Text/Image`, the gateway delivers text + ledgered verbs.
- `vikram-terminals` (SSH + Docker backends) and the ACP PTY exist in-tree and are
  reachable through the `terminal.exec` tool; the daemon's event-trigger work has
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
| ASR | **wavo 0.6**, pure Rust, GGUF on wgpu. Default hub model `whisper-base.en` (Q8_0, ~80 MB), downloaded once into `<home>/.vikram/models` on the **first utterance** | batch transcription underneath: no confidence, no word timestamps. Live captions exist (`listen_live` re-transcribes a trailing window every ~400 ms) but the final text is still one batch pass |
| ASR fast path | `voxtype` CLI, used only when it is already on PATH **and** `voxtype_fast_path` is true | optional accelerator; never required, and its absence changes nothing |
| ASR last resort | `whisper-cli` helper on a temp WAV | needs whisper.cpp installed; no accuracy gain over wavo |
| TTS | `piper` when a voice `.onnx` is configured or discovered, otherwise **espeak-ng** — the honest fallback, robotic, and reported as `degraded` in `voice status`. `tts_language` picks the espeak voice tag and the piper voice whose file name carries it | whole file synthesized before a single sample plays; no streaming |
| Playback | built-in `cpal` output; `paplay`/`aplay` only as fallback | abort only — no duck, no resume |

Every stage reports `ready` / `degraded` / `missing` / `disabled` with a reason;
`voice status` prints all five lines without opening a device or reading a model.

### 3.2 CLI — `vikram voice`

```
vikram voice status
vikram voice listen [--enable] [--backend local|webspeech|google|openai|custom] [flags]
vikram voice transcribe <WAV> [--backend local|webspeech|google|openai|custom]
vikram voice say    [--enable] <text> [--voice PATH] [--backend vikram|elevenlabs|openai|custom]
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
| `say` | `--backend NAME` | `vikram`, `elevenlabs`, `openai` or `custom` for this run | no — use `config set tts_provider` |
| `listen`, `transcribe` | `--backend NAME` | `local`, `webspeech`, `google`, `openai` or `custom` for this run | no — use `config set stt_provider` |
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
$VIKRAM_HOME/voice.json              # when VIKRAM_HOME is set (it is the home itself)
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
| `tts_language` | none → the helper's default | espeak voice tag (`en`, `en-gb`, `de`); also picks the piper voice whose file name carries the tag |
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

Permissions: the voice session refuses to open the mic or the speaker unless voice I/O is
enabled, and the CLI keeps it opt-in behind `--enable` / `/voice`. It is not yet part of
the deny-first tool-policy system: `crates/vikram-contracts/src/policy.rs` has no voice
`ActionKind`, so a policy cannot veto the mic the way it can veto a shell command.

### 3.6 Optional cloud voice (ElevenLabs) — opt-in, never the default

The local piper voice stays the default and needs no key, no network and no
billing: `tts_backend` is `local` out of the box, and an existing `voice.json`
without the key keeps loading exactly as before. The cloud voice is opt-in:

```
vikram voice config set tts_backend elevenlabs
vikram voice config set tts_voice_id <elevenlabs-voice-id>
export ELEVENLABS_API_KEY=...        # env only; never written to voice.json
vikram voice say --enable "status report"
```

- The key is read from the variable `tts_api_key_env` names (default
  `ELEVENLABS_API_KEY`). It is never stored in the config file, never printed,
  and never put in an error message.
- `tts_voice_id` is empty by default on purpose: a baked-in voice id would bill
  an operator who never picked one. `tts_model` defaults to
  `eleven_turbo_v2_5`.
- The request is `POST https://api.elevenlabs.io/v1/text-to-speech/{voice_id}`
  with `output_format=pcm_16000`; the returned PCM is wrapped as WAV by the same
  writer the local voice uses, so the player stage is unchanged.
- **Any failure falls back to the local voice and says so once** — a missing key,
  an HTTP error, a network failure, or an empty response costs one note
  (`elevenlabs: … ; the local voice spoke instead`), never a silent turn and
  never a dead one. `voice status` reports `tts: ready: elevenlabs (voice …,
  model …)`, or `degraded:` naming the exact variable or field to fix.
- `--backend` on `say` is a one-run override; the saved `tts_backend` is not
  touched.

### 3.7 STT and TTS providers: local by default, presets, bring your own

Local is the default in both directions, and nothing about that path changed:
`stt_provider = local` transcribes with the built-in wavo model (`whisper-base.en`, Q8_0,
Vulkan), and `tts_provider = vikram` speaks with the Jarvis piper voice
(`en_GB-jarvis-medium`, espeak-ng only when the voice model is missing). Neither needs a key, a
network or a billing account.

A cloud provider is opt-in, one `config set` away, and **every cloud failure falls back to the
local engine with one note** — `google: HTTP 503 …; transcribing with the built-in wavo model` —
never a dead turn and never silence. A cancel is not a failure and ends the turn as a cancel.
Keys come from the environment variable named in `stt_api_key_env` / `tts_api_key_env`; the key
itself is never written to `voice.json`, never printed, and never put in an error message.

| Direction | Provider | Endpoint | Model | Voice | Key from |
|---|---|---|---|---|---|
| STT | `local` (default) | built-in wavo | `whisper-base.en` Q8_0 | — | none |
| STT | `webspeech` (optional) | one loopback page, then Google from the browser | Chrome/Edge speech engine (`stt_language`) | — | none |
| STT | `google` (preset) | `https://speech.googleapis.com/v1/speech:recognize` | Google auto-selects; `stt_model` overrides | — | `$GOOGLE_API_KEY` |
| STT | `openai` (preset) | `https://api.openai.com/v1/audio/transcriptions` | `whisper-1` | — | `$GOOGLE_API_KEY` until `stt_api_key_env` names another |
| STT | `custom` | your `stt_base_url` + `/audio/transcriptions` | your `stt_model` (required) | — | your `stt_api_key_env` |
| TTS | `vikram` (default) | built-in piper | — | `en_GB-jarvis-medium` | none |
| TTS | `elevenlabs` (preset) | `https://api.elevenlabs.io/v1/text-to-speech/<voice id>` | `eleven_turbo_v2_5` | your `tts_voice_id` (required) | `$ELEVENLABS_API_KEY` |
| TTS | `openai` (preset) | `https://api.openai.com/v1/audio/speech` | `tts-1` | `alloy` | `$ELEVENLABS_API_KEY` until `tts_api_key_env` names another |
| TTS | `custom` | your `tts_base_url` + `/audio/speech` | your `tts_model`, else `tts-1` | your `tts_voice_id`, else `alloy` | your `tts_api_key_env` |

A preset fills in the endpoint and the default model; an explicit `stt_base_url` / `stt_model` or
`tts_base_url` / `tts_model` / `tts_voice_id` overrides it. `custom` is the same
OpenAI-compatible shape against a base URL you supply, which is why "any provider" works: it is
the de-facto standard audio API. The four shapes, read off the published references
(2026-11-23):

- **STT, OpenAI-compatible** (`openai`, `custom`) — `POST {base}/audio/transcriptions`,
  `Authorization: Bearer <key>`, `multipart/form-data` with `file` (a 16 kHz mono WAV), `model`,
  and `language` in ISO-639-1, so `stt_language = en-US` is sent as `en`; the reply is
  `{"text": "…"}`.
- **STT, Google v1** (`google`) — `POST …/v1/speech:recognize?key=<key>` with
  `{"config":{"encoding":"LINEAR16","sampleRateHertz":16000,"languageCode":"en-US",
  "enableAutomaticPunctuation":true},"audio":{"content":"<base64 pcm16>"}}`; the reply is
  `results[0].alternatives[0].transcript`. Google prefers the `X-goog-api-key` header and
  documents `?key=` as the fallback that exposes the key in the URL, so the URL is never logged
  and every error has the key redacted.
- **TTS, OpenAI-compatible** (`openai`, `custom`) — `POST {base}/audio/speech`,
  `Authorization: Bearer <key>`, `{"model", "input", "voice", "response_format": "wav"}`; a WAV
  body is played as it arrives, a raw PCM body is wrapped the way §3.6 describes.
- **TTS, ElevenLabs** (`elevenlabs`) — unchanged from §3.6.

`voice status` prints both directions, and for a cloud provider the endpoint, model and the name
of the key variable — never the key:

```
tts backend: vikram (available: vikram | elevenlabs | openai | custom)
stt backend: local (available: local | webspeech | google | openai | custom)
webspeech: available — free, keyless, uses your browser's Chrome speech engine; needs Chrome or Edge with Google speech enabled, and the audio goes to Google from the browser (local never leaves the machine)
stt backend: google (available: local | webspeech | google | openai | custom) — https://speech.googleapis.com/v1/speech:recognize, model auto, language en-US, key from $GOOGLE_API_KEY
```

`voice transcribe --backend google` is the one-run override for transcription; `voice say
--backend openai` is the same for speech. Neither writes to `voice.json`.

### 3.8 Optional: `webspeech`, free keyless STT through your browser

`stt_provider = webspeech` transcribes with the speech engine Chrome and Edge already ship. It
needs **no key, no account and no model download**, and it is opt-in — `local` stays the default:

```
vikram voice listen --backend webspeech
vikram voice transcribe clip.wav --backend webspeech
vikram voice config set stt_provider webspeech      # keep it
```

Why there is a page involved: `webkitSpeechRecognition` only exists inside a document. So the
recognizer binds a single-use listener on `127.0.0.1:<random port>` with a random token, serves
one inline page (no external asset), opens it with `xdg-open`, and waits up to
`webspeech_timeout_ms` (60 000 by default) for the page to POST `{"text": "…"}` to
`/result?token=…`. The page starts recognition with `stt_language`, shows "listening", and posts
the words when it stops. The listener is loopback only, accepts exactly one POST, refuses a wrong
or missing token, and closes on success, timeout or cancel. The token is never logged, and a
second POST cannot replace the first.

What it needs:

- a **Chrome-family browser** with Google's speech engine enabled: Chrome, Edge or Brave. Plain
  Chromium builds usually ship without the API key and fail with `network` or `not-allowed`;
  Firefox has no recognition API at all.
- `xdg-open` and a desktop session: that is how the page is opened.
- a microphone the browser can use. The browser asks for permission the first time.

Where the audio goes: **the page sends it to Google's speech service**, the same endpoint Chrome
dictation uses. That is why it is free and keyless, and it is also a third party. Vikram itself
uploads nothing, and `local` never leaves the machine. If that trade is not acceptable, use
`local`.

`webspeech` ignores the local take, because the page records the microphone: `voice listen` opens
no audio device of its own (a machine without a microphone can still dictate through the browser),
and `voice transcribe` reads the file only to time the run. Every failure — no `xdg-open`, no
browser, a browser-side speech error, an empty transcript, the timeout — is one note and the
built-in wavo model transcribes instead. For `listen`, the local take is recorded at that point,
so the fallback is real audio rather than silence. A cancel is not a failure and ends the turn.

`voice status` names it either way, selected or not:

```
stt backend: local (available: local | webspeech | google | openai | custom)
webspeech: available — free, keyless, uses your browser's Chrome speech engine; needs Chrome or Edge with Google speech enabled, and the audio goes to Google from the browser (local never leaves the machine)
```

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
