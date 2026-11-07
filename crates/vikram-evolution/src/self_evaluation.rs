//! Self-Evaluation Engine: Measure whether Vikram is actually improving.
//!
//! This engine tracks performance metrics, creates benchmarks, runs evaluations,
//! and detects regressions.
//!
//! A benchmark run does not execute a synthetic task: it evaluates the
//! *recorded experiences* for the benchmark's `task_type` over the recency
//! window (see [`crate::metrics`]). Each [`EvaluationResult`] therefore carries
//! the number of experiences it was computed from (`data_points`) and whether
//! that is enough to draw a conclusion (`sufficient_data`). Metrics the store
//! cannot answer — per-model/per-agent performance, resource consumption,
//! recovery/regression rates, skill effectiveness — are reported as `0.0` and
//! named in `notes` as *not measured*, never estimated.

use crate::error::{EvolutionError, Result};
use crate::metrics;
use crate::store::EvolutionStore;
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};

/// Performance metrics.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    pub task_completion_rate: f32,
    pub task_correctness: f32,
    pub tool_success_rate: f32,
    pub model_performance: f32,
    pub agent_performance: f32,
    pub execution_time_ms: u64,
    pub token_consumption: u64,
    pub resource_consumption: f32,
    pub failure_rate: f32,
    pub recovery_rate: f32,
    pub regression_rate: f32,
    pub user_corrections: u32,
    pub skill_effectiveness: f32,
}

/// Evaluation benchmark.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationBenchmark {
    pub id: String,
    pub name: String,
    pub description: String,
    pub task_type: String,
    pub expected_metrics: PerformanceMetrics,
    pub created_ms: u64,
}

/// Evaluation result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationResult {
    pub id: String,
    pub benchmark_id: String,
    pub actual_metrics: PerformanceMetrics,
    pub passed: bool,
    pub score: f64,
    pub notes: String,
    pub timestamp_ms: u64,
    /// How many recorded experiences the metrics were computed from.
    #[serde(default)]
    pub data_points: usize,
    /// `false` when [`Self::data_points`] is below [`metrics::MIN_SAMPLE`]:
    /// the store does not hold enough evidence for this benchmark, so
    /// `actual_metrics` is zero-filled rather than estimated and `notes`
    /// starts with `insufficient data`.
    #[serde(default)]
    pub sufficient_data: bool,
}

/// Regression detection result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegressionDetection {
    pub id: String,
    pub regression_detected: bool,
    pub regressed_metrics: Vec<String>,
    pub severity: RegressionSeverity,
    pub timestamp_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegressionSeverity {
    None,
    Minor,
    Moderate,
    Severe,
}

pub struct SelfEvaluationEngine {
    store: EvolutionStore,
    metrics_history: Vec<(u64, PerformanceMetrics)>,
    benchmarks: Vec<EvaluationBenchmark>,
    evaluation_results: Vec<EvaluationResult>,
}

