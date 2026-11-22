# Voice upgrade roadmap — from "it works" to "it feels human"

Status: 2026-11-22, worker V0, read-only review. HEAD `f8876e6`.
Scope: everything missing from Vikram's voice path, small and large, ranked into waves.
Subordinate to `docs/voice-plan.md` (stage choices) and `docs/jarvis-class.md` (why voice-first).

**Tree caveat.** The working tree was being edited by parallel workers while this was written.
`crates/vikram-cli/src/voice.rs` grew 365 → 931 lines at 19:20 (uncommitted: `voice config
show|path|set`, per-run overrides). `crates/vikram-voice/**` is unmodified vs HEAD. Everything
below is anchored to line numbers I read, not to the commit message.

**Evidence labels.** `[src]` = I read this code or these repo docs in this session. `[know]` =
outside knowledge, not verified against the crate in this repo. `[est]` = my estimate, no
measurement taken (I could not run `cargo` — other agents hold the target lock).

---

## 1. What exists today

| Stage | Where | What it actually does | Honest limit |
|---|---|---|---|
| Capture | `capture.rs:74-171` | cpal 0.18 (`pipewire` feature, `Cargo.toml:14`), default input device, any sample format, mix→mono, linear resample to 16 kHz at the boundary | Opens a stream **per utterance**, records for the **whole** `max_record_ms`, then stops |
| Capture fallback | `session.rs:90-123` | `parecord`/`arecord` via `timeout`, raw s16le | Only when cpal sees no device (`capture.rs:174-186`) |
| VAD | `vad.rs:27-102` | 10 ms RMS frames, noise floor = 10th percentile, gate = `floor × ratio` vs absolute, `min_speech_ms` 250, hangover 200 | Energy only; the module's own header admits it cannot separate speech from loud non-steady noise (`vad.rs:1-6`) |
| ASR | `asr.rs:200-216` | wavo 0.6, GGUF via hub, `transcribe_with(&audio, &options)` on the **complete** buffer | Batch only. No partials, no streaming, no confidence, no word timestamps used |
| ASR fast path | `asr.rs:254-296` | `voxtype` CLI if already on PATH, stdout last-paragraph parse | Whole-file WAV; another process spawn per utterance |
| ASR fallback | `session.rs:143-190` | `whisper-cli` helper, temp WAV, `-otxt` | Needs the same model file; no gain over wavo |
| TTS | `session.rs:208-229` | `piper` when `tts_voice` is set and piper is on PATH, else `espeak-ng -w` | Whole WAV synthesized before a single sample plays; espeak-ng is the default on a fresh machine (`helpers.rs:69-82` marks it degraded) |
| Playback | `player.rs:78-175` | hound decode (16-bit only), resample, cpal output, cancel within 50 ms poll | Cannot duck or resume; only abort |
| Loop | `session.rs:333-391` | `listen_once` = record-all → VAD-trim → ASR; `speak` = synth-all → play | Serial. Nothing overlaps. No turn-taking |
| Cancel | `cancel.rs:1-36`, `voice.rs:94-145` | Atomic token; SIGINT during a turn cancels instead of killing | Cancellation, not interruption — it ends the turn, it does not yield to the operator |
| CLI | `voice.rs` | `status`/`listen`/`say`/`chat` (+ WIP `config show|path|set`, `--language/--input-device/--threshold/...`) | `chat` is one utterance per process |
| TUI hands-free | `terminal.rs:447-506` | `/voice` spawns a worker that owns mic+model for the whole mode | **Never listens while speaking** — deliberate, per comment `terminal.rs:466-468` |
| TUI dictation | `terminal.rs:585-636` | `Ctrl+G` → transcript into the editor, never auto-sends | One utterance, cap 8 s |
| Status bar | `panels.rs:470-477` | `◉ voice` while the mode is on | Says the mic is open; says nothing about phase, level, or captions |

### 1.1 Known weak spots, with the line that proves each

