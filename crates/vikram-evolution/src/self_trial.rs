//! Measured self-configuration: apply one change, measure it once, keep it only
//! when it did not get worse.
//!
//! [`crate::self_config`] can write a live key and [`crate::self_evaluation`]
//! can measure, but neither answers "was this change worth it". That joint
//! question is here: read the newest stored metric as the baseline, apply, run
//! the benchmark once, then keep the new value or restore the old one.
//!
//! A trial always ends in exactly one of two states — the old value (rolled
//! back) or the new one (kept) — and is written to `engine_records` under
//! [`TRIAL_NAMESPACE`], so [`trial_history`] can say after a restart what was
//! tried and what it measured.
//!
//! Nothing here invents a candidate change: the caller supplies the target, the
//! value and the benchmark.

use crate::error::{EvolutionError, Result};
use crate::self_config::{ConfigTarget, SelfConfigurationEngine};
use crate::self_evaluation::{PerformanceMetrics, SelfEvaluationEngine};
use crate::store::{EvolutionStore, from_json_value, to_json_value};
use serde::{Deserialize, Serialize};

/// `engine_records` namespace holding every [`TrialOutcome`].
pub const TRIAL_NAMESPACE: &str = "self_trial";

/// The namespace `self_evaluation` persists its metric history under. A trial
/// reads the newest entry instead of running a benchmark just for the baseline.
const METRIC_NAMESPACE: &str = "self_evaluation.metric";

/// What one measured change did, and what it was measured from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrialOutcome {
    pub change_id: String,
    /// The surface key that was trialled, e.g. `context:memory_budget_tokens`.
    pub target: String,
    /// Whether the new value is live when the trial returns.
    pub applied: bool,
    /// Whether the old value was restored because the measurement did not justify the change.
    pub rolled_back: bool,
    /// The newest stored metric read before the change; `None` when none existed.
    pub before: Option<PerformanceMetrics>,
    /// The post-change benchmark reading; `None` when it could not be measured.
    pub after: Option<PerformanceMetrics>,
    pub reason: String,
}

/// Try `value` for `target`, measure it with `benchmark_id`, and keep it only
/// when the benchmark did not get worse.
///
/// Refusals (no surface, a vetoed key, an unknown or unchanged current value)
/// are errors and write nothing. A completed measurement is an `Ok` outcome
/// even when the change was rolled back: a rejected change is a result, not a
/// failure.
pub fn trial_change(
    config: &mut SelfConfigurationEngine,
    evaluation: &mut SelfEvaluationEngine,
    target: ConfigTarget,
    value: &str,
    benchmark_id: &str,
) -> Result<TrialOutcome> {
    let key = target.key();
    let surface = config.surface().ok_or_else(|| {
        EvolutionError::Validation(format!(
            "cannot trial {key}: this engine has no live configuration surface attached, \
             so there is nothing to write and nothing to measure"
        ))
    })?;
    if !surface.writable(&key) {
        return Err(EvolutionError::Validation(format!(
            "the live surface refuses {key}: a learning engine may not change \
             what the assistant is allowed to do"
        )));
    }
    let previous = surface.read(&key).ok_or_else(|| {
        EvolutionError::Validation(format!(
            "the live surface reports no current value for {key}, so a rollback could not \
             restore one; refusing to start a trial that cannot be undone"
        ))
    })?;
    if previous == value {
        return Err(EvolutionError::Validation(format!(
            "{key} already holds `{value}`; there is nothing to measure, so nothing was written"
        )));
    }

    let before = latest_metric(evaluation.store())?;
    let change = config.record_target_change(
        target,
        value,
        &format!("measured trial against benchmark {benchmark_id}"),
    )?;
    let change_id = change.id;
    config.apply_change(&change_id)?;

    let mut after = None;
    let (reason, rolled_back) = match evaluation.run_benchmark(benchmark_id) {
        Err(e) => (
            format!(
                "rolled back {key} to `{previous}`: benchmark {benchmark_id} failed after the \
                 change ({e}); an unmeasured change is not kept"
            ),
            true,
        ),
        Ok(result) if !result.sufficient_data => (
            format!(
                "rolled back {key} to `{previous}`: the post-change benchmark could not be \
                 measured ({}); an unmeasurable change is not kept",
                result.notes
            ),
            true,
        ),
        Ok(result) => {
            after = Some(result.actual_metrics.clone());
            match before.as_ref() {
                None => (
                    format!(
                        "rolled back {key} to `{previous}`: no earlier benchmark metric was on \
                         record, so the comparison is inconclusive; an unmeasurable change is \
                         not kept"
                    ),
                    true,
                ),
                Some(b) => match compare(b, &result.actual_metrics) {
                    (Verdict::Worse, detail) => (
                        format!(
                            "rolled back {key} to `{previous}`: benchmark {benchmark_id} got \
                             worse ({detail})"
                        ),
                        true,
                    ),
                    (Verdict::Better, detail) => (
                        format!(
                            "kept {key} = `{value}`: benchmark {benchmark_id} improved ({detail})"
                        ),
                        false,
                    ),
                    (Verdict::Equal, detail) => (
                        format!(
                            "kept {key} = `{value}`: benchmark {benchmark_id} did not get worse \
                             ({detail})"
                        ),
                        false,
                    ),
                },
            }
        }
    };

    if rolled_back {
        config.rollback_change(&change_id)?;
    }

    let outcome = TrialOutcome {
        change_id,
        target: key,
        applied: !rolled_back,
        rolled_back,
        before,
        after,
        reason,
    };
    config.store().save_engine_record(
        TRIAL_NAMESPACE,
        &outcome.change_id,
        &to_json_value(&outcome)?,
    )?;
    Ok(outcome)
}

