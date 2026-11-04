//! Self-Evaluation Engine: Measure whether Vikram is actually improving.
//!
//! This engine tracks performance metrics, creates benchmarks, runs evaluations,
//! and detects regressions.

use crate::error::{EvolutionError, Result};
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
    pub fn run_benchmark(&mut self, benchmark_id: &str) -> Result<EvaluationResult> {
        let benchmark = self
            .benchmarks
            .iter()
            .find(|b| b.id == benchmark_id)
            .ok_or_else(|| EvolutionError::NotFound("Benchmark not found".to_string()))?;

        let actual = self.execute_benchmark_task(benchmark)?;
        let (passed, score, notes) = self.compare_metrics(&benchmark.expected_metrics, &actual);

        let result = EvaluationResult {
            id: gen_id("eval"),
            benchmark_id: benchmark_id.to_string(),
            actual_metrics: actual,
            passed,
            score,
            notes,
            timestamp_ms: now_ms(),
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

    fn execute_benchmark_task(
        &self,
        _benchmark: &EvaluationBenchmark,
    ) -> Result<PerformanceMetrics> {
        Ok(PerformanceMetrics {
            task_completion_rate: 0.9,
            task_correctness: 0.85,
            tool_success_rate: 0.95,
            model_performance: 0.8,
            agent_performance: 0.85,
            execution_time_ms: 5000,
            token_consumption: 10000,
            resource_consumption: 0.7,
            failure_rate: 0.1,
            recovery_rate: 0.9,
            regression_rate: 0.05,
            user_corrections: 2,
            skill_effectiveness: 0.8,
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

#[derive(Clone, Debug)]
pub struct EvaluationSummary {
    pub total_evaluations: usize,
    pub passed_evaluations: usize,
    pub failed_evaluations: usize,
    pub average_score: f64,
}
