//! Built-in speaker playback via cpal, with the paplay/aplay helper as a detected fallback.
//!
//! Synthesis (piper/espeak-ng helpers, per `docs/voice-plan.md` §2.6) hands this stage WAV
//! bytes; the engine decodes them once, mixes and resamples to the device's own format, and
//! plays through a linked cpal stream. Playback is bounded by the WAV's duration plus slack
//! and stops within one poll interval of a cancel.

use crate::cancel::CancelToken;
use crate::capture::{device_name, match_device, mix_to_mono, ordered_names, resample};
use crate::session::Player;
use crate::{HelperPlayer, Result, VoiceConfig, VoiceError};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);
const TIMEOUT_SLACK_SECS: u64 = 5;

/// The default output device's name, without opening a stream.
pub fn probe_playback() -> Option<String> {
    let device = cpal::default_host().default_output_device()?;
    let name = device.description().ok()?.name().to_owned();
    let has_config = device.default_output_config().is_ok();
    has_config.then_some(name)
}

/// Every output device cpal can see, the default first.
pub fn output_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let default = host
        .default_output_device()
        .map(|device| device_name(&device));
    let devices: Vec<cpal::Device> = host
        .output_devices()
        .map_err(|err| VoiceError::NoDevice(format!("enumerate output devices: {err}")))?
        .collect();
    Ok(ordered_names(&devices, default.as_deref()))
}

/// The configured output device, or the host default when none is configured.
fn open_output(configured: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let Some(requested) = configured.map(str::trim).filter(|name| !name.is_empty()) else {
        return host
            .default_output_device()
            .ok_or_else(|| VoiceError::NoDevice("no default output device".into()));
    };
    let devices: Vec<cpal::Device> = host
        .output_devices()
        .map_err(|err| VoiceError::NoDevice(format!("enumerate output devices: {err}")))?
        .collect();
    let names: Vec<String> = devices.iter().map(device_name).collect();
    let index = match_device("output", requested, &names).map_err(VoiceError::NoDevice)?;
    Ok(devices
        .into_iter()
        .nth(index)
        .expect("index came from the same list"))
}

/// WAV decode, one place: 16-bit PCM only, the format every synthesizer here emits.
fn decode_wav(wav: &[u8]) -> Result<(Vec<i16>, u32, u16)> {
    let reader = hound::WavReader::new(std::io::Cursor::new(wav))
        .map_err(|err| VoiceError::InvalidAudio(format!("wav decode: {err}")))?;
    let spec = reader.spec();
    if !matches!(spec.sample_format, hound::SampleFormat::Int) {
        return Err(VoiceError::InvalidAudio(format!(
            "unsupported wav sample format {:?}; expected 16-bit PCM",
            spec.sample_format
        )));
    }
    let samples = reader
        .into_samples::<i16>()
        .collect::<std::result::Result<Vec<i16>, _>>()
        .map_err(|err| VoiceError::InvalidAudio(format!("wav samples: {err}")))?;
    Ok((samples, spec.sample_rate, spec.channels))
}

fn interleave_mono(pcm: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return pcm.to_vec();
    }
    pcm.iter()
        .flat_map(|sample| std::iter::repeat_n(*sample, channels))
        .collect()
}

/// Fill each callback from the precomputed frames; flip `done` when the last frame is sent.
fn writer<T>(
    frames: Arc<Vec<T>>,
    done: Arc<AtomicBool>,
) -> impl FnMut(&mut [T], &cpal::OutputCallbackInfo) + use<T>
where
    T: cpal::Sample,
{
    let mut next = 0;
    move |data, _| {
        let take = frames.len().saturating_sub(next).min(data.len());
        data[..take].copy_from_slice(&frames[next..next + take]);
        next += take;
        for slot in &mut data[take..] {
            *slot = T::EQUILIBRIUM;
        }
        if next >= frames.len() {
            done.store(true, Ordering::SeqCst);
        }
    }
}

/// The linked playback engine. WAV in, speaker out.
pub struct CpalPlayer {
    device: Option<String>,
}