impl SelfEvaluationEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            metrics_history: Vec::new(),
            benchmarks: Vec::new(),
            evaluation_results: Vec::new(),
        }
    }

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Track a performance metric.
    pub fn track_metric(&mut self, metrics: PerformanceMetrics) {
        self.metrics_history.push((now_ms(), metrics));
    }

    /// Create an evaluation benchmark.
    pub fn create_benchmark(
        &mut self,
        name: &str,
        description: &str,
        task_type: &str,
        expected: PerformanceMetrics,
    ) -> EvaluationBenchmark {
        let benchmark = EvaluationBenchmark {
            id: gen_id("bench"),
            name: name.to_string(),
            description: description.to_string(),
            task_type: task_type.to_string(),
            expected_metrics: expected,
            created_ms: now_ms(),
        };

        self.benchmarks.push(benchmark.clone());
        benchmark
    }

    /// Run a benchmark.
    ///
    /// The benchmark is evaluated against the experiences recorded for its
    /// `task_type`; nothing is executed to produce the numbers.
    pub fn run_benchmark(&mut self, benchmark_id: &str) -> Result<EvaluationResult> {
        let benchmark = self
            .benchmarks
            .iter()
            .find(|b| b.id == benchmark_id)
            .ok_or_else(|| EvolutionError::NotFound("Benchmark not found".to_string()))?;

        let sample = self.execute_benchmark_task(benchmark)?;
        let (passed, score, comparison_notes) =
            self.compare_metrics(&benchmark.expected_metrics, &sample.metrics);

        let notes = if sample.sufficient_data {
            let mut parts = sample.notes.clone();
            if !comparison_notes.is_empty() {
                parts.push(comparison_notes);
            }
            parts.join("; ")
        } else {
            let mut parts = vec![format!(
                "insufficient data: {} of {} required recorded experience(s) for task_type '{}'",
                sample.data_points,
                metrics::MIN_SAMPLE,
                benchmark.task_type
            )];
            parts.extend(sample.notes.clone());
            parts.push(
                "no metrics were estimated; zero values above mean 'not measured'".to_string(),
            );
            parts.join("; ")
        };

        let result = EvaluationResult {
            id: gen_id("eval"),
            benchmark_id: benchmark_id.to_string(),
            actual_metrics: sample.metrics,
            passed: sample.sufficient_data && passed,
            score,
            notes,
            timestamp_ms: now_ms(),
            data_points: sample.data_points,
            sufficient_data: sample.sufficient_data,
        };

        self.evaluation_results.push(result.clone());
        Ok(result)
    }

    /// Compare performance before and after a change.
    pub fn compare_performance(
        &self,
        before_ms: u64,
        after_ms: u64,
    ) -> Result<PerformanceComparison> {
        let before = self
            .metrics_history
            .iter()
            .find(|(ts, _)| *ts == before_ms)
            .map(|(_, m)| m.clone());
        let after = self
            .metrics_history
            .iter()
            .find(|(ts, _)| *ts == after_ms)
            .map(|(_, m)| m.clone());

        let improvement_detected = self.detect_improvement(before.as_ref(), after.as_ref());

        Ok(PerformanceComparison {
            before,
            after,
            improvement_detected,
        })
    }

    /// Detect regressions in metrics.
    pub fn detect_regression(&self) -> Result<RegressionDetection> {
        if self.metrics_history.len() < 2 {
            return Ok(RegressionDetection {
                id: gen_id("regression"),
                regression_detected: false,
                regressed_metrics: vec![],
                severity: RegressionSeverity::None,
                timestamp_ms: now_ms(),
            });
        }

        let latest = &self.metrics_history.last().unwrap().1;
        let previous = &self.metrics_history[self.metrics_history.len() - 2].1;

        let mut regressed = Vec::new();

        if latest.task_completion_rate < previous.task_completion_rate - 0.1 {
            regressed.push("task_completion_rate".to_string());
        }
        if latest.task_correctness < previous.task_correctness - 0.1 {
            regressed.push("task_correctness".to_string());
        }
        if latest.tool_success_rate < previous.tool_success_rate - 0.1 {
            regressed.push("tool_success_rate".to_string());
        }

        let severity = if regressed.is_empty() {
            RegressionSeverity::None
        } else if regressed.len() == 1 {
            RegressionSeverity::Minor
        } else if regressed.len() <= 3 {
            RegressionSeverity::Moderate
        } else {
            RegressionSeverity::Severe
        };

        Ok(RegressionDetection {
            id: gen_id("regression"),
            regression_detected: !regressed.is_empty(),
            regressed_metrics: regressed,
            severity,
            timestamp_ms: now_ms(),
        })
    }

    /// Aggregate evaluation results.
    pub fn aggregate_results(&self) -> Result<EvaluationSummary> {
        let total = self.evaluation_results.len();
        let passed = self.evaluation_results.iter().filter(|r| r.passed).count();
        let avg_score = if total > 0 {
            self.evaluation_results.iter().map(|r| r.score).sum::<f64>() / total as f64
        } else {
            0.0
        };

        Ok(EvaluationSummary {
            total_evaluations: total,
            passed_evaluations: passed,
            failed_evaluations: total - passed,
            average_score: avg_score,
        })
    }

    /// Derive this benchmark's metrics from the experiences recorded for its
    /// `task_type` in the store.
    ///
    /// Mapping (each value is a real aggregate, none is a constant):
    /// - `task_completion_rate` — share of sampled experiences that did not
    ///   fail outright (`total - failure`);
    /// - `task_correctness` — share that succeeded outright (an experience
    ///   stores an outcome, not a graded answer);
    /// - `tool_success_rate` — share that recorded no error;
    /// - `failure_rate` — share that failed;
    /// - `user_corrections` — experiences that carried a user correction;
    /// - `execution_time_ms` / `token_consumption` — means of the
    ///   `metadata.duration_ms` / `metadata.tokens_used` keys producers may
    ///   record (`0` when nothing recorded them).
    ///
    /// Everything else is *not measured*: it is reported as `0.0` and named in
    /// the `not measured from store data` note by
    /// [`NOT_MEASURED_FIELDS`].
    fn execute_benchmark_task(&self, benchmark: &EvaluationBenchmark) -> Result<BenchmarkSample> {
        let stats = self.store.experience_stats_for(
            Some(benchmark.task_type.as_str()),
            metrics::DEFAULT_WINDOW_MS,
        )?;
        // Counts and averages always come from the *same* window: the recent
        // one when it holds enough samples, else the whole recorded history.
        let (counts, avg_duration_ms, avg_tokens, window_label) =
            if stats.recent.total >= metrics::MIN_SAMPLE {
                (
                    stats.recent,
                    stats.recent_avg_duration_ms,
                    stats.recent_avg_tokens,
                    format!("the last {}h", metrics::DEFAULT_WINDOW_MS / 3_600_000),
                )
            } else {
                (
                    stats.all_time,
                    stats.all_time_avg_duration_ms,
                    stats.all_time_avg_tokens,
                    "all recorded history".to_string(),
                )
            };
        let data_points = counts.total;
        let sufficient_data = data_points >= metrics::MIN_SAMPLE;

        let mut notes = Vec::new();
        if data_points > 0 {
            notes.push(format!(
                "{data_points} recorded experience(s) for task_type '{}' from {window_label}",
                benchmark.task_type
            ));
        }
        if avg_duration_ms.is_none() {
            notes.push(
                "execution_time_ms not measured: no experience recorded metadata.duration_ms"
                    .to_string(),
            );
        }
        if avg_tokens.is_none() {
            notes.push(
                "token_consumption not measured: no experience recorded metadata.tokens_used"
                    .to_string(),
            );
        }
        notes.push(format!(
            "not measured from store data (reported as 0.0): {}",
            NOT_MEASURED_FIELDS.join(", ")
        ));

        let metrics = PerformanceMetrics {
            task_completion_rate: rate(counts.non_failure(), counts.total),
            task_correctness: rate(counts.success, counts.total),
            tool_success_rate: rate(
                counts.total.saturating_sub(counts.with_errors),
                counts.total,
            ),
            model_performance: 0.0,
            agent_performance: 0.0,
            execution_time_ms: avg_duration_ms.unwrap_or(0),
            token_consumption: avg_tokens.unwrap_or(0),
            resource_consumption: 0.0,
            failure_rate: rate(counts.failure, counts.total),
            recovery_rate: 0.0,
            regression_rate: 0.0,
            user_corrections: counts.with_corrections as u32,
            skill_effectiveness: 0.0,
        };

        Ok(BenchmarkSample {
            metrics,
            data_points,
            sufficient_data,
            notes,
        })
    }

    fn compare_metrics(
        &self,
        expected: &PerformanceMetrics,
        actual: &PerformanceMetrics,
    ) -> (bool, f64, String) {
        let mut score = 0.0;
        let mut notes = Vec::new();

        if actual.task_completion_rate >= expected.task_completion_rate {
            score += 0.2;
        } else {
            notes.push(format!(
                "Task completion rate below expected: {} < {}",
                actual.task_completion_rate, expected.task_completion_rate
            ));
        }

        if actual.task_correctness >= expected.task_correctness {
            score += 0.2;
        } else {
            notes.push(format!(
                "Task correctness below expected: {} < {}",
                actual.task_correctness, expected.task_correctness
            ));
        }

        if actual.tool_success_rate >= expected.tool_success_rate {
            score += 0.2;
        } else {
            notes.push(format!(
                "Tool success rate below expected: {} < {}",
                actual.tool_success_rate, expected.tool_success_rate
            ));
        }

        if actual.execution_time_ms <= expected.execution_time_ms {
            score += 0.2;
        } else {
            notes.push(format!(
                "Execution time exceeded: {}ms > {}ms",
                actual.execution_time_ms, expected.execution_time_ms
            ));
        }

        if actual.token_consumption <= expected.token_consumption {
            score += 0.2;
        } else {
            notes.push(format!(
                "Token consumption exceeded: {} > {}",
                actual.token_consumption, expected.token_consumption
            ));
        }

        let passed = score >= 0.6;
        (passed, score, notes.join("; "))
    }

    fn detect_improvement(
        &self,
        before: Option<&PerformanceMetrics>,
        after: Option<&PerformanceMetrics>,
    ) -> bool {
        match (before, after) {
            (Some(b), Some(a)) => {
                a.task_completion_rate > b.task_completion_rate
                    || a.task_correctness > b.task_correctness
                    || a.execution_time_ms < b.execution_time_ms
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PerformanceComparison {
    pub before: Option<PerformanceMetrics>,
    pub after: Option<PerformanceMetrics>,
    pub improvement_detected: bool,
}

/// `PerformanceMetrics` fields with no store signal behind them.
///
/// Listing them is the point: a benchmark run reports these as `0.0` and names
/// them in `notes`, instead of filling them with a plausible number.
const NOT_MEASURED_FIELDS: &[&str] = &[
    "model_performance",
    "agent_performance",
    "resource_consumption",
    "recovery_rate",
    "regression_rate",
    "skill_effectiveness",
];

/// Outcome of evaluating one benchmark against recorded experiences.
#[derive(Clone, Debug)]
struct BenchmarkSample {
    metrics: PerformanceMetrics,
    /// Number of recorded experiences the metrics were computed from.
    data_points: usize,
    /// Whether `data_points` reaches [`metrics::MIN_SAMPLE`].
    sufficient_data: bool,
    /// What was measured, what was not, and from which rows.
    notes: Vec<String>,
}

/// `n / total`, or `0.0` when there is no sample at all (the caller says so
/// via [`BenchmarkSample::sufficient_data`]).
fn rate(n: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        n as f32 / total as f32
    }
}

#[derive(Clone, Debug)]
pub struct EvaluationSummary {
    pub total_evaluations: usize,
    pub passed_evaluations: usize,
    pub failed_evaluations: usize,
    pub average_score: f64,
}