| # | Weak spot | Evidence | Consequence |
|---|---|---|---|
| W1 | **No early stop.** Capture always runs to `max_record_ms` (default 8 000, `lib.rs:115`); VAD runs afterwards | `capture.rs:145-167` loop exits only on deadline; `session.rs:350` VAD after the fact | Say "hello" and wait 8 s. This is the single worst thing about the current voice |
| W2 | Energy VAD, not model VAD | `vad.rs:1-6` | A door, music, or a keyboard burst can open the gate; a soft-spoken start can be missed |
| W3 | ASR is batch-only | `asr.rs:207-209` | No partial caption, no early endpointing from text, no "you" feedback while speaking |
| W4 | No barge-in, no AEC | no AEC dependency in `crates/vikram-voice/Cargo.toml`; `terminal.rs:466-468` | You cannot interrupt the assistant. You wait, or you press Ctrl+C |
| W5 | espeak-ng fallback | `session.rs:219-222`, `helpers.rs:74-77` | Fresh machine sounds robotic; no rate/pitch control at all |
| W6 | No wake word | no `wake.rs`; `voice-plan.md:54-63` states no reliable Rust engine | Every activation is a keypress or a slash command |
| W7 | No speaker identity | no embedding code anywhere in the crate | Cannot tell the owner from a colleague; no enrolment, no per-speaker profile |
| W8 | No prosody/emotion | `HelperSynth` passes text only (`session.rs:217`) | One flat delivery; no `--length_scale`, no emphasis, no pauses at punctuation |
| W9 | No voice-scoped memory | turns land in the text session (`terminal.rs:519-533`) | "What did I just say?" / "say that again" have nothing to read |
| W10 | Captions only at end | `terminal.rs:522` pushes the notice after the final transcript | Zero feedback during capture or inference — looks like a hang |
| W11 | Device knobs stored, not honored | `input_device`/`output_device` exist (`lib.rs:93-95`, `voice.rs:141-148`) but capture/playback call `default_input_device()`/`default_output_device()` (`capture.rs:93`, `player.rs:89-91`) | `voice config set input_device ...` silently does nothing |
| W12 | Latent config-path doubling | `lib.rs:159-170` and `asr.rs:25-37` do `base.join(".vikram")`, but `vikram_home()` already returns `VIKRAM_HOME` verbatim (`vikram-runtime/src/config.rs:601-612`) | If `VIKRAM_HOME=~/.vikram`, the file becomes `~/.vikram/.vikram/voice.json`. Latent: unset on this machine |
| W13 | No AGC, no noise suppression | raw device samples go straight to VAD (`capture.rs:108-138`) | A quiet mic produces no transcript at all, and the operator's only fix is `floor_ratio` |
| W14 | 8 s hard cap | `lib.rs:115` | Dictating a paragraph is impossible |
| W15 | Transcript goes straight to `Submit` | `terminal.rs:528-533` | "Stop" or "cancel that" becomes a user turn asking the model to stop |
| W16 | No latency instrumentation | only `eprintln!` inside an ignored test (`player.rs:268`) | Nobody can say where the 3 s went |

---

## 2. Feature inventory

Cost: **S** ≤ 1 day · **M** 2-4 days · **L** > 1 week.

### 2.1 The table

