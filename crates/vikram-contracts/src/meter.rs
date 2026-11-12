//! The two primitives the live loop shares with every interface above it: a cancellation flag and
//! a token count. Both are deliberately dependency-free — a caller must be able to price or stop
//! work without reaching for an execution engine.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Default, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Clear the flag in place, so the next turn starts uncancelled **and** every holder of a clone
    /// sees it. Replacing the token with a new one leaves the running turn holding a clone that is
    /// still cancelled, which is what made the turn after a stop end before it began.
    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Two numbers, deliberately separate. `estimated` is what the runtime can know before a call is
/// billed; `reported` is what the provider said it cost. Conflating them is how agent harnesses
/// end up with unusable cost data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenMeter {
    pub estimated_in: u64,
    pub estimated_out: u64,
    pub reported_in: Option<u64>,
    pub reported_out: Option<u64>,
    pub calls: u64,
}

impl TokenMeter {
    /// Structural token count: the one counter the whole workspace shares.
    ///
    /// A real BPE tokenizer needs the model's own vocabulary, which is not distributable here, so
    /// this counts the shapes a tokenizer actually splits on — word runs, punctuation runs, digit
    /// groups, newlines, one token per CJK or emoji glyph — instead of dividing the length by four.
    /// Four-chars-per-token prices "the" the same as a 40-character identifier and treats Chinese as
    /// free, which makes every budget downstream wrong in the same direction.
    ///
    /// Used for pre-call budgeting and the compaction trigger, never for billing: reported usage
    /// always outranks it.
    pub fn estimate(text: &str) -> u64 {
        let mut total = 0u64;
        let mut word = 0u64;
        let mut punct = 0u64;
        let mut digits = 0u64;

        let flush_word = |word: &mut u64, total: &mut u64| {
            if *word > 0 {
                // Common short words are one token; long ones split roughly every five characters.
                *total += 1 + word.saturating_sub(6).div_ceil(5);
                *word = 0;
            }
        };
        let flush_punct = |punct: &mut u64, total: &mut u64| {
            if *punct > 0 {
                *total += (*punct as f64 * 0.75).ceil() as u64;
                *punct = 0;
            }
        };
        let flush_digits = |digits: &mut u64, total: &mut u64| {
            if *digits > 0 {
                *total += digits.div_ceil(3);
                *digits = 0;
            }
        };

        for ch in text.chars() {
            if ch.is_alphabetic() {
                flush_punct(&mut punct, &mut total);
                flush_digits(&mut digits, &mut total);
                if is_wide_glyph(ch) {
                    total += 1;
                } else {
                    word += 1;
                }
            } else if ch.is_ascii_digit() {
                flush_word(&mut word, &mut total);
                flush_punct(&mut punct, &mut total);
                digits += 1;
            } else if ch.is_whitespace() {
                flush_word(&mut word, &mut total);
                flush_punct(&mut punct, &mut total);
                flush_digits(&mut digits, &mut total);
                if ch == '\n' {
                    total += 1;
                }
            } else if is_wide_glyph(ch) {
                flush_word(&mut word, &mut total);
                flush_punct(&mut punct, &mut total);
                flush_digits(&mut digits, &mut total);
                total += 1;
            } else {
                flush_word(&mut word, &mut total);
                flush_digits(&mut digits, &mut total);
                punct += 1;
            }
        }

        flush_word(&mut word, &mut total);
        flush_punct(&mut punct, &mut total);
        flush_digits(&mut digits, &mut total);
        total
    }

    pub fn add_input(&mut self, tokens: u64) {
        self.estimated_in += tokens;
    }

    pub fn add_output(&mut self, tokens: u64) {
        self.estimated_out += tokens;
    }

    pub fn record_reported(&mut self, input: u64, output: u64) {
        self.reported_in = Some(self.reported_in.unwrap_or(0) + input);
        self.reported_out = Some(self.reported_out.unwrap_or(0) + output);
        self.calls += 1;
    }

    pub fn estimated_total(&self) -> u64 {
        self.estimated_in + self.estimated_out
    }
}

/// Scripts that tokenize near one token per glyph rather than per word.
fn is_wide_glyph(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x11FF      // hangul jamo
        | 0x2E80..=0x303F    // CJK radicals, kangxi, CJK punctuation
        | 0x3040..=0x30FF    // kana
        | 0x3100..=0x312F    // bopomofo
        | 0x3400..=0x4DBF    // extension A
        | 0x4E00..=0x9FFF    // han
        | 0xAC00..=0xD7AF    // hangul syllables
        | 0xF900..=0xFAFF    // compatibility ideographs
        | 0x1F300..=0x1FAFF) // emoji and symbols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_across_clones() {
        let token = CancellationToken::new();
        let handle = token.clone();
        assert!(!handle.is_cancelled());
        token.cancel();
        assert!(handle.is_cancelled());
    }

    #[test]
    fn estimation_counts_shapes_not_characters() {
        assert_eq!(TokenMeter::estimate(""), 0);
        // One word run, however long the four-chars rule would have called it two.
        assert_eq!(TokenMeter::estimate("abc"), 1);
        assert_eq!(TokenMeter::estimate("hello world"), 2);
        // Repeated short words are not free: each one is its own token.
        assert_eq!(TokenMeter::estimate("the the the"), 3);
        // A long identifier really does split.
        assert_eq!(TokenMeter::estimate("context_window_bytes"), 6);
        // Digits pack three to a token; punctuation costs about three quarters each.
        assert_eq!(TokenMeter::estimate("123456"), 2);
        assert_eq!(TokenMeter::estimate("中文测试"), 4);
        assert_eq!(TokenMeter::estimate("a\nb"), 3);
    }

    #[test]
    fn reported_usage_accumulates_separately_from_estimates() {
        let mut meter = TokenMeter::default();
        meter.add_input(500);
        meter.record_reported(120, 30);
        meter.record_reported(80, 20);
        assert_eq!(meter.estimated_in, 500);
        assert_eq!(meter.reported_in, Some(200));
        assert_eq!(meter.reported_out, Some(50));
        assert_eq!(meter.calls, 2);
    }
}