/// Every durable trial record, oldest first. This is what a restarted process
/// reads to see what was tried and how it measured.
pub fn trial_history(store: &EvolutionStore) -> Result<Vec<TrialOutcome>> {
    store
        .list_engine_records(TRIAL_NAMESPACE)?
        .into_iter()
        .map(|(_, payload)| from_json_value::<TrialOutcome>(payload))
        .collect()
}

fn latest_metric(store: &EvolutionStore) -> Result<Option<PerformanceMetrics>> {
    let mut newest: Option<(u64, PerformanceMetrics)> = None;
    for (_, payload) in store.list_engine_records(METRIC_NAMESPACE)? {
        let (timestamp_ms, metrics): (u64, PerformanceMetrics) = from_json_value(payload)?;
        if newest
            .as_ref()
            .is_none_or(|(seen, _)| timestamp_ms >= *seen)
        {
            newest = Some((timestamp_ms, metrics));
        }
    }
    Ok(newest.map(|(_, metrics)| metrics))
}

#[derive(Clone, Copy)]
enum Verdict {
    Better,
    Equal,
    Worse,
}

/// A regression on any measured dimension is a worse result: with one reading
/// per side there is no evidence that a drop is noise, so it is not kept.
fn compare(before: &PerformanceMetrics, after: &PerformanceMetrics) -> (Verdict, String) {
    let mut improved = Vec::new();
    let mut worsened = Vec::new();

    push_delta(
        "task_completion_rate",
        before.task_completion_rate,
        after.task_completion_rate,
        true,
        &mut improved,
        &mut worsened,
    );
    push_delta(
        "task_correctness",
        before.task_correctness,
        after.task_correctness,
        true,
        &mut improved,
        &mut worsened,
    );
    push_delta(
        "tool_success_rate",
        before.tool_success_rate,
        after.tool_success_rate,
        true,
        &mut improved,
        &mut worsened,
    );
    push_delta(
        "failure_rate",
        before.failure_rate,
        after.failure_rate,
        false,
        &mut improved,
        &mut worsened,
    );
    if before.execution_time_ms > 0 && after.execution_time_ms > 0 {
        push_delta(
            "execution_time_ms",
            before.execution_time_ms as f32,
            after.execution_time_ms as f32,
            false,
            &mut improved,
            &mut worsened,
        );
    }
    if before.token_consumption > 0 && after.token_consumption > 0 {
        push_delta(
            "token_consumption",
            before.token_consumption as f32,
            after.token_consumption as f32,
            false,
            &mut improved,
            &mut worsened,
        );
    }

    if !worsened.is_empty() {
        (Verdict::Worse, worsened.join(", "))
    } else if !improved.is_empty() {
        (Verdict::Better, improved.join(", "))
    } else {
        (Verdict::Equal, "no measured metric moved".to_string())
    }
}

fn push_delta(
    name: &str,
    before: f32,
    after: f32,
    higher_is_better: bool,
    improved: &mut Vec<String>,
    worsened: &mut Vec<String>,
) {
    if (after - before).abs() <= f32::EPSILON {
        return;
    }
    let line = format!("{name} {before:.2} -> {after:.2}");
    if (after > before) == higher_is_better {
        improved.push(line);
    } else {
        worsened.push(line);
    }
}