| ID | Feature | What it is | Why a JARVIS-class assistant needs it | Cost | Risk | Concrete technique / crate |
|---|---|---|---|---|---|---|
| V1 | Early stop on silence | Capture loop consults the gate while recording and returns at endpoint, not at the cap | Removes the 8 s wait; nothing else matters until this is fixed | S | Cutting people off if the endpoint is naive — pair with V2 | Move the frame loop into `capture.rs`; feed `vad::detect_speech` incrementally; keep a 250 ms pre-roll |
| V2 | Adaptive endpointing | Silence window varies with utterance length, rate, and trailing filler | Fixed 200 ms hangover cuts mid-thought speech (`lib.rs:118`) | M | Too-long windows feel laggy; needs tuning data | New `endpoint.rs`: trailing-silence run + voiced-fraction + median inter-word gap. Values in §4.2 |
| V3 | Pre-roll ring buffer | Keep the 250 ms *before* the gate opens | Today the trim starts at the first voiced frame (`vad.rs:99`) → clipped first syllable | S | None material | 250 ms `VecDeque<i16>` in the capture loop, prepended to the segment |
| V4 | Streaming / partial captions | Re-transcribe a growing buffer every 300-500 ms and show the partial | Turns a 2 s black box into visible progress | M | CPU: N extra wavo passes per utterance; garbage partials flicker | **Fake streaming first**: re-run `WavoRecognizer` on the buffer so far, debounce 400 ms, only repaint on ≥ 2 word change. Real streaming = `sherpa-onnx` streaming zipformer [know] |
| V5 | Barge-in (tier 0, no AEC) | While speaking, watch the mic; on sustained energy above the playback-bleed threshold, duck then stop TTS | You can cut the assistant off, which is the whole point of a conversation | M | Speaker bleed self-triggers; laptop speakers are the worst case | Duck TTS to 20 % then abort; needs `Player` to grow a gain/pause verb (`player.rs` has abort only) |
| V6 | Barge-in (tier 1, AEC) | True echo cancellation so the mic ignores the assistant's own voice | Tier 0 is unusable on open speakers without it | L | C++ toolchain, cmake, per-platform build breakage | `webrtc-audio-processing` (AEC3 bindings) [know]; the player must feed the render reference signal |
| V7 | Noise suppression | Remove stationary noise before VAD/ASR | A fan or a café currently raises the gate and eats the utterance | M | Adds latency and can muffle soft consonants | `nnnoiseless` (RNNoise port, pure Rust) [know]; ~1-2 % of one core [est] |
| V8 | Automatic gain control | Normalize quiet/loud mics to a target RMS | A quiet mic produces *no* transcript today; the operator's only lever is `floor_ratio` | S | Pumping on transients | 20 ms RMS, target -20 dBFS, max +20 dB gain, 200 ms attack / 500 ms release, in `capture.rs` |
| V9 | Wake word | "Hey Vikram" starts listening hands-free | The presence cue. Without it, voice is a command you type with your mouth | L | False accepts in an open room are worse than no wake word | `openWakeWord` ONNX via `ort`, or `sherpa-onnx` KWS [know]; **must** publish a measured false-accept rate (`voice-plan.md:212`) |
| V10 | Three interaction modes | Push-to-talk hold, toggle, hands-free | Different rooms, different tasks; one mode cannot serve all | S | Mode confusion; the status bar must always say which | Config `interaction_mode`; see §4.1 |
| V11 | Speaker identification / enrolment | Embedding of the operator's voice, compared per utterance | Stops the assistant obeying a nearby colleague; enables per-speaker voice profiles | L | Enrolment UX, model size, false rejects locking the owner out | ECAPA-TDNN ONNX via `ort`, or `sherpa-onnx` speaker embedding [know]. Enrolment = 3 × 5 s clips → centroid |
| V12 | VAD tuning surface | A calibration command that measures the room and writes the gate | Hand-tuning `floor_ratio` is guesswork today | S | Over-fitting to one room | `vikram voice calibrate` → 3 s of room tone → writes `rms_threshold`/`floor_ratio`/AGC target |
| V13 | Multi-language + auto-detect | Per-utterance language, or a pinned one | Auto-detect already exists (`asr.rs:205-206`, `asr_language: None`); what is missing is a *voice* per language and a visible indicator | M | Whisper mis-detects short utterances and switches mid-sentence | `wavo` language detect (already wired) + piper voice per language + `asr_language` pin per session |
| V14 | TTS quality | Piper voices, rate, volume, sentence pauses | espeak-ng reads like a 1995 screen reader (`session.rs:219-222`) | M | Voice files are 50-100 MB downloads; licence per voice | piper `--length_scale` (rate), `--noise_scale`/`--noise_w` (expressiveness), `--output-raw` for streaming [know]. Ship a voice picker in `voice config` |
| V15 | Interrupt words | "stop", "wait", "cancel that", "never mind" handled locally, never sent to the model | Saying "stop" must stop, not start a turn | S | False positives on "stop the build" | Closed-set matcher on the transcript *before* `Submit` (`terminal.rs:528`); require the whole utterance to be the word |
| V16 | Spoken approvals | "yes / allow once / always / no" answers a pending approval, with read-back | Risky tool calls are keyboard-gated today; hands-free dies at the first approval | M | Misrecognition approving a destructive verb is the worst failure in the product | Hook `UiCommand::AnswerApproval { approval, allow, persist }` (`app.rs:471-475`, answered at `terminal.rs:967`); **two-step**: speak the action, require an explicit "confirm" |
| V17 | Voice-scoped memory | Last N spoken turns, "say that again", "what did I just say" | Voice conversations need voice recall, not just session recall | M | Duplicating the session transcript | Ring buffer of `(transcript, reply, ts)` in the voice worker; answers served locally without a model call |
| V18 | Live captions in the TUI | A caption region that shows partial → final, plus the reply being spoken | Accessibility (deaf/HoH) *and* trust: you see what it heard | M | Layout churn; ratatui redraw cost | New `VoicePhase` events → `VoiceEvent::Partial/Final/Speaking`; render in the composer area or a one-line strip above the status bar |
| V19 | Device hot-plug | Detect a USB headset appearing/disappearing, switch, or say so | Laptop + dock + headset is the normal desk; today the device is resolved once | M | cpal device enumeration is per-host and platform-specific | Poll `cpal` device list every 2 s while idle; on stream error, rebuild once, else report |
| V20 | Latency budget + instrumentation | Per-stage ms, logged and shown in `voice status` | You cannot tune what you cannot see | S | None | `Instant` deltas at 6 points; `--timings` flag; one line per turn |
| V21 | Offline-only default | Nothing leaves the machine unless the operator opts in | The current path already is local (`Cargo.toml` has no network ASR/TTS) — the risk is *regressing* it | S | Someone "helpfully" adds a cloud fallback | Keep cloud as a provider seam behind an explicit `voice.network_asr = false` default; assert it in a test |
| V22 | Accessibility: captions + non-audio path | Everything spoken is also written; every caption is copyable | Deaf/HoH operators, and noisy rooms where audio is useless | S | None | Captions (V18) + `voice say --no-audio` already implied by the text-first runtime |
| V23 | CPU/GPU cost discipline | A documented budget per stage, enforced in a test | Intel UHD laptop: the assistant must not pin a core while idle | S | Silent regressions | Idle ≤ 3 % of one core in hands-free; per-utterance ASR ≤ 1 core-seconds per audio-second [est] |
| V24 | Thinking feedback | Ack immediately, "still working" only after N s | 1-3 s of ASR + a slow model turn with no sound reads as a crash | S | Chatter | §4.3 timeline; earcon + TUI `◐` (`TurnState::Streaming`, `terminal.rs:646`) |

