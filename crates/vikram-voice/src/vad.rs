//! Energy-based voice activity detection. Phase 1 heuristic: zero dependencies, honest limits.
//!
//! RMS over 10 ms frames with a hangover keeps short pauses inside one utterance. It cannot
//! separate speech from a noisy fan; that is why `docs/voice-plan.md` schedules Silero VAD for
//! phase 2 instead of claiming noise robustness here.

const FRAME_MS: u64 = 10;

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
}
