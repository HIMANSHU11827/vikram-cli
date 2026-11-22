//! Built-in microphone capture via cpal, with the helper path as a detected fallback.
//!
//! The device is opened at whatever rate and channel count it supports; samples are converted
//! to interleaved i16, mixed to mono, and linearly resampled to the config rate once, at this
//! boundary, as `docs/voice-plan.md` §2.1 requires. Recording stops as soon as speech has
//! started and the trailing silence passes the hangover, with `max_ms` as the hard cap; a cancel
//! lands within one poll interval.

use crate::cancel::CancelToken;
use crate::session::AudioSource;
use crate::vad::detect_speech;
use crate::{HelperSource, Result, VoiceConfig, VoiceError};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);

/// When a take is over. The gate is the VAD's own (`vad::detect_speech`, the same noise floor
/// and ratio the session applies afterwards), fed the samples as they arrive, so capture stops
/// when the speaker does instead of recording the rest of the cap. The knobs are the config's
/// existing gate fields: `hangover_ms` is how long a pause may last before the take ends, and
/// `min_speech_ms` keeps a cough from ending it.
#[derive(Clone, Copy)]
pub(crate) struct SilenceStop {
    absolute: f32,
    ratio: f32,
    min_speech_ms: u64,
    hangover_ms: u64,
}

impl SilenceStop {
    fn from_config(config: &VoiceConfig) -> Self {
        Self {
            absolute: config.rms_threshold,
            ratio: config.floor_ratio,
            min_speech_ms: config.min_speech_ms,
            hangover_ms: config.hangover_ms,
        }
    }

    /// How much of `pcm` to keep once the speaker has stopped: everything up to the end of the
    /// speech plus the hangover. Everything before it stays, so the frames ahead of the gate
    /// opening are the take's pre-roll and the first syllable is never clipped. `None` means
    /// speech has not started, or is still going.
    fn keep(&self, pcm: &[i16], sample_rate: u32) -> Option<usize> {
        let (_, end) = detect_speech(
            pcm,
            self.absolute,
            self.ratio,
            self.min_speech_ms,
            self.hangover_ms,
            sample_rate as u16,
        )?;
        (end < pcm.len()).then_some(end)
    }
}

/// The default input device's name, without opening a stream. `None` means cpal cannot see a
/// microphone on this machine.
pub fn probe_capture() -> Option<String> {
    let device = cpal::default_host().default_input_device()?;
    let name = device.description().ok()?.name().to_owned();
    let has_config = device.default_input_config().is_ok();
    has_config.then_some(name)
}

/// cpal's own name for a device, falling back to its `Display` form when the backend cannot
/// describe it.
pub(crate) fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|description| description.name().to_owned())
        .unwrap_or_else(|_| device.to_string())
}

/// The names in cpal's order, the default first, duplicates collapsed.
pub(crate) fn ordered_names(devices: &[cpal::Device], default: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for device in devices {
        let name = device_name(device);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    default_first(names, default)
}

/// Move `default` to the front, keeping cpal's order for everything else.
fn default_first(mut names: Vec<String>, default: Option<&str>) -> Vec<String> {
    if let Some(default) = default {
        match names.iter().position(|name| name == default) {
            Some(0) => {}
            Some(pos) => {
                let name = names.remove(pos);
                names.insert(0, name);
            }
            None => names.insert(0, default.to_owned()),
        }
    }
    names
}

/// The index of `requested` in `available`: an exact match wins, then a case-insensitive one.
/// Anything else is an error naming what was asked for and what the machine actually has.
pub(crate) fn match_device(
    kind: &str,
    requested: &str,
    available: &[String],
) -> std::result::Result<usize, String> {
    if let Some(index) = available.iter().position(|name| name == requested) {
        return Ok(index);
    }
    if let Some(index) = available
        .iter()
        .position(|name| name.eq_ignore_ascii_case(requested))
    {
        return Ok(index);
    }
    let known = if available.is_empty() {
        "none".to_owned()
    } else {
        available
            .iter()
            .map(|name| format!("{name:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    Err(format!(
        "no {kind} device named {requested:?}; available {kind} devices: {known}"
    ))
}

/// Every input device cpal can see, the default first.
pub fn input_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let default = host
        .default_input_device()
        .map(|device| device_name(&device));
    let devices: Vec<cpal::Device> = host
        .input_devices()
        .map_err(|err| VoiceError::NoDevice(format!("enumerate input devices: {err}")))?
        .collect();
    Ok(ordered_names(&devices, default.as_deref()))
}

/// The configured input device, or the host default when none is configured.
fn open_input(configured: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let Some(requested) = configured.map(str::trim).filter(|name| !name.is_empty()) else {
        return host
            .default_input_device()
            .ok_or_else(|| VoiceError::NoDevice("no default input device".into()));
    };
    let devices: Vec<cpal::Device> = host
        .input_devices()
        .map_err(|err| VoiceError::NoDevice(format!("enumerate input devices: {err}")))?
        .collect();
    let names: Vec<String> = devices.iter().map(device_name).collect();
    let index = match_device("input", requested, &names).map_err(VoiceError::NoDevice)?;
    Ok(devices
        .into_iter()
        .nth(index)
        .expect("index came from the same list"))
}

/// Interleaved channels averaged down to one channel.
pub(crate) fn mix_to_mono(samples: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|s| i32::from(*s)).sum();
            (sum / frame.len() as i32) as i16
        })
        .collect()
}