### 2.2 Latency budget — target ms per stage

"Now" is what the code implies, not a measurement. "Target" is what a reviewer should hold the
implementation to on this laptop.

| Stage | Now | Target (p50) | How |
|---|---|---|---|
| Stream open | 50-150 ms per utterance (`capture.rs:93-102`) | ≤ 10 ms | Keep one stream open for the session lifetime |
| Endpoint decision | after the full 8 000 ms cap (W1) | 400-1 200 ms after speech ends (§4.2) | V1+V2 |
| ASR (3 s utterance, base.en) | 1-3 s per `voice-plan.md:192` | ≤ 800 ms | Warm model (already lazy-loaded, `asr.rs:177-198`), avoid re-load, consider `tiny.en` for the live path |
| TTS to first audio | whole file first (`session.rs:209-218`) | ≤ 300 ms | piper `--output-raw` piped into the player |
| Playback start | immediate after synth | ≤ 100 ms | cpal stream reuse |
| **Perceived turn** | **~2-9 s** | **≤ 2.5 s** | The sum above, plus V24 |

---

## 3. Priority waves

### P0 — make the current voice trustworthy

| ID | Item | Files | Acceptance test a reviewer can run | New dep |
|---|---|---|---|---|
| P0.1 | Early stop on silence (V1) | `capture.rs`, `session.rs`, `vad.rs` | `cargo test -p vikram-voice early_stop`; then `time vikram voice listen --enable` and say one word → returns in < 1.5 s, not 8 s | none |
| P0.2 | Adaptive endpointing (V2) | new `endpoint.rs`, `session.rs`, `lib.rs` (`VoiceConfig`) | Unit tests over three fixtures: short command ends at ~400 ms, dictation at ~900-1 200 ms, a sentence split by a 600 ms cough stays one turn | none |
| P0.3 | Pre-roll 250 ms (V3) | `capture.rs` | Fixture whose first 30 ms is below the gate still transcribes the first word (compare against the current trim at `vad.rs:99`) | none |
| P0.4 | Honor `input_device`/`output_device` (W11) | `capture.rs`, `player.rs` | `voice config set input_device <name>` then `voice listen --enable` uses that device; a wrong name fails with the available list | none |
| P0.5 | `voice devices` listing | `voice.rs` | Prints input/output devices with the default marked; exit 0 with no mic and a reason | none |
| P0.6 | AGC (V8) | `capture.rs` | A -45 dBFS fixture reaches the recognizer at ≥ -25 dBFS; a -6 dBFS fixture is not clipped | none |
| P0.7 | Honest miss (no hallucinated transcript) | `session.rs`, `voice.rs` | Speech > 600 ms that yields an empty/short transcript says "I did not catch that" instead of submitting; test asserts no `Submit` on empty | none |
| P0.8 | Instant listening earcon | `player.rs`, new in-memory WAV builder (today `write_wav` is disk-only, `helpers.rs:219-239`) | Press Ctrl+G → audible blip < 100 ms before any capture; `cargo test -p vikram-voice earcon` | none |
| P0.9 | Fix the config-path doubling (W12) | `lib.rs:159-170`, `asr.rs:25-37` | `VIKRAM_HOME=/tmp/vh cargo test -p vikram-voice config_path` asserts `/tmp/vh/voice.json`, not `/tmp/vh/.vikram/voice.json` | none |
| P0.10 | Per-stage latency log (V20) | `session.rs`, `voice.rs` | `vikram voice chat --enable --timings` prints `open/endpoint/asr/tts/play` ms for one turn | none |
| P0.11 | Raise the dictation cap (W14) | `lib.rs:115`, `voice.rs` | `--max-record-ms 60000` accepted; a 20 s dictation transcribes in full | none |

