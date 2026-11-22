# Voice plan — from text runtime to voice-first

Status: 2026-11-21. Scope: how Vikram gets ears and a voice on Linux, in what order,
what proves each phase, and what we honestly cannot promise.

This plan is subordinate to `docs/jarvis-class.md`. The film JARVIS is concept only;
"voice-first" here means a real, working voice loop with honest limits, not a
hologram claim.

## 0. What exists today

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
| wavo 0.6 (pure Rust) | Local, wgpu/Vulkan, GGUF weights | **Chosen engine.** No C/C++, no ONNX runtime; ~85 MB `whisper-base.en-Q8_0.gguf` pulled once into the hub cache. Accuracy on our test clip: exact, versus whisper.cpp base mishearing the name. |
| Voxtype (if installed) | External CLI fast path | Optional accelerator, never required — Vikram works without it. |
| `whisper-cli` binary | Same whisper.cpp engine, shelled out | Last-resort helper when both built-in paths fail. |
| Cloud ASR (provider APIs) | Network | Optional provider seam — but voice must work offline; never the only path. |

Model tiers (operator chooses, stored as config): `whisper-base.en` (default,
8-bit, ~85 MB), `whisper-small.en` (~270 MB) when accuracy matters more than
latency. GGML `.bin` files from whisper.cpp cannot be loaded by wavo; the old
tier constants are gone.

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

## 3. Crate layout

> **Status (2026-11-22, current): implemented and shipped.** `crates/vikram-voice`
> now holds `asr.rs`, `capture.rs`, `player.rs`, `session.rs`, `vad.rs`,
> `helpers.rs`, `cancel.rs`. Capture and playback are built-in `cpal` 0.18 with
> the `pipewire` feature; ASR is **wavo 0.6** (pure Rust, wgpu/Vulkan, GGUF
> models, downloaded once through the hub cache on the first utterance) with the
> Voxtype fast path and a `whisper-cli` helper behind it; TTS is `piper` when an
> ONNX voice is supplied, otherwise `espeak-ng`. The energy VAD now gates on a
> measured noise floor instead of a fixed level. `whisper-rs`, whisper.cpp and
> the `gpu`/vulkan feature were removed at `6a56df9`. Interfaces: CLI
> `vikram voice status|listen|say|chat`, TUI `/voice` (hands-free loop) and
> `Ctrl+G` (one dictation). The layout below is the original plan, kept for
> history.

New crate `crates/vikram-voice`, depending only on `vikram-contracts` for shared
types (audio frames stay `Vec<i16>`/`Vec<f32>` primitives; no new contract types
until a consumer needs them — same rule that deferred WASM prompts).

```
crates/vikram-voice/src/
  lib.rs        — public API surface only
  capture.rs    — mic → frames (cpal or parecord helper)
  vad.rs        — energy gate (phase 1), silero hook (phase 2)
  wake.rs       — push-to-talk (phase 1), openwakeword seam (phase 3)
  asr.rs        — whisper.cpp binding + whisper-cli helper fallback
  tts.rs        — piper helper + espeak-ng fallback
  player.rs     — speaker out (cpal or paplay helper)
  session.rs    — voice loop state machine: listen → transcribe → send → speak
  config.rs     — models dir, voice profile, thresholds (durable)
```

Public API (shape, not final signatures):

```rust
VoiceSession::start(config, runtime_handle) -> Result<VoiceSession>
VoiceSession::listen_once() -> Result<Option<String>>   // None = VAD timeout/silence
VoiceSession::speak(text) -> Result<()>
VoiceSession::status() -> VoiceStatus                   // each stage: Ready/Missing/Disabled
```

`listen_once` returning `Option` (not `Result<String>`) is deliberate: silence
is a normal outcome, not an error.

Permissions: `voice.listen` and `voice.speak` are tool-permission strings in
the existing deny-first system; the voice session refuses to open the mic or
the speaker without them.

## 4. Phases and acceptance criteria

### Phase 1 — push-to-talk loop (the honest MVP)

- `vikram voice` command: press key → record → VAD-trim → ASR → send → TTS reply.
- Deps: `cpal` + `whisper-rs` + piper/espeak helpers. If `whisper-rs` fails to
  build on the target, fall back to `whisper-cli` helper — either path counts.
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

- **Latency.** Whisper `base` on CPU ≈ 1–3 s per utterance; `tiny` ≈ 0.5–1 s.
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
