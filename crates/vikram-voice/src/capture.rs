//! Built-in microphone capture via cpal, with the helper path as a detected fallback.
//!
//! The device is opened at whatever rate and channel count it supports; samples are converted
//! to interleaved i16, mixed to mono, and linearly resampled to the config rate once, at this
//! boundary, as `docs/voice-plan.md` §2.1 requires. Recording is bounded by `max_ms` and
//! aborted within one poll interval of a cancel.

use crate::cancel::CancelToken;
use crate::session::AudioSource;
use crate::{HelperSource, Result, VoiceConfig, VoiceError};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);

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

/// One capture from the configured input device (the host default when unset), converted to
/// config-rate mono i16.
pub struct CpalSource {
    sample_rate: u16,
    device: Option<String>,
}

impl CpalSource {
    pub fn start(config: &VoiceConfig) -> Self {
        Self {
            sample_rate: config.sample_rate,
            device: config.input_device.clone(),
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
        let stream =
            stream.map_err(|err| VoiceError::NoDevice(format!("open input stream: {err}")))?;
        stream
            .play()
            .map_err(|err| VoiceError::NoDevice(format!("start input stream: {err}")))?;

        let deadline = Instant::now() + Duration::from_millis(max_ms.max(1));
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
                Ok(chunk) => pcm.extend_from_slice(&chunk),
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
}

/// Built-in cpal when the machine has an input device; the helper path only when it does not.
pub enum DeviceSource {
    BuiltIn(CpalSource),
    Helper(HelperSource),
}

impl DeviceSource {
    pub fn new(config: &VoiceConfig) -> Self {
        match probe_capture() {
            Some(_) => Self::BuiltIn(CpalSource::start(config)),
            None => Self::Helper(HelperSource::new(config)),
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

    #[test]
    #[ignore = "hardware test: enumerates the machine's input devices"]
    fn input_devices_lists_the_machine_default_first() {
        let names = input_devices().expect("enumeration must not fail");
        if let Some(default) = probe_capture() {
            assert_eq!(names.first().map(String::as_str), Some(default.as_str()));
        }
    }
}