Why P0 stops here: after P0.1-P0.3 the loop is *fast and does not cut you off*. Everything below
assumes that.

### P1 — make it feel alive

| ID | Item | Files | Acceptance test | New dep |
|---|---|---|---|---|
| P1.1 | Partial captions (V4) | `asr.rs`, `session.rs`, `terminal.rs` | Speak a 5 s sentence: at least 3 partial updates appear before the final, each monotonically longer | none (fake streaming) |
| P1.2 | Live caption region (V18) | `terminal.rs`, `vikram-tui/src/widgets/`, `app.rs` | During `/voice`, the partial text is visible in the TUI within 700 ms of speech starting; the final replaces it in place | none |
| P1.3 | Barge-in tier 0 (V5) | `player.rs`, `session.rs`, `terminal.rs` | While a 10 s reply plays, say "wait" at normal volume from 40 cm → audio ducks within 300 ms and stops within 600 ms | none |
| P1.4 | Interrupt words (V15) | `terminal.rs` (before `Submit`), `session.rs` | Say "stop" during a turn → turn cancelled, no user message in the transcript; "stop the build" still submits | none |
| P1.5 | Thinking feedback (V24) | `terminal.rs`, `voice.rs`, `panels.rs` | Transcript final → ack ≤ 150 ms; if the turn exceeds 6 s exactly one "still working" cue; never 6 s of silence | none |
| P1.6 | Spoken approvals (V16) | `terminal.rs` (`AnswerApproval` at `:967`), `app.rs` | With a deny-by-default tool pending, "allow once" + read-back + "confirm" answers it; a bare "yes" does not | none |
| P1.7 | Voice-scoped memory (V17) | `terminal.rs` voice worker | "What did I just say?" returns the previous transcript verbatim without a model call | none |
| P1.8 | Noise suppression (V7) | `capture.rs`, `Cargo.toml` | A speech fixture mixed with -20 dB fan noise transcribes with ≥ 90 % word match, vs the current failure | `nnnoiseless` [know] — small, pure Rust, adds ~1-2 % core [est] |
| P1.9 | Device hot-plug (V19) | `capture.rs`, `player.rs`, `terminal.rs` | Unplug the headset mid-session → notice in the status bar, one rebuild attempt, session survives | none |

### P2 — presence

| ID | Item | Files | Acceptance test | New dep |
|---|---|---|---|---|
| P2.1 | Wake word (V9) | new `wake.rs`, `session.rs`, `lib.rs` | Hands-free: "hey vikram" starts a turn; a published false-accept rate over a 30 min room recording (target < 1/hour, stated even if worse) | `ort` + openWakeWord ONNX, or `sherpa-onnx` [know] — verify build cost first |
| P2.2 | Speaker ID + enrolment (V11) | new `speaker.rs`, `lib.rs` | `voice enroll` records 3 clips; a second speaker's utterance is rejected with a stated reason, not silently obeyed | `ort` + ECAPA-TDNN ONNX [know] |
| P2.3 | Always-listening with a real VAD (W2) | `vad.rs`, `capture.rs` | Idle CPU ≤ 3 % of one core for 10 min; speech onset detected within 150 ms | Silero VAD ONNX via `ort` (~2 MB) [know]; `voice-plan.md:50-52` already names this path |
| P2.4 | Language auto-detect surfacing (V13) | `asr.rs`, `voice.rs` | Hindi/English mixed utterance: detected language shown; pinning `--language en` overrides and is remembered | none (detect already wired) |
| P2.5 | Barge-in tier 1 / AEC (V6) | `player.rs`, `capture.rs` | On open speakers at 60 % volume, the assistant's own voice never triggers barge-in | `webrtc-audio-processing` [know] — C++/cmake build, highest build risk in this doc |

### P3 — polish

| ID | Item | Files | Acceptance test | New dep |
|---|---|---|---|---|
| P3.1 | Piper voices + rate/volume (V14) | `session.rs`, `voice.rs` | `voice config set tts_voice <path>` and `--rate 0.9` change delivery audibly; a missing voice falls back with a stated reason | none (piper flags) |
| P3.2 | Streaming TTS | `session.rs`, `player.rs` | First audio ≤ 300 ms after synthesis starts, on a 1 200-byte reply | none (piper `--output-raw`) |
| P3.3 | Prosody: pauses at punctuation, no monotone | `session.rs` | A 3-sentence reply has an audible pause at each sentence end | none (split + concat) |
| P3.4 | Per-entity voice profiles | `lib.rs`, `vikram-store` | Two entities speak with different voices; the profile lives beside the text persona (`voice-plan.md:95-99`) | none |
| P3.5 | Backchannel filter (V15 extension) | `terminal.rs` | "mm-hmm" while the assistant speaks does not create a turn | none |

