//! Store-backed metrics: what the self-* engines are allowed to believe.
//!
//! Every value produced here is an aggregate over rows that actually exist in
//! the [`EvolutionStore`]. Nothing is synthesised: when the store holds no
//! evidence for a metric the aggregate is zero and the derived rate is `None`
//! (never a plausible-looking default), so a caller can distinguish
//! "0 % failures out of 0 samples" from "0 % failures out of 100 samples" and
//! must say so instead of reporting health it cannot verify.
//!
//! The aggregations are plain SQL (`COUNT`/`SUM`/`AVG`) run through
//! [`EvolutionStore::with_conn`]; no rows are filtered, rounded or dropped in
//! Rust beyond turning an `AVG` into a whole millisecond count.
//!
//! # What the store can and cannot answer
//!
//! | metric | source |
//! |---|---|
//! | outcomes, errors, corrections, reusable | `experiences.outcome/errors/corrections/reusable` |
//! | durations | `experiences.metadata.duration_ms` (optional producer key) |
//! | token usage | `experiences.metadata.tokens_used` (optional producer key) |
//! | reflection latency | `reflection_jobs.updated_ms - created_ms` for `done` jobs |
//! | proposal / job states | `proposals.status`, `reflection_jobs.status` |
//! | store footprint | `PRAGMA page_count * page_size` |
//!
//! Producers are *not required* to record `duration_ms`/`tokens_used`; when
//! they do not, the corresponding average is `None` and every engine that
//! would have used it reports "not recorded" instead of guessing.

use crate::error::{EvolutionError, Result};
use crate::store::EvolutionStore;
use crate::types::{Experience, ProposalStatus, ReflectionJobStatus, now_ms};
use serde::{Deserialize, Serialize};

/// Recency window the engines reason over by default (24 hours).
pub const DEFAULT_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;

/// Minimum number of sampled experiences before a rate is treated as
/// evidence. Below this the engines report "unverified"/"insufficient data"
/// rather than a percentage computed from one or two data points.
pub const MIN_SAMPLE: usize = 5;

/// Producer-provided `metadata` key holding an experience's wall-clock
/// duration in milliseconds.
pub const META_DURATION_MS: &str = "duration_ms";

/// Producer-provided `metadata` key holding an experience's token usage.
pub const META_TOKENS_USED: &str = "tokens_used";

// ---------- experience outcomes ----------

/// Outcome tallies for a set of experiences.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeCounts {
    pub total: usize,
    pub success: usize,
    pub failure: usize,
    pub partial: usize,
    /// Experiences that recorded at least one error (may overlap `failure`
    /// and `partial`).
    pub with_errors: usize,
    /// Experiences that recorded at least one user correction.
    pub with_corrections: usize,
    /// Experiences flagged reusable.
    pub reusable: usize,
    /// Experiences that failed *or* recorded an error. Counted as a union in
    /// SQL, so a failing experience that also logged an error counts once.
    pub problems: usize,
}

impl OutcomeCounts {
    /// Share of experiences that succeeded outright, or `None` when there is
    /// no sample.
    pub fn success_rate(&self) -> Option<f32> {
        self.rate(self.success)
    }
    /// Share of experiences that failed outright, or `None` when there is no
    /// sample.
    pub fn failure_rate(&self) -> Option<f32> {
        self.rate(self.failure)
    }
    /// Share of experiences that recorded at least one error, or `None`.
    pub fn error_rate(&self) -> Option<f32> {
        self.rate(self.with_errors)
    }
    /// Experiences that did not fail outright (`total - failure`).
    pub fn non_failure(&self) -> usize {
        self.total.saturating_sub(self.failure)
    }
    fn rate(&self, n: usize) -> Option<f32> {
        if self.total == 0 {
            None
        } else {
            Some(n as f32 / self.total as f32)
        }
    }
}

/// Aggregated experience statistics, all time and for one recency window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExperienceStats {
    /// Window (ms) `recent` and the `recent_*` averages were computed over.
    pub window_ms: u64,
    pub all_time: OutcomeCounts,
    pub recent: OutcomeCounts,
    /// Mean of `metadata.duration_ms` over experiences that recorded it.
    pub all_time_avg_duration_ms: Option<u64>,
    pub recent_avg_duration_ms: Option<u64>,
    /// Mean of `metadata.tokens_used` over experiences that recorded it.
    pub all_time_avg_tokens: Option<u64>,
    pub recent_avg_tokens: Option<u64>,
    pub first_timestamp_ms: Option<u64>,
    pub last_timestamp_ms: Option<u64>,
}

