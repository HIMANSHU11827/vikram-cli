use crate::{StageStatus, SttProvider, TtsProvider, VoiceConfig, VoiceError, VoiceStatus};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The install hint a missing voice helper gets. No helper is installed by us.
pub fn install_hint(name: &str) -> &'static str {
    match name {
        "parecord" | "paplay" => "apt install pulseaudio-utils",
        "arecord" | "aplay" => "apt install alsa-utils",
        "whisper-cli" => "build whisper.cpp and put whisper-cli on PATH",
        "piper" => "install piper and put it on PATH (operator-supplied voice model)",
        "espeak-ng" => "apt install espeak-ng",
        "timeout" => "install GNU coreutils",
        _ => "install it and keep it on PATH",
    }
}

pub fn missing_tool_message(name: &str) -> String {
    format!("{name} not found; {}", install_hint(name))
}

pub fn which(name: &str) -> Option<PathBuf> {
    if name.trim().is_empty() {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

pub fn first_available(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find(|name| which(name).is_some())
        .map(|name| (*name).to_owned())
}

pub fn probe(config: &VoiceConfig) -> VoiceStatus {
    let capture = if let Some(device) = crate::capture::probe_capture() {
        StageStatus::Ready(format!("built-in cpal ({device})"))
    } else if let Some(helper) = first_available(&[
        config.capture_helper.as_str(),
        config.capture_fallback.as_str(),
    ]) {
        StageStatus::Degraded(format!("no cpal input device; falling back to {helper}"))
    } else {
        StageStatus::Missing(format!(
            "no cpal input device and {} / {} not found",
            config.capture_helper, config.capture_fallback
        ))
    };
    // A cloud provider is reported as that provider, including the exact thing to fix when it
    // cannot run; the local chain is only probed when it is the one that will run.
    let asr = match SttProvider::parse(&config.stt_provider) {
        Some(provider) if provider.is_cloud() => crate::cloud_stt::probe_cloud_stt(config),
        _ => crate::asr::probe_asr(config),
    };
    let tts = match TtsProvider::parse(&config.tts_provider) {
        Some(TtsProvider::ElevenLabs) => crate::cloud_tts::probe_cloud_tts(config),
        Some(TtsProvider::OpenAi | TtsProvider::Custom) => {
            crate::cloud_tts::probe_openai_tts(config)
        }
        _ => probe_local_tts(config),
    };
    let playback = if let Some(device) = crate::player::probe_playback() {
        StageStatus::Ready(format!("built-in cpal output ({device})"))
    } else if let Some(helper) = first_available(&[
        config.player_helper.as_str(),
        config.player_fallback.as_str(),
    ]) {
        StageStatus::Degraded(format!("no cpal output device; falling back to {helper}"))
    } else {
        StageStatus::Missing(format!(
            "no cpal output device and {} / {} not found",
            config.player_helper, config.player_fallback
        ))
    };
    VoiceStatus {
        enabled: config.enabled,
        degraded: capture.is_degraded()
            || asr.is_degraded()
            || tts.is_degraded()
            || playback.is_degraded(),
        capture,
        asr,
        tts,
        playback,
        notes: Vec::new(),
    }
}

/// The vikram voice, probed: piper with a voice model when both are there, espeak-ng when the
/// voice model is missing, and a line naming what to install when neither is.
pub(crate) fn probe_local_tts(config: &VoiceConfig) -> StageStatus {
    let voice = crate::session::pick_voice(config.tts_voice.as_deref());
    let piper = which(&config.tts_helper).is_some();
    let espeak = which(&config.tts_fallback).is_some();
    match (piper, voice) {
        (true, Some(voice)) => {
            StageStatus::Ready(format!("{} with {}", config.tts_helper, voice.display()))
        }
        (false, Some(_)) => StageStatus::Missing(missing_tool_message(&config.tts_helper)),
        (true, None) if espeak => StageStatus::Degraded(format!(
            "no {} voice model; robotic {} fallback",
            config.tts_helper, config.tts_fallback
        )),
        (true, None) => StageStatus::Missing(format!(
            "no {} voice model; {} not found",
            config.tts_helper, config.tts_fallback
        )),
        (false, None) if espeak => StageStatus::Degraded(format!(
            "{} and no voice model; robotic {} fallback",
            config.tts_helper, config.tts_fallback
        )),
        (false, None) => StageStatus::Missing(format!(
            "{} with a voice model, or {}, not found",
            config.tts_helper, config.tts_fallback
        )),
    }
}

/// Run one helper with bounded output and a deadline. `input` is written on a detached thread so
/// a wedged helper cannot block the write past the deadline. A cancel observed mid-run kills the
/// helper and returns [`VoiceError::Cancelled`] instead of waiting the helper out.
pub(crate) fn run_cancellable(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    timeout_secs: u64,
    cancel: &crate::CancelToken,
) -> Result<Vec<u8>, VoiceError> {
    run_with(program, args, input, timeout_secs, Some(cancel))
}

fn run_with(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    timeout_secs: u64,
    cancel: Option<&crate::CancelToken>,
) -> Result<Vec<u8>, VoiceError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                VoiceError::MissingHelper(missing_tool_message(program))
            } else {
                VoiceError::Helper(format!("cannot run `{program}`: {err}"))
            }
        })?;

    if let Some(data) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Some(cancel) = cancel
            && cancel.is_cancelled()
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(VoiceError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|err| VoiceError::Helper(format!("`{program}`: {err}")))?;
                if status.success() {
                    return Ok(output.stdout);
                }
                let stderr = String::from_utf8_lossy(&output.stderr);
                let detail = stderr.trim();
                return Err(VoiceError::Helper(if detail.is_empty() {
                    format!("`{program}` exited {status}")
                } else {
                    format!("`{program}` exited {status}: {detail}")
                }));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(VoiceError::Timeout(timeout_secs));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(VoiceError::Helper(format!("`{program}`: {err}"))),
        }
    }
}