---

## 4. HUMAN TURN-TAKING

How people actually talk. This section is the difference between a voice *feature* and a voice
*interface*. Ranked at the end of §4.7.

### 4.1 Three modes, and what a mid-sentence pause does

| Mode | Binding | Config key | Mid-sentence pause behaviour |
|---|---|---|---|
| Push-to-talk, hold | `Ctrl+G` held (or a configurable `ptt_key`) | `interaction_mode = "hold"` | Nothing — the key is the endpoint. Release ends the turn. Best default for a shared room |
| Push-to-talk, toggle | `Ctrl+G` tap | `interaction_mode = "toggle"` | Endpointing applies (V2); a second tap force-ends. For long dictation |
| Hands-free | `/voice` | `interaction_mode = "hands_free"` | Endpointing applies, plus the 2.5 s resume window; a cough inside 2.5 s does not split the turn |

Config keys to add to `VoiceConfig` (`lib.rs:63-96`): `interaction_mode`, `ptt_key`,
`endpoint_short_ms`, `endpoint_long_ms`, `resume_window_ms`, `preroll_ms`, `barge_in`,
`interrupt_words`, `wake_word`, `agc`, `denoise`. All are already persistable — `voice config
set` exists (WIP `voice.rs:73-76`, `:550-575`) and reads keys off the struct, so a new field is one line plus
validation in `VoiceConfig::validate` (`lib.rs:128-155`).

Today: only `Ctrl+G` (one utterance, `terminal.rs:585-609`) and `/voice` (hands-free,
`terminal.rs:447-464`) exist. Hold-to-talk does not.

### 4.2 Adaptive endpointing

The current 200 ms hangover (`lib.rs:118`) is a *trim* constant, not a turn-taking policy. Replace
it with a decision, not a constant:

| Constant | Value | Lives in | Why |
|---|---|---|---|
| `endpoint_short_ms` | 400 | `VoiceConfig` + `voice.json` | Short commands ("lights off") should land fast |
| `endpoint_long_ms` | 1 200 | same | Dictation needs room to think mid-sentence |
| `resume_window_ms` | 2 500 | same | After an endpoint, keep the buffer warm; speech inside the window appends to the same turn |
| `preroll_ms` | 250 | same | V3 |
| `min_speech_ms` | 250 (unchanged, `lib.rs:119`) | same | A click is still not a word |
| `filler_tail` | `and, so, because, but, um, uh, like` | `endpoint.rs` const | A trailing filler means "I am not done" |

Decision (in `endpoint.rs`, ~120 lines, pure functions so it is testable without a mic):

```
threshold = endpoint_short_ms                       if utterance < 1200 ms
          = endpoint_long_ms                        if utterance >= 1200 ms
          = endpoint_long_ms + 400                  if last 3 words contain a filler tail
end turn when trailing_silence >= threshold
```

Two extra guards that matter more than the numbers: never end inside 300 ms of the *first* voiced
frame (people start with a breath), and never end while the AGC reports a rising envelope.

### 4.3 "Thinking" feedback — the timeline

| t | What happens | Where |
|---|---|---|
| 0 ms | Transcript final; **earcon** (40 ms blip) or a short spoken "got it" | `player.rs` + `terminal.rs` |
| 0 ms | TUI shows `◐` working (already exists: `TurnState::Streaming`, `terminal.rs:646`) and the transcript as a caption | `panels.rs` |
| 150 ms | If ASR is still running: caption says `transcribing…` | `terminal.rs` |
| > 6 s | One "still working" cue (earcon, not words — words get annoying) | `terminal.rs` |
| > 20 s | Spoken "this is taking a while" once | `terminal.rs` |
| never | 6 s of silence with no cue | — |

Hard rule: silence must never be ambiguous between "thinking" and "crashed". The status bar
already distinguishes `voice_mode` (`panels.rs:470-477`); it must also distinguish
`listening / transcribing / thinking / speaking`.

### 4.4 Thinking-user detection (pause vs finished)

| Signal | Computed over | Use |
|---|---|---|
| Trailing silence run | last voiced frame → now | The primary endpoint trigger |
| Speech rate proxy | voiced frames / total frames in the last 1.5 s | Fast talkers get a shorter window; slow, deliberate speech gets the long one |
| Median inter-word gap | last 3 gaps, from partial captions (needs V4) | A speaker whose gaps run 400 ms is not finished at 400 ms |
| Trailing filler | last 3 tokens of the partial | +400 ms |
| Resume window | 2 500 ms after an endpoint | A cough, a door, a sip of water must not split one sentence into two turns |