impl ExperienceStats {
    /// The tally a caller may treat as evidence: the recent window when it
    /// holds at least [`MIN_SAMPLE`] experiences, otherwise all time when it
    /// is large enough, otherwise `None` (no sample to reason from).
    pub fn authoritative_sample(&self) -> Option<OutcomeCounts> {
        if self.recent.total >= MIN_SAMPLE {
            Some(self.recent)
        } else if self.all_time.total >= MIN_SAMPLE {
            Some(self.all_time)
        } else {
            None
        }
    }

    /// Recent mean duration when recorded, else the all-time mean.
    pub fn avg_duration_ms(&self) -> Option<u64> {
        self.recent_avg_duration_ms
            .or(self.all_time_avg_duration_ms)
    }

    /// Recent mean token usage when recorded, else the all-time mean.
    pub fn avg_tokens(&self) -> Option<u64> {
        self.recent_avg_tokens.or(self.all_time_avg_tokens)
    }

    /// True when the store holds no experiences at all.
    pub fn is_empty(&self) -> bool {
        self.all_time.total == 0
    }
}

/// Per-`task_category` tallies, used to locate failures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryStats {
    pub category: String,
    pub total: usize,
    pub failures: usize,
    pub errors: usize,
    pub corrections: usize,
    /// Experiences that failed *or* recorded an error. Counted as a union in
    /// SQL, so a failing experience that also logged an error counts once.
    pub problems: usize,
}

impl CategoryStats {
    /// Share of this category's experiences that failed outright.
    pub fn failure_rate(&self) -> Option<f32> {
        if self.total == 0 {
            None
        } else {
            Some(self.failures as f32 / self.total as f32)
        }
    }

    /// Share of this category's experiences that failed or errored.
    pub fn problem_rate(&self) -> Option<f32> {
        if self.total == 0 {
            None
        } else {
            Some(self.problems as f32 / self.total as f32)
        }
    }
}

// ---------- proposals ----------

/// Proposal tallies by status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalStats {
    pub total: usize,
    pub pending: usize,
    pub approved: usize,
    pub rejected: usize,
    pub applied: usize,
    /// Pending proposals of kind `record_issue`: failures the learning loop
    /// has already noticed and nobody has dealt with yet.
    pub pending_record_issue: usize,
}

impl ProposalStats {
    /// Share of decided proposals (`approved + applied + rejected`) that were
    /// rejected, or `None` when nothing has been decided.
    pub fn rejection_rate(&self) -> Option<f32> {
        let decided = self.approved + self.applied + self.rejected;
        if decided == 0 {
            None
        } else {
            Some(self.rejected as f32 / decided as f32)
        }
    }
}

// ---------- reflection jobs ----------

/// Reflection-job tallies by status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflectionStats {
    pub total: usize,
    pub pending: usize,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
    /// Mean `updated_ms - created_ms` over `done` jobs, when any exist.
    pub avg_latency_ms: Option<u64>,
}

impl ReflectionStats {
    /// Jobs accepted but not finished (`pending + running`).
    pub fn backlog(&self) -> usize {
        self.pending + self.running
    }

    /// Share of finished jobs (`done + failed`) that failed, or `None`.
    pub fn failure_rate(&self) -> Option<f32> {
        let finished = self.done + self.failed;
        if finished == 0 {
            None
        } else {
            Some(self.failed as f32 / finished as f32)
        }
    }
}

// ---------- per-experience helpers ----------

/// Duration the producer recorded in `metadata.duration_ms`, if it did.
pub fn recorded_duration_ms(exp: &Experience) -> Option<u64> {
    exp.metadata
        .get(META_DURATION_MS)
        .and_then(serde_json::Value::as_u64)
}

/// Token usage the producer recorded in `metadata.tokens_used`, if it did.
pub fn recorded_tokens_used(exp: &Experience) -> Option<u64> {
    exp.metadata
        .get(META_TOKENS_USED)
        .and_then(serde_json::Value::as_u64)
}

fn store_err(e: rusqlite::Error) -> EvolutionError {
    EvolutionError::Store(e.to_string())
}

fn positive(avg: Option<f64>) -> Option<u64> {
    avg.filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v.round() as u64)
}

// ---------- aggregates, implemented on the store ----------

