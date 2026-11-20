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
}

pub fn run(action: BenchAction) -> Result<(), String> {
    match action {
        BenchAction::Learning {
            baseline,
            save_baseline,
        } => vikram_bench::learning::learning(baseline.as_deref(), save_baseline.as_deref()),
    }
}