Before partial captions exist (P1.1), the first three signals are energy-only: voiced fraction
plus gap statistics from the frame envelope. That is enough for V2 and honest about its limits —
energy cannot tell "um" from a fan.

### 4.5 Barge-in and backchannels

| Tier | Mechanism | Requirement | Cost | Risk |
|---|---|---|---|---|
| 0 | Duck TTS on any voice energy above `gate × 1.5` for ≥ 120 ms; stop if it persists 300 ms | None — works on speakers, imperfectly | M | Self-trigger from speaker bleed; mitigate by requiring energy *and* a spectral change, and by documenting headphones |
| 1 | AEC3 with the TTS as the render reference | Player must emit frames, not just abort | L | Build toolchain (`webrtc-audio-processing`), highest risk item in this doc |
| 2 | Headphones | Nothing | S | Document "barge-in is reliable with headphones; speakers are best-effort" |

Interrupt words (local, closed set, checked before `Submit`): `stop`, `stop it`, `wait`, `cancel
that`, `never mind`, `quiet`. Match only when the whole utterance is the phrase.

Backchannels that must **not** become turns: `mm-hmm`, `mm`, `uh-huh`, `okay`, `ok`, `yeah`,
`right`, `yes`. Rule: transcript in the set **and** duration < 700 ms **and** arriving while the
assistant is speaking → discard silently. Without this, a listener who nods becomes a user who
keeps issuing one-word commands.

### 4.6 Smoothness a human notices

| Item | Current behaviour | Fix | Cost |
|---|---|---|---|
| Clipped first syllable | Trim starts at the first voiced frame (`vad.rs:99`) | 250 ms pre-roll (V3) | S |
| Dead air after the keypress | `listen` prints a line, then opens a stream (50-150 ms) | Keep the stream warm for the session; earcon at t=0 | S |
| Dropped device mid-turn | `VoiceError::NoDevice` ends the mode (`terminal.rs:493-496`) | One rebuild attempt, then a stated reason and the session survives | M |
| Hallucinated transcript | wavo returns whatever it heard; no confidence used (`asr.rs:214-215`) | "I did not catch that" when speech > 600 ms yields < 2 words; never submit an empty turn | S |
| Truncated long replies | Capped at 1 200 bytes at a sentence end (`voice.rs:275-294`) — correct, keep | — | — |

### 4.7 Ranking against the waves

| Order | Item | Wave | Why this order |
|---|---|---|---|
| 1 | Early stop + adaptive endpointing (§4.2, P0.1/P0.2) | P0 | The 8 s wait is the single largest "this is a machine" tell |
| 2 | Pre-roll + earcon + honest miss (§4.6, P0.3/P0.8/P0.7) | P0 | Sub-second, perceptible quality; costs days, not weeks |
| 3 | Thinking feedback + live captions (§4.3, §4.5 P1.2/P1.5) | P1 | Removes the "is it dead?" silence during ASR and the model turn |
| 4 | Barge-in tier 0 + interrupt words (§4.5, P1.3/P1.4) | P1 | Turns a monologue into a conversation |
| 5 | Resume window + thinking-user detection (§4.4, P1.1) | P1 | Stops splitting sentences; needs partials to be honest |
| 6 | Spoken approvals (§4.5/P1.6) | P1 | Unblocks hands-free past the first risky tool call |
| 7 | Backchannel filter (§4.5, P3.5) | P3 | Only matters once barge-in exists |

**The three that turn "voice that works" into "voice that feels human" fastest:**
1. **Early stop on silence** (P0.1) — one file, one day, removes the 8 s wait.
2. **Adaptive endpointing + pre-roll** (P0.2, P0.3) — stops cutting people off, stops clipping them.
3. **Thinking feedback + partial captions** (P1.1, P1.2, P1.5) — turns 3 s of silence into visible progress.

---

## 5. Do not do

