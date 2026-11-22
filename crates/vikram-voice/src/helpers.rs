use crate::{StageStatus, VoiceConfig, VoiceError, VoiceStatus};
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
    let capture = match first_available(&[
        config.capture_helper.as_str(),
        config.capture_fallback.as_str(),
    ]) {
        Some(_) => StageStatus::Ready,
        None => StageStatus::Missing(missing_tool_message(&config.capture_helper)),
    };
    let asr = if which(&config.asr_helper).is_none() {
        StageStatus::Missing(missing_tool_message(&config.asr_helper))
    } else if config.asr_model.as_os_str().is_empty() {
        StageStatus::Missing("no asr_model configured".into())
    } else if !config.asr_model.is_file() {
        StageStatus::Missing(format!(
            "model file missing: {}",
            config.asr_model.display()
        ))
    } else {
        StageStatus::Ready
    };
    let tts = match (
        which(&config.tts_helper).is_some() && config.tts_voice.is_some(),
        which(&config.tts_fallback).is_some(),
    ) {
        (true, _) | (_, true) => StageStatus::Ready,
        _ => StageStatus::Missing(format!(
            "{} with a voice model, or {}, not found",
            config.tts_helper, config.tts_fallback
        )),
    };
    let playback = match first_available(&[
        config.player_helper.as_str(),
        config.player_fallback.as_str(),
    ]) {
        Some(_) => StageStatus::Ready,
        None => StageStatus::Missing(missing_tool_message(&config.player_helper)),
    };
    VoiceStatus {
        enabled: config.enabled,
        capture,
        asr,
        tts,
        playback,
    }
}

/// Run one helper with bounded output and a deadline. `input` is written on a detached thread so
/// a wedged helper cannot block the write past the deadline.
pub(crate) fn run(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    timeout_secs: u64,
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
    fs::write(path, bytes)
        .map_err(|err| VoiceError::Helper(format!("write {}: {err}", path.display())))
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
