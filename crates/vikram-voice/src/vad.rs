//! Energy-based voice activity detection with an adaptive noise floor.
//!
//! RMS over 10 ms frames. The gate is the measured quiet level of the take times a ratio, so a
//! steady fan or hum raises the gate instead of opening it; a run of voiced frames shorter than
//! `MIN_SPEECH_MS` is not a word. This still cannot separate speech from loud non-steady noise
//! (a door slam, music), which is why `docs/voice-plan.md` schedules a model-based VAD later.

const FRAME_MS: u64 = 10;

fn frame_rms(pcm: &[i16], frame: usize) -> Vec<f32> {
    pcm.chunks(frame)
        .map(|chunk| {
            let sum: f64 = chunk
                .iter()
                .map(|sample| {
                    let value = *sample as f64 / i16::MAX as f64;
                    value * value
                })
                .sum();
            (sum / chunk.len() as f64).sqrt() as f32
        })
        .collect()
}

/// The quiet level of a recording: the tenth percentile of frame energy. A fan, a hum or a
/// keyboard is steady, so it lands here and the gate rises above it.
pub fn noise_floor(frames: &[f32]) -> f32 {
    if frames.is_empty() {
        return 0.0;
    }
    let mut sorted = frames.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let index = ((sorted.len() as f32 * 0.1) as usize).min(sorted.len() - 1);
    sorted[index]
}

/// A room this loud cannot be gated: playback, a fan at full speed or a clipped microphone puts
/// the whole take near full scale, so every frame looks voiced and the gate rises above the
/// speaker. Above this floor the honest answer is to say so, not to report "no speech".
pub const LOUD_FLOOR: f32 = 0.12;

/// Whether a take is so loud that energy gating cannot separate speech from background.
pub fn is_loud(pcm: &[i16], sample_rate: u16) -> bool {
    let frame = (sample_rate as usize * FRAME_MS as usize) / 1000;
    if pcm.is_empty() || frame == 0 {
        return false;
    }
    noise_floor(&frame_rms(pcm, frame)) >= LOUD_FLOOR
}

/// The level a frame must reach to be called voiced: above the noise floor by `FLOOR_RATIO`,
/// never below `absolute`.
pub fn speech_gate(frames: &[f32], absolute: f32, ratio: f32) -> f32 {
    (noise_floor(frames) * ratio).max(absolute)
}

/// Sample range of the first sustained speech run, extended by the hangover.
///
/// `None` means nothing in this recording was speech: either the whole take sat at the noise
/// floor (a fan, a hum, a keyboard) or it never stayed above the gate long enough to be a
/// word. Returning `None` here is what keeps a noise-only take from being sent to the model,
/// where it would be invented into a sentence.
pub fn detect_speech(
    pcm: &[i16],
    absolute: f32,
    ratio: f32,
    min_speech_ms: u64,
    hangover_ms: u64,
    sample_rate: u16,
) -> Option<(usize, usize)> {
    if pcm.is_empty() || sample_rate == 0 {
        return None;
    }
    let frame = (sample_rate as usize * FRAME_MS as usize) / 1000;
    if frame == 0 {
        return None;
    }
    let frames = frame_rms(pcm, frame);
    let gate = speech_gate(&frames, absolute, ratio);
    let voiced: Vec<bool> = frames.iter().map(|rms| *rms >= gate).collect();
    let min_frames = (((min_speech_ms * sample_rate as u64) / 1000) as usize / frame).max(1);
    let hangover_frames = ((hangover_ms * sample_rate as u64) / 1000) as usize / frame;

    let mut start_frame = None;
    let mut run = 0usize;
    for (index, is_voiced) in voiced.iter().enumerate() {
        if *is_voiced {
            run += 1;
            if run >= min_frames {
                start_frame = Some(index + 1 - run);
                break;
            }
        } else {
            run = 0;
        }
    }
    let start_frame = start_frame?;

    let mut last_voiced = start_frame;
    let mut gap = 0usize;
    for (index, is_voiced) in voiced.iter().enumerate().skip(start_frame) {
        if *is_voiced {
            last_voiced = index;
            gap = 0;
        } else {
            gap += 1;
            if gap > hangover_frames {
                break;
            }
        }
    }

    let start = start_frame * frame;
    let end = ((last_voiced + 1 + hangover_frames) * frame).min(pcm.len());
    (start < end).then_some((start, end))
}

pub fn is_silent(pcm: &[i16], threshold: f32) -> bool {
    pcm.iter()
        .all(|sample| (*sample as f32 / i16::MAX as f32).abs() < threshold)
}