| Tempting | Why it is wrong here |
|---|---|
| Cloud ASR/TTS as the default or the fallback | Breaks offline-first (`voice-plan.md:209`), sends the operator's room to a vendor. Keep it as a seam that is off by default |
| Porcupine wake word | Requires an access key — a secret in a single-user CLI, plus a vendor dependency (`voice-plan.md:61-63`) |
| Full AEC/beamforming before barge-in tier 0 | C++/cmake on an Intel UHD laptop, and it needs render-reference plumbing that does not exist. Duck first |
| Bundling model weights in the repo | Licence + size (`voice-plan.md:208`); the hub download already works (`asr.rs:154-175`) |
| Diarization / multi-speaker | Single user. Speaker *identification* is worth it; diarization is not |
| Emotion/prosody ML models | Cost >> value at this stage; piper's `length_scale` buys most of the perceived gain for free |
| CUDA/GPU-specific paths | Intel UHD: no CUDA. wavo already falls back on its own (`Cargo.toml:16-18`); do not hand-roll a GPU branch |
| Re-architecting wavo for true streaming | Its API is batch (`asr.rs:207-209`). Fake streaming by re-transcribing a growing buffer gets 80 % of the value for 5 % of the work |
| A second audio backend (rodio/GStreamer) beside cpal | Two backends means two sets of device bugs. cpal 0.18 + pipewire is already in (`Cargo.toml:14`) |
| Always-on listening with no visible indicator | A mic that is open without a status-bar glyph is a privacy bug, not a feature |
| Tuning VAD thresholds by hand in `voice.json` | Ship `voice calibrate` (V12) instead; hand-tuned constants rot per room |
| Speaking whole long replies | Already capped at 1 200 bytes (`voice.rs:275-294`). Keep the cap; do not "fix" it |
| Wake word before speaker ID | A wake word that obeys anyone in the room, combined with spoken approvals, is a security hole |

---

## 6. Unknowns I could not verify

1. **wavo streaming/partial API.** I read only `Model::transcribe_with(&audio, &options)`
   (`asr.rs:207-209`). Whether 0.6 exposes segments, timestamps, or confidence is unverified.
   `[src]` for the call shape, `[unknown]` for the API surface.
2. **wavo confidence.** V4/P0.7 assume none is available. If it is, the honest-miss rule gets much
   better; check before building the heuristic.
3. **Actual ASR latency on this laptop.** No benchmark was run (target lock held by other agents).
   The 1-3 s figure is `voice-plan.md:192`'s claim, not a measurement.
4. **Vulkan iGPU vs CPU for wavo here.** `Cargo.toml:16-18` says wgpu falls back by itself; whether
   the Intel iGPU beats the CPU for `whisper-base.en` is unknown.
5. **What TTS is actually installed.** I did not run `which piper espeak-ng`. If piper has no voice
   file, every reply today is espeak-ng (`helpers.rs:69-82`).
6. **`webrtc-audio-processing` build on this machine.** Needs cmake + a C++ toolchain; unverified,
   and it is the riskiest dependency in this document.
7. **`ort`, `sherpa-onnx`, `nnnoiseless` versions and licences.** Named from `[know]`; none were
   checked against crates.io in this session.
8. **Whether the TUI has room for a caption region.** I read `ui.rs:121` and `panels.rs:435-477`,
   not `layout.rs`; V18's placement is a guess.
9. **`libc` dependency declaration for `vikram-cli`.** `voice.rs:102-145` uses `libc`; I did not
   read `crates/vikram-cli/Cargo.toml` to confirm it is a declared dependency rather than
   transitive.
10. **The config-path doubling (W12) as an observed bug.** `VIKRAM_HOME` is unset on this machine,
    so the doubled path never materialised. It is a code-level mismatch between `lib.rs:159-170`
    and `vikram-runtime/src/config.rs:601-612`, not a reproduced failure.
11. **Whether `voice config set` (WIP) validates before saving.** Partially resolved: `voice config set` does call `VoiceConfig::validate` before saving
    (`voice.rs:533`, `:567`). Whether the write-back after a `--flag` override validates the same
    way (`voice.rs:681`, `:718`) I did not trace end to end.
12. **Endpoint numbers.** 400/900-1200/2500 ms are starting values from conversational-speech
    convention `[know]`, not measured on this operator. P0.2's acceptance test is what turns them
    into data.

---

## 7. The three I would do first, and why

| # | Item | Why first |
|---|---|---|
| 1 | **Early stop on silence** (P0.1) | Every utterance currently costs the full 8 s cap (`capture.rs:145-167`). One file, one day, and it is the difference between "voice works" and "voice is unusable". Nothing else can be judged fairly until it is gone |
| 2 | **Adaptive endpointing + 250 ms pre-roll** (P0.2, P0.3) | Early stop without a real endpoint policy just cuts people off mid-thought, and the current trim clips the first syllable (`vad.rs:99`). These two together are what make the loop feel like listening rather than recording |
| 3 | **Thinking feedback + partial captions** (P1.5, P1.1, P1.2) | After 1 and 2, the remaining dead time is ASR plus the model turn. A caption and one earcon turn that silence into progress — the cheapest perceived-latency win in the whole list, and it doubles as the accessibility path (V22) |

Deferred on purpose: barge-in (needs a `Player` verb and, for speakers, AEC), wake word (needs a
false-accept measurement before it is allowed to exist), and speaker ID (needs an enrolment UX).
Each is a wave of its own, and each is worse than useless if the base loop still takes 8 seconds.