impl EvolutionStore {
    /// Outcome tallies, optionally restricted to one `task_category` and to
    /// experiences at or after `since_ms`.
    pub fn outcome_counts(
        &self,
        category: Option<&str>,
        since_ms: Option<u64>,
    ) -> Result<OutcomeCounts> {
        let since = since_ms.map(|ms| ms as i64);
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT COUNT(*),
                            COALESCE(SUM(outcome = 'success'), 0),
                            COALESCE(SUM(outcome = 'failure'), 0),
                            COALESCE(SUM(outcome = 'partial'), 0),
                            COALESCE(SUM(errors <> '[]'), 0),
                            COALESCE(SUM(corrections <> '[]'), 0),
                            COALESCE(SUM(reusable <> 0), 0),
                            COALESCE(SUM(outcome = 'failure' OR errors <> '[]'), 0)
                     FROM experiences
                     WHERE (?1 IS NULL OR task_category = ?1)
                       AND (?2 IS NULL OR timestamp_ms >= ?2)",
                )
                .map_err(store_err)?;
            let counts = stmt
                .query_row(rusqlite::params![category, since], |r| {
                    Ok(OutcomeCounts {
                        total: r.get::<_, i64>(0)? as usize,
                        success: r.get::<_, i64>(1)? as usize,
                        failure: r.get::<_, i64>(2)? as usize,
                        partial: r.get::<_, i64>(3)? as usize,
                        with_errors: r.get::<_, i64>(4)? as usize,
                        with_corrections: r.get::<_, i64>(5)? as usize,
                        reusable: r.get::<_, i64>(6)? as usize,
                        problems: r.get::<_, i64>(7)? as usize,
                    })
                })
                .map_err(store_err)?;
            Ok(counts)
        })
    }

    /// Mean of a numeric `metadata` key over the experiences that recorded it.
    ///
    /// `key` is bound as a JSON path (`$.<key>`), never interpolated, and rows
    /// whose metadata is not valid JSON or whose value is not numeric are
    /// skipped by SQLite rather than guessed at. Returns `None` when no
    /// experience recorded the key.
    pub fn avg_recorded_metric(
        &self,
        key: &str,
        category: Option<&str>,
        since_ms: Option<u64>,
    ) -> Result<Option<u64>> {
        let path = format!("$.{key}");
        let since = since_ms.map(|ms| ms as i64);
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT COUNT(m), AVG(m) FROM (
                         SELECT CASE WHEN json_valid(metadata)
                                     THEN json_extract(metadata, ?1)
                                END AS m
                         FROM experiences
                         WHERE (?2 IS NULL OR task_category = ?2)
                           AND (?3 IS NULL OR timestamp_ms >= ?3)
                     )
                     WHERE typeof(m) IN ('integer', 'real')",
                )
                .map_err(store_err)?;
            let (count, avg): (i64, Option<f64>) = stmt
                .query_row(rusqlite::params![path, category, since], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .map_err(store_err)?;
            if count == 0 {
                Ok(None)
            } else {
                Ok(positive(avg))
            }
        })
    }

    /// Experience statistics for one `task_category` (or all of them) over a
    /// recency window of `window_ms`.
    pub fn experience_stats_for(
        &self,
        category: Option<&str>,
        window_ms: u64,
    ) -> Result<ExperienceStats> {
        let now = now_ms();
        let since = now.saturating_sub(window_ms);
        let all_time = self.outcome_counts(category, None)?;
        let recent = self.outcome_counts(category, Some(since))?;
        let (first_timestamp_ms, last_timestamp_ms) = self.experience_time_bounds()?;
        Ok(ExperienceStats {
            window_ms,
            all_time,
            recent,
            all_time_avg_duration_ms: self.avg_recorded_metric(META_DURATION_MS, category, None)?,
            recent_avg_duration_ms: self.avg_recorded_metric(
                META_DURATION_MS,
                category,
                Some(since),
            )?,
            all_time_avg_tokens: self.avg_recorded_metric(META_TOKENS_USED, category, None)?,
            recent_avg_tokens: self.avg_recorded_metric(META_TOKENS_USED, category, Some(since))?,
            first_timestamp_ms,
            last_timestamp_ms,
        })
    }

    /// Experience statistics over all categories for a recency window.
    pub fn experience_stats(&self, window_ms: u64) -> Result<ExperienceStats> {
        self.experience_stats_for(None, window_ms)
    }

    /// Oldest and newest recorded experience timestamps, if any exist.
    pub fn experience_time_bounds(&self) -> Result<(Option<u64>, Option<u64>)> {
        self.with_conn(|conn| {
            let (min, max): (Option<i64>, Option<i64>) = conn
                .query_row(
                    "SELECT MIN(timestamp_ms), MAX(timestamp_ms) FROM experiences",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(store_err)?;
            Ok((min.map(|v| v.max(0) as u64), max.map(|v| v.max(0) as u64)))
        })
    }

    /// Per-`task_category` tallies, worst first (failures + errors, then
    /// volume), restricted to `since_ms` when given.
    pub fn category_stats(
        &self,
        since_ms: Option<u64>,
        limit: usize,
    ) -> Result<Vec<CategoryStats>> {
        let since = since_ms.map(|ms| ms as i64);
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT task_category,
                            COUNT(*),
                            COALESCE(SUM(outcome = 'failure'), 0),
                            COALESCE(SUM(errors <> '[]'), 0),
                            COALESCE(SUM(corrections <> '[]'), 0),
                            COALESCE(SUM(outcome = 'failure' OR errors <> '[]'), 0)
                     FROM experiences
                     WHERE (?1 IS NULL OR timestamp_ms >= ?1)
                     GROUP BY task_category
                     ORDER BY COALESCE(SUM(outcome = 'failure' OR errors <> '[]'), 0) DESC,
                              COUNT(*) DESC,
                              task_category ASC
                     LIMIT ?2",
                )
                .map_err(store_err)?;
            let rows = stmt
                .query_map(rusqlite::params![since, limit as i64], |r| {
                    Ok(CategoryStats {
                        category: r.get(0)?,
                        total: r.get::<_, i64>(1)? as usize,
                        failures: r.get::<_, i64>(2)? as usize,
                        errors: r.get::<_, i64>(3)? as usize,
                        corrections: r.get::<_, i64>(4)? as usize,
                        problems: r.get::<_, i64>(5)? as usize,
                    })
                })
                .map_err(store_err)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(store_err)?);
            }
            Ok(out)
        })
    }

    /// Proposal tallies by status, plus the number of unresolved
    /// `record_issue` proposals.
    pub fn proposal_stats(&self) -> Result<ProposalStats> {
        self.with_conn(|conn| {
            let mut stats = ProposalStats::default();
            let mut stmt = conn
                .prepare("SELECT status, COUNT(*) FROM proposals GROUP BY status")
                .map_err(store_err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(store_err)?;
            for row in rows {
                let (status_s, n) = row.map_err(store_err)?;
                let n = n as usize;
                let status = ProposalStatus::parse(&status_s).ok_or_else(|| {
                    EvolutionError::Store(format!("unknown proposal status: {status_s}"))
                })?;
                match status {
                    ProposalStatus::Pending => stats.pending += n,
                    ProposalStatus::Approved => stats.approved += n,
                    ProposalStatus::Rejected => stats.rejected += n,
                    ProposalStatus::Applied => stats.applied += n,
                }
                stats.total += n;
            }
            let pending_record_issue: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM proposals WHERE status = 'pending' AND kind = 'record_issue'",
                    [],
                    |r| r.get(0),
                )
                .map_err(store_err)?;
            stats.pending_record_issue = pending_record_issue as usize;
            Ok(stats)
        })
    }

    /// Reflection-job tallies by status plus the mean latency of `done` jobs.
    pub fn reflection_stats(&self) -> Result<ReflectionStats> {
        self.with_conn(|conn| {
            let mut stats = ReflectionStats::default();
            let mut stmt = conn
                .prepare("SELECT status, COUNT(*) FROM reflection_jobs GROUP BY status")
                .map_err(store_err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(store_err)?;
            for row in rows {
                let (status_s, n) = row.map_err(store_err)?;
                let n = n as usize;
                let status = ReflectionJobStatus::parse(&status_s).ok_or_else(|| {
                    EvolutionError::Store(format!("unknown reflection job status: {status_s}"))
                })?;
                match status {
                    ReflectionJobStatus::Pending => stats.pending += n,
                    ReflectionJobStatus::Running => stats.running += n,
                    ReflectionJobStatus::Done => stats.done += n,
                    ReflectionJobStatus::Failed => stats.failed += n,
                }
                stats.total += n;
            }
            let avg_latency: Option<f64> = conn
                .query_row(
                    "SELECT AVG(updated_ms - created_ms) FROM reflection_jobs
                     WHERE status = 'done' AND updated_ms >= created_ms",
                    [],
                    |r| r.get(0),
                )
                .map_err(store_err)?;
            stats.avg_latency_ms = positive(avg_latency);
            Ok(stats)
        })
    }

    /// Bytes the SQLite database currently occupies (`page_count * page_size`).
    pub fn store_footprint_bytes(&self) -> Result<u64> {
        self.with_conn(|conn| {
            let page_count: i64 = conn
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .map_err(store_err)?;
            let page_size: i64 = conn
                .query_row("PRAGMA page_size", [], |r| r.get(0))
                .map_err(store_err)?;
            Ok((page_count.max(0) * page_size.max(0)) as u64)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Experience, ExperienceOutcome, Sensitivity};

    fn exp(
        id: &str,
        outcome: ExperienceOutcome,
        category: &str,
        errors: Vec<String>,
        metadata: serde_json::Value,
    ) -> Experience {
        Experience {
            id: id.to_string(),
            timestamp_ms: now_ms(),
            agent_id: "a".to_string(),
            project_id: None,
            task_category: category.to_string(),
            user_request: "req".to_string(),
            outcome,
            tools_used: vec![],
            errors,
            corrections: vec![],
            feedback: None,
            reusable: false,
            sensitivity: Sensitivity::Public,
            metadata,
        }
    }

    #[test]
    fn empty_store_reports_no_rates() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let stats = store.experience_stats(DEFAULT_WINDOW_MS).unwrap();
        assert!(stats.is_empty());
        assert_eq!(stats.all_time.failure_rate(), None);
        assert_eq!(stats.authoritative_sample(), None);
        assert_eq!(stats.avg_duration_ms(), None);
        assert_eq!(store.category_stats(None, 10).unwrap(), vec![]);
        assert_eq!(store.proposal_stats().unwrap(), ProposalStats::default());
        assert_eq!(
            store.reflection_stats().unwrap(),
            ReflectionStats::default()
        );
    }

    #[test]
    fn outcome_counts_and_metadata_averages_come_from_rows() {
        let store = EvolutionStore::open_in_memory().unwrap();
        store
            .save_experience(&exp(
                "e1",
                ExperienceOutcome::Success,
                "coding",
                vec![],
                serde_json::json!({"duration_ms": 1000, "tokens_used": 100}),
            ))
            .unwrap();
        store
            .save_experience(&exp(
                "e2",
                ExperienceOutcome::Failure,
                "coding",
                vec!["boom".to_string()],
                serde_json::json!({"duration_ms": 3000, "tokens_used": 300}),
            ))
            .unwrap();
        // No producer metrics recorded on this one: must not dilute the
        // averages with a fabricated zero.
        store
            .save_experience(&exp(
                "e3",
                ExperienceOutcome::Partial,
                "shell",
                vec![],
                serde_json::json!({}),
            ))
            .unwrap();

        let stats = store.experience_stats(DEFAULT_WINDOW_MS).unwrap();
        assert_eq!(stats.all_time.total, 3);
        assert_eq!(stats.all_time.success, 1);
        assert_eq!(stats.all_time.failure, 1);
        assert_eq!(stats.all_time.partial, 1);
        assert_eq!(stats.all_time.with_errors, 1);
        // `e2` both failed and recorded an error: counted once.
        assert_eq!(stats.all_time.problems, 1);
        assert_eq!(stats.avg_duration_ms(), Some(2000));
        assert_eq!(stats.avg_tokens(), Some(200));

        // Category filter is pushed into SQL.
        let coding = store
            .experience_stats_for(Some("coding"), DEFAULT_WINDOW_MS)
            .unwrap();
        assert_eq!(coding.all_time.total, 2);
        assert_eq!(coding.avg_duration_ms(), Some(2000));

        let categories = store.category_stats(None, 10).unwrap();
        assert_eq!(categories.len(), 2);
        // `coding` sorts first: it has a failure.
        assert_eq!(categories[0].category, "coding");
        assert_eq!(categories[0].failures, 1);
        assert_eq!(categories[0].errors, 1);
        // The failing row also logged an error: the union counts it once.
        assert_eq!(categories[0].problems, 1);
        assert_eq!(categories[0].problem_rate(), Some(0.5));
        assert_eq!(categories[1].category, "shell");
        assert_eq!(categories[1].problems, 0);
    }

    #[test]
    fn reflection_latency_comes_from_job_rows() {
        let store = EvolutionStore::open_in_memory().unwrap();
        store
            .save_job(&crate::types::ReflectionJob {
                id: "j1".to_string(),
                experience_id: "e1".to_string(),
                agent_id: "a".to_string(),
                status: ReflectionJobStatus::Done,
                attempts: 1,
                created_ms: 1_000,
                updated_ms: 1_500,
                error: None,
            })
            .unwrap();
        let stats = store.reflection_stats().unwrap();
        assert_eq!(stats.done, 1);
        assert_eq!(stats.avg_latency_ms, Some(500));
        assert_eq!(stats.failure_rate(), Some(0.0));
    }
}