/// Returns the half-open sample range containing speech. `(0, 0)` means silence.
pub fn trim_silence(
    pcm: &[i16],
    threshold: f32,
    hangover_ms: u64,
    sample_rate: u16,
) -> (usize, usize) {
    if pcm.is_empty() || sample_rate == 0 {
        return (0, 0);
    }
    let frame = (sample_rate as usize * FRAME_MS as usize) / 1000;
    let hangover = ((hangover_ms * sample_rate as u64) / 1000) as usize / frame.max(1);
    let mut first = None;
    let mut last = None;
    for (index, chunk) in pcm.chunks(frame).enumerate() {
        let sum: f64 = chunk
            .iter()
            .map(|sample| {
                let value = *sample as f64 / i16::MAX as f64;
                value * value
            })
            .sum();
        let rms = (sum / chunk.len() as f64).sqrt() as f32;
        if rms >= threshold {
            if first.is_none() {
                first = Some(index * frame);
            }
            last = Some(((index + 1) * frame).min(pcm.len()));
        }
    }
    match (first, last) {
        (Some(start), Some(end)) if start < end => (start, (end + hangover * frame).min(pcm.len())),
        _ => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|i| {
                let phase = (i as f64 * 0.08).sin();
                (phase * i16::MAX as f64 * 0.5) as i16
            })
            .collect()
    }

    #[test]
    fn silence_trims_to_nothing() {
        let pcm = vec![0i16; 16_000];
        assert!(is_silent(&pcm, 0.01));
        assert_eq!(trim_silence(&pcm, 0.01, 200, 16_000), (0, 0));
    }

    #[test]
    fn speech_in_the_middle_is_kept_with_hangover() {
        let mut pcm = vec![0i16; 1_600];
        pcm.extend_from_slice(&tone(1_600));
        pcm.extend_from_slice(&vec![0i16; 1_600]);
        let (start, end) = trim_silence(&pcm, 0.01, 100, 16_000);
        assert_eq!(start, 1_600);
        assert!(end > 3_200, "hangover should extend past the tone: {end}");
        assert!(end <= pcm.len());
    }

    #[test]
    fn zero_sample_rate_is_not_a_panic() {
        assert_eq!(trim_silence(&tone(100), 0.01, 100, 0), (0, 0));
    }

    fn steady(amplitude: f32, samples: usize) -> Vec<i16> {
        let peak = (i16::MAX as f32 * amplitude) as i16;
        (0..samples)
            .map(|i| if (i / 8) % 2 == 0 { peak } else { -peak })
            .collect()
    }

    #[test]
    fn a_steady_fan_is_not_speech() {
        let pcm = steady(0.2, 16_000);
        assert_eq!(detect_speech(&pcm, 0.01, 2.5, 250, 200, 16_000), None);
    }

    #[test]
    fn a_short_click_is_not_speech() {
        let mut pcm = vec![0i16; 4_000];
        pcm.extend_from_slice(&steady(0.4, 800));
        pcm.extend_from_slice(&vec![0i16; 4_000]);
        assert_eq!(detect_speech(&pcm, 0.01, 2.5, 250, 200, 16_000), None);
    }

    #[test]
    fn a_word_over_room_noise_is_speech() {
        let mut pcm = steady(0.002, 4_800);
        pcm.extend_from_slice(&steady(0.15, 8_000));
        pcm.extend_from_slice(&steady(0.002, 4_800));
        let (start, end) =
            detect_speech(&pcm, 0.01, 2.5, 250, 200, 16_000).expect("speech expected");
        assert!(start >= 4_000, "leading noise should be trimmed: {start}");
        assert!(start <= 5_000, "speech starts around 4800 samples: {start}");
        assert!(end >= 12_800, "the word must be inside the range: {end}");
    }

    #[test]
    fn the_gate_follows_a_loud_room() {
        let quiet = vec![0.001f32; 100];
        let loud = vec![0.05f32; 100];
        assert!(speech_gate(&loud, 0.01, 2.5) > speech_gate(&quiet, 0.01, 2.5));
        assert!(
            speech_gate(&quiet, 0.01, 2.5) >= 0.01,
            "absolute floor still applies"
        );
    }

    #[test]
    fn a_loud_room_is_reported_as_too_loud_to_gate() {
        assert!(
            !is_loud(&vec![200i16; 16_000], 16_000),
            "a quiet room is fine"
        );
        assert!(
            is_loud(&vec![12_000i16; 16_000], 16_000),
            "playback or a clipped mic puts the floor above LOUD_FLOOR"
        );
        assert!(!is_loud(&[], 16_000), "an empty take is not loud");
    }
}
