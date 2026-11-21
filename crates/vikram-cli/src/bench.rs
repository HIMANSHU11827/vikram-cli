//! `vikram bench` — benchmark suites. The learning suite lives in
//! `vikram-bench` so `vikram-bench learning` and `vikram bench learning`
//! report identical numbers.

use std::path::PathBuf;

use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum BenchAction {
    /// Learning-system benchmarks: real store state, deterministic, offline.
    Learning {
        /// Compare metrics against a baseline JSON written by --save-baseline.
        #[arg(long, value_name = "PATH")]
        baseline: Option<PathBuf>,
        /// Save this run's metrics as a baseline JSON for later comparison.
        #[arg(long, value_name = "PATH")]
        save_baseline: Option<PathBuf>,
    },
    /// Personality-consistency harness: deterministic style fingerprinting of
    /// assistant outputs against a seeded baseline. Offline, no model calls.
    Personality {
        /// Directory of observed assistant turns (*.txt files).
        #[arg(long, value_name = "DIR")]
        turns: Option<PathBuf>,
        /// An inline observed turn; repeatable.
        #[arg(long, value_name = "TEXT")]
        text: Vec<String>,
        /// Drift threshold in [0, 1]; turns below it are flagged WARN.
        #[arg(long, value_name = "F", default_value_t = vikram_bench::personality::DEFAULT_THRESHOLD)]
        threshold: f64,
    },
}

pub fn run(action: BenchAction) -> Result<(), String> {
    match action {
        BenchAction::Learning {
            baseline,
            save_baseline,
        } => vikram_bench::learning::learning(baseline.as_deref(), save_baseline.as_deref()),
        BenchAction::Personality {
            turns,
            text,
            threshold,
        } => vikram_bench::personality::personality(turns.as_deref(), &text, Some(threshold)),
    }
}