/// Linear resample between integer rates. Passing equal rates returns the input unchanged.
pub(crate) fn resample(pcm: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || pcm.is_empty() {
        return pcm.to_vec();
    }
    let step = f64::from(from_rate) / f64::from(to_rate);
    let out_len = ((pcm.len() as f64) / step).floor() as usize;
    (0..out_len)
        .map(|i| {
            let at = i as f64 * step;
            let left = at.floor() as usize;
            let right = (left + 1).min(pcm.len() - 1);
            let frac = at - at.floor();
            let delta = i32::from(pcm[right]) - i32::from(pcm[left]);
            (i32::from(pcm[left]) as f64 + f64::from(delta) * frac).round() as i16
        })
        .collect()
}

fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

fn u16_to_i16(samples: &[u16]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| i32::from(*s).wrapping_sub(32_768) as i16)
        .collect()
}

/// What the capture thread tells the consumer: audio as it arrives, then the trimmed take, or
/// why the take could not be made. Private to this module; the consumer sees a callback.
enum LiveMsg {
    Chunk(Vec<i16>),
    Done(Vec<i16>),
    Failed(String),
}

/// Build and start an input stream that delivers mono i16 chunks at the device's own rate.
/// Returns the running stream and the channel a stream error arrives on.
fn start_mono_stream(
    device: &cpal::Device,
    format: cpal::SampleFormat,
    stream_config: cpal::StreamConfig,
    channels: usize,
    tx: mpsc::Sender<Vec<i16>>,
) -> Result<(cpal::Stream, mpsc::Receiver<String>)> {
    let (err_tx, err_rx) = mpsc::channel::<String>();
    let err_fn = move |err: cpal::Error| {
        let _ = err_tx.send(err.to_string());
    };
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            stream_config,
            move |data: &[f32], _| {
                let _ = tx.send(mix_to_mono(&f32_to_i16(data), channels));
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            stream_config,
            move |data: &[i16], _| {
                let _ = tx.send(mix_to_mono(data, channels));
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            stream_config,
            move |data: &[u16], _| {
                let _ = tx.send(mix_to_mono(&u16_to_i16(data), channels));
            },
            err_fn,
            None,
        ),
        other => {
            return Err(VoiceError::NoDevice(format!(
                "device sample format {other:?} is not supported"
            )));
        }
    };
    let stream = stream.map_err(|err| VoiceError::NoDevice(format!("open input stream: {err}")))?;
    stream
        .play()
        .map_err(|err| VoiceError::NoDevice(format!("start input stream: {err}")))?;
    Ok((stream, err_rx))
}