pub(crate) fn pcm_from_bytes(bytes: &[u8]) -> Result<Vec<i16>, VoiceError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(VoiceError::InvalidAudio(
            "raw PCM has an odd byte count".into(),
        ));
    }
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| i16::from_le_bytes(*chunk))
        .collect())
}

pub(crate) fn write_wav(path: &Path, pcm: &[i16], sample_rate: u16) -> Result<(), VoiceError> {
    fs::write(path, wav_bytes(pcm, sample_rate))
        .map_err(|err| VoiceError::Helper(format!("write {}: {err}", path.display())))
}

/// The same WAV `write_wav` puts on disk, in memory: a cloud reply arrives as raw PCM and has to
/// become WAV bytes for the player without a scratch file in between.
pub(crate) fn wav_bytes(pcm: &[i16], sample_rate: u16) -> Vec<u8> {
    let data_len = pcm.len() * 2;
    let mut bytes = Vec::with_capacity(44 + data_len);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&u32::from(sample_rate).to_le_bytes());
    bytes.extend_from_slice(&(u32::from(sample_rate) * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in pcm {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// A scratch directory that is removed on drop, including on error paths.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new() -> Result<Self, VoiceError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("vikram-voice-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path)
            .map_err(|err| VoiceError::Helper(format!("create {}: {err}", path.display())))?;
        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_conversion_handles_little_endian_and_rejects_odd_lengths() {
        assert_eq!(
            pcm_from_bytes(&[0x00, 0x80, 0xff, 0x7f]).unwrap(),
            vec![i16::MIN, i16::MAX]
        );
        assert!(pcm_from_bytes(&[1]).is_err());
    }

    #[test]
    fn wav_header_matches_the_pcm_it_wraps() {
        let scratch = Scratch::new().unwrap();
        let path = scratch.path().join("t.wav");
        write_wav(&path, &[1, -2, 3], 16_000).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 6);
        assert_eq!(bytes.len(), 50);
    }

    #[test]
    fn scratch_directory_disappears_on_drop() {
        let path = {
            let scratch = Scratch::new().unwrap();
            scratch.path().to_owned()
        };
        assert!(!path.exists());
    }

    #[test]
    fn missing_helper_names_an_install_hint() {
        let text = missing_tool_message("whisper-cli");
        assert!(text.contains("whisper.cpp"));
    }
}