impl CpalPlayer {
    pub fn open(config: &VoiceConfig) -> Self {
        Self {
            device: config.output_device.clone(),
        }
    }
}

impl Player for CpalPlayer {
    fn play(&mut self, wav: &[u8]) -> Result<()> {
        self.play_cancellable(wav, &CancelToken::new())
    }

    fn play_cancellable(&mut self, wav: &[u8], cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        let (samples, rate, channels) = decode_wav(wav)?;
        let mono = mix_to_mono(&samples, channels.max(1) as usize);
        let device = open_output(self.device.as_deref())?;
        let supported = device
            .default_output_config()
            .map_err(|err| VoiceError::NoDevice(format!("output device has no config: {err}")))?;
        let device_rate = supported.sample_rate();
        let device_channels = supported.channels().max(1) as usize;
        let stream_config: cpal::StreamConfig = supported.into();
        let frames = interleave_mono(&resample(&mono, rate, device_rate), device_channels);
        let done = Arc::new(AtomicBool::new(false));
        let (err_tx, err_rx) = std::sync::mpsc::channel::<String>();
        let err_fn = move |err: cpal::Error| {
            let _ = err_tx.send(err.to_string());
        };
        let timeout_secs = ((frames.len() as u64 * 1_000)
            / (u64::from(device_rate) * device_channels as u64).max(1))
            / 1_000
            + TIMEOUT_SLACK_SECS;
        let stream = match supported.sample_format() {
            cpal::SampleFormat::I16 => device.build_output_stream(
                stream_config,
                writer(Arc::new(frames), done.clone()),
                err_fn,
                None,
            ),
            cpal::SampleFormat::F32 => device.build_output_stream(
                stream_config,
                writer(
                    Arc::new(
                        frames
                            .iter()
                            .map(|sample| f32::from(*sample) / i16::MAX as f32)
                            .collect(),
                    ),
                    done.clone(),
                ),
                err_fn,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_output_stream(
                stream_config,
                writer(
                    Arc::new(
                        frames
                            .iter()
                            .map(|sample| (i32::from(*sample) + 32_768) as u16)
                            .collect(),
                    ),
                    done.clone(),
                ),
                err_fn,
                None,
            ),
            other => {
                return Err(VoiceError::NoDevice(format!(
                    "device sample format {other:?} is not supported"
                )));
            }
        };
        let stream =
            stream.map_err(|err| VoiceError::NoDevice(format!("open output stream: {err}")))?;
        stream
            .play()
            .map_err(|err| VoiceError::NoDevice(format!("start output stream: {err}")))?;

        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if cancel.is_cancelled() {
                drop(stream);
                return Err(VoiceError::Cancelled);
            }
            if done.load(Ordering::SeqCst) {
                return Ok(());
            }
            if let Ok(err) = err_rx.try_recv() {
                return Err(VoiceError::NoDevice(format!(
                    "output stream stopped: {err}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(VoiceError::Timeout(timeout_secs));
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Built-in cpal output when the machine has a speaker; the helper path only when it does not.
pub enum DevicePlayer {
    BuiltIn(CpalPlayer),
    Helper(HelperPlayer),
}

impl DevicePlayer {
    pub fn new(config: &VoiceConfig) -> Self {
        // A configured name means the built-in engine even when the host has no default output:
        // `open_output` resolves the name, so an unknown one is an honest error at play time
        // instead of a silent switch to the helper's own default device.
        let built_in = match config.output_device.as_deref() {
            Some(name) if !name.trim().is_empty() => true,
            _ => probe_playback().is_some(),
        };
        if built_in {
            Self::BuiltIn(CpalPlayer::open(config))
        } else {
            Self::Helper(HelperPlayer::new(config))
        }
    }
}

impl Player for DevicePlayer {
    fn play(&mut self, wav: &[u8]) -> Result<()> {
        match self {
            Self::BuiltIn(player) => player.play(wav),
            Self::Helper(player) => player.play(wav),
        }
    }

    fn play_cancellable(&mut self, wav: &[u8], cancel: &CancelToken) -> Result<()> {
        match self {
            Self::BuiltIn(player) => player.play_cancellable(wav, cancel),
            Self::Helper(player) => player.play_cancellable(wav, cancel),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::Scratch;

    #[test]
    fn a_wav_round_trips_through_decode() {
        let scratch = Scratch::new().unwrap();
        let path = scratch.path().join("t.wav");
        let pcm: Vec<i16> = vec![0, 1000, -1000, i16::MAX];
        crate::helpers::write_wav(&path, &pcm, 16_000).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let (samples, rate, channels) = decode_wav(&bytes).unwrap();
        assert_eq!(samples, pcm);
        assert_eq!(rate, 16_000);
        assert_eq!(channels, 1);
    }

    #[test]
    fn garbage_bytes_are_rejected_honestly() {
        assert!(matches!(
            decode_wav(b"not a wav"),
            Err(VoiceError::InvalidAudio(_))
        ));
    }

    #[test]
    fn mono_expands_to_the_device_channel_count() {
        assert_eq!(interleave_mono(&[7, -7], 1), vec![7, -7]);
        assert_eq!(interleave_mono(&[7, -7], 2), vec![7, 7, -7, -7]);
    }

    #[test]
    fn the_player_prefers_the_built_in_engine_and_falls_back_honestly() {
        let config = VoiceConfig::default();
        match DevicePlayer::new(&config) {
            DevicePlayer::BuiltIn(_) => {
                let name = probe_playback().expect("built-in implies a device");
                assert!(!name.is_empty());
            }
            DevicePlayer::Helper(_) => {
                assert!(probe_playback().is_none());
            }
        }
    }

    #[test]
    fn a_configured_device_keeps_the_built_in_engine() {
        let config = VoiceConfig {
            output_device: Some("Studio Monitors".into()),
            ..Default::default()
        };
        assert!(
            matches!(DevicePlayer::new(&config), DevicePlayer::BuiltIn(_)),
            "the named device must be resolved, never replaced by the helper"
        );
    }

    #[test]
    fn the_configured_device_is_carried_into_the_player() {
        let config = VoiceConfig {
            output_device: Some("Studio Monitors".into()),
            ..Default::default()
        };
        assert_eq!(
            CpalPlayer::open(&config).device.as_deref(),
            Some("Studio Monitors")
        );
        let config = VoiceConfig::default();
        assert!(CpalPlayer::open(&config).device.is_none());
    }

    #[test]
    #[ignore = "hardware test: opens the machine's output devices by name"]
    fn a_named_output_device_opens_and_an_unknown_one_lists_the_alternatives() {
        let Some(default) = probe_playback() else {
            return;
        };
        assert!(open_output(Some(&default)).is_ok());
        let err = open_output(Some("No Such Speaker"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("No Such Speaker"), "{err}");
        assert!(err.contains(&default), "{err}");
    }

    #[test]
    #[ignore = "hardware test: enumerates the machine's output devices"]
    fn output_devices_lists_the_machine_default_first() {
        let names = output_devices().expect("enumeration must not fail");
        if let Some(default) = probe_playback() {
            assert_eq!(names.first().map(String::as_str), Some(default.as_str()));
        }
    }

    #[test]
    #[ignore = "hardware test: opens the default output device and plays a short quiet tone"]
    fn the_built_in_player_plays_a_tone() {
        let tone: Vec<i16> = (0..8_000)
            .map(|i| ((i as f32 * 0.05).sin() * 3_000.0) as i16)
            .collect();
        let scratch = Scratch::new().unwrap();
        let path = scratch.path().join("tone.wav");
        crate::helpers::write_wav(&path, &tone, 16_000).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let started = Instant::now();
        CpalPlayer::open(&VoiceConfig::default())
            .play_cancellable(&bytes, &CancelToken::new())
            .expect("built-in playback must succeed");
        eprintln!("tone played in {:?}", started.elapsed());
    }
}