/// The capture thread's body: drain the device, hand each chunk to the consumer as it arrives,
/// and stop on the same end-of-speech rule as a plain take. The returned buffer is the trimmed
/// take at config rate, identical to what `record_cancellable` produces for the same audio.
fn stream_capture(
    device: Option<&str>,
    config_rate: u16,
    stop: &SilenceStop,
    max_ms: u64,
    cancel: &CancelToken,
    tx: &mpsc::Sender<LiveMsg>,
) -> Result<Vec<i16>> {
    cancel.check()?;
    let device = open_input(device)?;
    let supported = device
        .default_input_config()
        .map_err(|err| VoiceError::NoDevice(format!("input device has no config: {err}")))?;
    let rate = supported.sample_rate();
    let channels = supported.channels().max(1) as usize;
    let format = supported.sample_format();
    let stream_config: cpal::StreamConfig = supported.into();
    let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<i16>>();
    let (stream, err_rx) = start_mono_stream(&device, format, stream_config, channels, chunk_tx)?;

    let deadline = Instant::now() + Duration::from_millis(max_ms.max(1));
    let cap = (max_ms.max(1) * u64::from(rate) / 1_000) as usize;
    let mut pcm = Vec::new();
    let outcome = loop {
        if cancel.is_cancelled() {
            break Err(VoiceError::Cancelled);
        }
        let now = Instant::now();
        if now >= deadline {
            break Ok(());
        }
        let wait = (deadline - now).min(POLL);
        match chunk_rx.recv_timeout(wait) {
            Ok(chunk) => {
                pcm.extend_from_slice(&chunk);
                // The tap gets the audio before the trim, so the last chunk may carry the
                // hangover: a caption may trail the speaker by that much and never leads.
                let _ = tx.send(LiveMsg::Chunk(resample(
                    &chunk,
                    rate,
                    u32::from(config_rate),
                )));
                if let Some(keep) = stop.keep(&pcm, rate) {
                    pcm.truncate(keep);
                    break Ok(());
                }
                if pcm.len() >= cap {
                    pcm.truncate(cap);
                    break Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Ok(err) = err_rx.try_recv() {
                    break Err(VoiceError::NoDevice(format!("input stream stopped: {err}")));
                }
                break Ok(());
            }
        }
    };
    drop(stream);
    outcome?;
    Ok(resample(&pcm, rate, u32::from(config_rate)))
}

/// One capture from the configured input device (the host default when unset), converted to
/// config-rate mono i16.
pub struct CpalSource {
    sample_rate: u16,
    device: Option<String>,
    stop: SilenceStop,
}

impl CpalSource {
    pub fn start(config: &VoiceConfig) -> Self {
        Self {
            sample_rate: config.sample_rate,
            device: config.input_device.clone(),
            stop: SilenceStop::from_config(config),
        }
    }
}

impl AudioSource for CpalSource {
    fn record(&mut self, max_ms: u64) -> Result<Vec<i16>> {
        self.record_cancellable(max_ms, &CancelToken::new())
    }

    fn record_cancellable(&mut self, max_ms: u64, cancel: &CancelToken) -> Result<Vec<i16>> {
        cancel.check()?;
        let device = open_input(self.device.as_deref())?;
        let supported = device
            .default_input_config()
            .map_err(|err| VoiceError::NoDevice(format!("input device has no config: {err}")))?;
        let rate = supported.sample_rate();
        let channels = supported.channels().max(1) as usize;
        let format = supported.sample_format();
        let stream_config: cpal::StreamConfig = supported.into();
        let (tx, rx) = mpsc::channel::<Vec<i16>>();
        let (stream, err_rx) = start_mono_stream(&device, format, stream_config, channels, tx)?;

        let deadline = Instant::now() + Duration::from_millis(max_ms.max(1));
        let cap = (max_ms.max(1) * u64::from(rate) / 1_000) as usize;
        let mut pcm = Vec::new();
        loop {
            if cancel.is_cancelled() {
                drop(stream);
                return Err(VoiceError::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = (deadline - now).min(POLL);
            match rx.recv_timeout(wait) {
                Ok(chunk) => {
                    pcm.extend_from_slice(&chunk);
                    if let Some(keep) = self.stop.keep(&pcm, rate) {
                        pcm.truncate(keep);
                        break;
                    }
                    if pcm.len() >= cap {
                        pcm.truncate(cap);
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Ok(err) = err_rx.try_recv() {
                        return Err(VoiceError::NoDevice(format!("input stream stopped: {err}")));
                    }
                    break;
                }
            }
        }
        drop(stream);
        Ok(resample(&pcm, rate, u32::from(self.sample_rate)))
    }

    /// The live path: the cpal stream is owned by a capture thread, so the caller's thread is
    /// free to transcribe the audio that has already arrived while the microphone keeps running.
    /// Nothing is dropped between the two: the thread sends every chunk as it comes, and the take
    /// it finally returns is trimmed by the same `SilenceStop` a plain recording uses.
    fn record_streaming(
        &mut self,
        max_ms: u64,
        cancel: &CancelToken,
        on_chunk: &mut dyn FnMut(&[i16]),
    ) -> Result<Vec<i16>> {
        cancel.check()?;
        let (tx, rx) = mpsc::channel::<LiveMsg>();
        let device = self.device.clone();
        let config_rate = self.sample_rate;
        let stop = self.stop;
        let thread_cancel = cancel.clone();
        let capture = std::thread::spawn(move || {
            let result = stream_capture(
                device.as_deref(),
                config_rate,
                &stop,
                max_ms,
                &thread_cancel,
                &tx,
            );
            match result {
                Ok(pcm) => {
                    let _ = tx.send(LiveMsg::Done(pcm));
                }
                // The consumer already knows it cancelled; a second word would race it.
                Err(VoiceError::Cancelled) => {}
                Err(err) => {
                    let _ = tx.send(LiveMsg::Failed(err.to_string()));
                }
            }
        });
        let outcome = loop {
            if cancel.is_cancelled() {
                break Err(VoiceError::Cancelled);
            }
            match rx.recv_timeout(POLL) {
                Ok(LiveMsg::Chunk(chunk)) => on_chunk(&chunk),
                Ok(LiveMsg::Done(pcm)) => break Ok(pcm),
                Ok(LiveMsg::Failed(err)) => break Err(VoiceError::NoDevice(err)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(VoiceError::NoDevice(
                        "input capture thread stopped without a take".into(),
                    ));
                }
            }
        };
        // Joining on every path is what guarantees no stream outlives the call: the thread drops
        // the cpal stream when its loop ends, and its loop polls the token every 50 ms.
        let _ = capture.join();
        outcome
    }
}

/// Built-in cpal when the machine has an input device; the helper path only when it does not.
pub enum DeviceSource {
    BuiltIn(CpalSource),
    Helper(HelperSource),
}

impl DeviceSource {
    pub fn new(config: &VoiceConfig) -> Self {
        // A configured name means the built-in engine even when the host has no default input:
        // `open_input` resolves the name, so an unknown one is an honest error at record time
        // instead of a silent switch to the helper's own default device.
        let built_in = match config.input_device.as_deref() {
            Some(name) if !name.trim().is_empty() => true,
            _ => probe_capture().is_some(),
        };
        if built_in {
            Self::BuiltIn(CpalSource::start(config))
        } else {
            Self::Helper(HelperSource::new(config))
        }
    }
}

impl AudioSource for DeviceSource {
    fn record(&mut self, max_ms: u64) -> Result<Vec<i16>> {
        match self {
            Self::BuiltIn(source) => source.record(max_ms),
            Self::Helper(source) => source.record(max_ms),
        }
    }

    fn record_cancellable(&mut self, max_ms: u64, cancel: &CancelToken) -> Result<Vec<i16>> {
        match self {
            Self::BuiltIn(source) => source.record_cancellable(max_ms, cancel),
            Self::Helper(source) => source.record_cancellable(max_ms, cancel),
        }
    }

    fn record_streaming(
        &mut self,
        max_ms: u64,
        cancel: &CancelToken,
        on_chunk: &mut dyn FnMut(&[i16]),
    ) -> Result<Vec<i16>> {
        match self {
            Self::BuiltIn(source) => source.record_streaming(max_ms, cancel, on_chunk),
            Self::Helper(source) => source.record_streaming(max_ms, cancel, on_chunk),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_frames_average_to_mono() {
        let stereo = [100, 200, -100, -200, 0, 1000];
        assert_eq!(mix_to_mono(&stereo, 2), vec![150, -150, 500]);
        assert_eq!(mix_to_mono(&[7, -7], 1), vec![7, -7]);
    }

    #[test]
    fn resample_changes_length_and_keeps_dc_level() {
        let flat = vec![1234i16; 4_800];
        let out = resample(&flat, 48_000, 16_000);
        assert_eq!(out.len(), 1_600);
        assert!(out.iter().all(|s| *s == 1234));
    }

    #[test]
    fn resample_with_equal_rates_is_the_identity() {
        let pcm = vec![5i16, -5, 9, 3];
        assert_eq!(resample(&pcm, 16_000, 16_000), pcm);
    }

    #[test]
    fn a_slow_ramp_resamples_without_new_extremes() {
        let ramp: Vec<i16> = (0..100).map(|i| i * 100).collect::<Vec<_>>();
        let out = resample(&ramp, 16_000, 8_000);
        assert_eq!(out.len(), 50);
        assert!(out.iter().all(|s| *s >= 0 && *s <= 9_900));
    }

    #[test]
    fn f32_clips_into_i16_range() {
        assert_eq!(
            f32_to_i16(&[0.0, 1.0, -1.0, 2.0]),
            vec![0, i16::MAX, -i16::MAX, i16::MAX]
        );
    }

    #[test]
    fn u16_midpoint_maps_to_zero() {
        assert_eq!(u16_to_i16(&[32_768, 32_767, 65_535]), vec![0, -1, i16::MAX]);
    }

    #[test]
    fn an_exact_device_name_wins_over_a_case_insensitive_one() {
        let available = vec!["USB Mic".to_owned(), "usb mic".to_owned()];
        assert_eq!(match_device("input", "USB Mic", &available).unwrap(), 0);
        assert_eq!(match_device("input", "usb mic", &available).unwrap(), 1);
    }

    #[test]
    fn a_device_name_matches_without_case() {
        let available = vec!["Built-in Audio".to_owned(), "Studio Mic".to_owned()];
        assert_eq!(match_device("input", "studio mic", &available).unwrap(), 1);
        assert_eq!(match_device("input", "STUDIO MIC", &available).unwrap(), 1);
    }

    #[test]
    fn an_unknown_device_names_the_request_and_the_alternatives() {
        let available = vec!["Built-in Audio".to_owned(), "Studio Mic".to_owned()];
        let err = match_device("input", "Headset", &available).unwrap_err();
        assert!(err.contains("Headset"), "{err}");
        assert!(err.contains("Studio Mic"), "{err}");
        assert!(err.contains("Built-in Audio"), "{err}");
        let err = match_device("input", "Headset", &[]).unwrap_err();
        assert!(err.contains("none"), "{err}");
    }

    #[test]
    fn the_default_device_is_listed_first() {
        let names = vec!["B".to_owned(), "A".to_owned(), "C".to_owned()];
        assert_eq!(default_first(names.clone(), Some("B")), vec!["B", "A", "C"]);
        assert_eq!(default_first(names.clone(), Some("C")), vec!["C", "B", "A"]);
        assert_eq!(default_first(names, None), vec!["B", "A", "C"]);
        assert_eq!(
            default_first(vec!["B".to_owned()], Some("A")),
            vec!["A", "B"]
        );
    }

    fn gate() -> SilenceStop {
        SilenceStop::from_config(&VoiceConfig::default())
    }

    fn tone(ms: u64, amplitude: f32, rate: u32) -> Vec<i16> {
        let samples = (ms as u32 * rate / 1_000) as usize;
        (0..samples)
            .map(|i| {
                let phase = (i as f64 * 0.08).sin();
                (phase * f64::from(i16::MAX) * f64::from(amplitude)) as i16
            })
            .collect()
    }

    fn chunks(pcm: &[i16], frame: usize) -> Vec<Vec<i16>> {
        pcm.chunks(frame).map(<[i16]>::to_vec).collect()
    }

    /// The capture loop's decision without a microphone: the same chunks, the same stop rule,
    /// the same cap.
    fn record_stream(stop: &SilenceStop, chunks: &[Vec<i16>], cap: usize, rate: u32) -> Vec<i16> {
        let mut pcm = Vec::new();
        for chunk in chunks {
            pcm.extend_from_slice(chunk);
            if let Some(keep) = stop.keep(&pcm, rate) {
                pcm.truncate(keep);
                break;
            }
            if pcm.len() >= cap {
                pcm.truncate(cap);
                break;
            }
        }
        pcm
    }

    #[test]
    fn a_take_ends_when_the_speaker_stops() {
        let rate = 16_000;
        let mut stream = chunks(&vec![0i16; rate as usize], 160);
        stream.extend(chunks(&tone(600, 0.5, rate), 160));
        stream.extend(chunks(&vec![0i16; 10 * rate as usize], 160));
        let take = record_stream(&gate(), &stream, 8 * rate as usize, rate);
        assert_eq!(
            take.len(),
            (1_000 + 600 + 200) as usize * rate as usize / 1_000,
            "speech plus the hangover, not the 8 s cap"
        );
    }

    #[test]
    fn silence_only_still_runs_to_the_cap() {
        let rate = 16_000;
        let stream = chunks(&vec![0i16; 10 * rate as usize], 160);
        let cap = 8 * rate as usize;
        let take = record_stream(&gate(), &stream, cap, rate);
        assert_eq!(take.len(), cap);
        assert!(take.iter().all(|sample| *sample == 0));
    }

    #[test]
    fn a_late_speaker_is_not_truncated() {
        let rate = 16_000;
        let mut stream = chunks(&vec![0i16; 6 * rate as usize], 160);
        stream.extend(chunks(&tone(500, 0.5, rate), 160));
        stream.extend(chunks(&vec![0i16; 10 * rate as usize], 160));
        let cap = 8 * rate as usize;
        let take = record_stream(&gate(), &stream, cap, rate);
        assert_eq!(
            take.len(),
            (6_000 + 500 + 200) as usize * rate as usize / 1_000
        );
        assert!(
            take.len() < cap,
            "still stops before the cap: {}",
            take.len()
        );
    }

    #[test]
    fn the_take_keeps_the_frames_from_before_the_gate_opened() {
        let rate = 16_000;
        let quiet: Vec<i16> = (0..rate as usize)
            .map(|i| if (i / 8) % 2 == 0 { 6 } else { -6 })
            .collect();
        let mut stream = chunks(&quiet, 160);
        stream.extend(chunks(&tone(600, 0.5, rate), 160));
        stream.extend(chunks(&vec![0i16; 10 * rate as usize], 160));
        let take = record_stream(&gate(), &stream, 8 * rate as usize, rate);
        assert!(
            take.len() > quiet.len(),
            "the pre-roll and the speech are both kept: {}",
            take.len()
        );
        assert_eq!(
            &take[..quiet.len()],
            &quiet[..],
            "pre-roll: every frame before the gate opened"
        );
    }

    #[test]
    fn a_configured_device_keeps_the_built_in_engine() {
        let config = VoiceConfig {
            input_device: Some("Studio Mic".into()),
            ..Default::default()
        };
        assert!(
            matches!(DeviceSource::new(&config), DeviceSource::BuiltIn(_)),
            "the named device must be resolved, never replaced by the helper"
        );
    }

    #[test]
    fn the_configured_device_is_carried_into_the_source() {
        let config = VoiceConfig {
            input_device: Some("Studio Mic".into()),
            ..Default::default()
        };
        assert_eq!(
            CpalSource::start(&config).device.as_deref(),
            Some("Studio Mic")
        );
        let config = VoiceConfig::default();
        assert!(CpalSource::start(&config).device.is_none());
    }

    #[test]
    #[ignore = "hardware test: opens the machine's input devices by name"]
    fn a_named_input_device_opens_and_an_unknown_one_lists_the_alternatives() {
        let Some(default) = probe_capture() else {
            return;
        };
        assert!(open_input(Some(&default)).is_ok());
        let err = open_input(Some("No Such Microphone"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("No Such Microphone"), "{err}");
        assert!(err.contains(&default), "{err}");
    }

    #[test]
    #[ignore = "hardware test: records from the machine's microphone for 1.5 s"]
    fn the_live_tap_delivers_chunks_while_the_microphone_is_open() {
        if probe_capture().is_none() {
            return;
        }
        let config = VoiceConfig::default();
        let mut source = CpalSource::start(&config);
        let mut chunks = 0usize;
        let take = source
            .record_streaming(1_500, &CancelToken::new(), &mut |chunk: &[i16]| {
                if !chunk.is_empty() {
                    chunks += 1;
                }
            })
            .expect("the live path must capture from the default microphone");
        assert!(
            chunks >= 1,
            "the tap must deliver audio, not only the final take"
        );
        assert!(!take.is_empty(), "a 1.5 s take is not empty");
    }

    #[test]
    #[ignore = "hardware test: enumerates the machine's input devices"]
    fn input_devices_lists_the_machine_default_first() {
        let names = input_devices().expect("enumeration must not fail");
        if let Some(default) = probe_capture() {
            assert_eq!(names.first().map(String::as_str), Some(default.as_str()));
        }
    }
}
