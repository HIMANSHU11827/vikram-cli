//! Runtime infrastructure the execution loop needs: cancellation, the clock seam, and the token
//! meter. All three exist so the loop is deterministic under test and honest about cost in
//! production.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

pub trait Clock {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Fixed clock for tests and for replaying a journal with its original timestamps.
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
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
    /// Rough four-chars-per-token heuristic, used only for pre-call budgeting and for the
    /// compaction trigger, never for billing.
    pub fn estimate(text: &str) -> u64 {
        (text.len() as u64).div_ceil(4)
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
    fn estimation_rounds_up_instead_of_truncating_small_values() {
        assert_eq!(TokenMeter::estimate(""), 0);
        assert_eq!(TokenMeter::estimate("abc"), 1);
        assert_eq!(TokenMeter::estimate("abcd"), 1);
        assert_eq!(TokenMeter::estimate("abcde"), 2);
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

    #[test]
    fn a_fixed_clock_makes_journal_timestamps_reproducible() {
        assert_eq!(FixedClock(1234).now_ms(), 1234);
        assert!(SystemClock.now_ms() > 1_700_000_000_000);
    }
}
