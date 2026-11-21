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
//! | per-agent/category performance | `experiences` grouped by `agent_id`/`task_category`, `json_each` over `errors`/`tools_used`, `metadata.duration_ms` |
//! | durations | `experiences.metadata.duration_ms` (optional producer key) |
//! | token usage | `experiences.metadata.tokens_used` (optional producer key) |
//! | reflection latency | `reflection_jobs.updated_ms - created_ms` for `done` jobs |
//! | proposal / job states | `proposals.status`, `reflection_jobs.status` |
//! | store footprint | `PRAGMA page_count * page_size` |
//!
//! Producers are *not required* to record `duration_ms`/`tokens_used`; when
//! they do not, the corresponding average is `None` and every engine that
//! would have used it reports "not recorded" instead of guessing.
//!
//! Reachability: every function here is a pure store read. Live callers are
//! the runtime's hourly self-check (via `mapek.monitor`/`analyze` and the
//! `self_improvement` opportunity scan) and the `vikram evolution` CLI.
//! The dormant `self_*` engines also consume these aggregates when invoked.

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

/// Which `experiences` column performance groups are keyed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerformanceDimension {
    /// Group by `experiences.agent_id`.
    Agent,
    /// Group by `experiences.task_category`.
    Category,
}

/// A named tally inside [`PerformanceStats`]: an error string or a tool name
/// with the number of distinct experiences it appears in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedCount {
    pub name: String,
    pub count: usize,
}

/// Per-agent or per-category performance derived only from recorded
/// experiences. Every count is a SQL aggregate over `experiences` rows;
/// rates are `None` when the group has no rows (a group with `total == 0`
/// cannot exist, but the rate helpers stay honest anyway).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PerformanceStats {
    /// The `agent_id` or `task_category` value this row aggregates.
    pub key: String,
    pub total: usize,
    pub success: usize,
    pub failure: usize,
    pub partial: usize,
    /// Experiences that failed *or* recorded at least one error (union).
    pub problems: usize,
    /// Mean of `metadata.duration_ms` over experiences that recorded it.
    pub avg_duration_ms: Option<u64>,
    /// Most frequent error strings, capped at [`TOP_PERFORMANCE_ITEMS`].
    pub top_errors: Vec<NamedCount>,
    /// Tool names by number of distinct experiences that used them, capped
    /// at [`TOP_PERFORMANCE_ITEMS`].
    pub tool_usage: Vec<NamedCount>,
}

impl PerformanceStats {
    /// Share of this group's experiences that succeeded outright.
    pub fn success_rate(&self) -> Option<f32> {
        self.rate(self.success)
    }
    /// Share of this group's experiences that failed outright.
    pub fn failure_rate(&self) -> Option<f32> {
        self.rate(self.failure)
    }
    /// Share of this group's experiences that failed or recorded an error.
    pub fn problem_rate(&self) -> Option<f32> {
        self.rate(self.problems)
    }
    fn rate(&self, n: usize) -> Option<f32> {
        if self.total == 0 {
            None
        } else {
            Some(n as f32 / self.total as f32)
        }
    }
}

/// Cap on `top_errors`/`tool_usage` entries per [`PerformanceStats`] row so
/// a group with pathological error/tool cardinality cannot produce an
/// unbounded payload.
pub const TOP_PERFORMANCE_ITEMS: usize = 5;

/// Recency window each side of an [`AgentTrend`] comparison covers.
pub const TREND_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Minimum absolute success-rate change (as a fraction, e.g. 0.05 = 5
/// points) before a trend is reported as improving or declining rather
/// than stable.
pub const TREND_EPSILON: f32 = 0.05;

// ---------- agent performance trends ----------

/// Direction of an agent's success-rate movement between the prior and
/// recent windows of an [`AgentTrend`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrendDirection {
    Improving,
    Declining,
    Stable,
    /// One or both windows held fewer than [`MIN_SAMPLE`] experiences, so
    /// the rate delta is not evidence of a real change.
    InsufficientData,
}

/// Per-agent success-rate comparison between the recent window and the
/// equally long window before it. Counts are SQL aggregates over
/// `experiences`; rates are `None` when a window has no rows in range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentTrend {
    pub agent_id: String,
    pub recent_total: usize,
    pub recent_success: usize,
    pub prior_total: usize,
    pub prior_success: usize,
    pub recent_success_rate: Option<f32>,
    pub prior_success_rate: Option<f32>,
    pub trend: TrendDirection,
}

impl AgentTrend {
    fn rate(total: usize, success: usize) -> Option<f32> {
        if total == 0 {
            None
        } else {
            Some(success as f32 / total as f32)
        }
    }

    fn classify(recent_total: usize, prior_total: usize, delta: f32) -> TrendDirection {
        if recent_total < MIN_SAMPLE || prior_total < MIN_SAMPLE {
            TrendDirection::InsufficientData
        } else if delta > TREND_EPSILON {
            TrendDirection::Improving
        } else if delta < -TREND_EPSILON {
            TrendDirection::Declining
        } else {
            TrendDirection::Stable
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
        let (first_timestamp_ms, last_timestamp_ms) = self.experience_time_bounds_for(category)?;
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
        self.experience_time_bounds_for(None)
    }

    /// The same bounds restricted to one `task_category`, matching the filters the rest of
    /// [`ExperienceStats`] applies — global bounds on a filtered stat would be wrong data.
    pub fn experience_time_bounds_for(
        &self,
        category: Option<&str>,
    ) -> Result<(Option<u64>, Option<u64>)> {
        self.with_conn(|conn| {
            let (min, max): (Option<i64>, Option<i64>) = conn
                .query_row(
                    "SELECT MIN(timestamp_ms), MAX(timestamp_ms) FROM experiences
                     WHERE (?1 IS NULL OR task_category = ?1)",
                    rusqlite::params![category],
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

    /// Per-agent or per-category performance tallies over recorded
    /// experiences, highest volume first (`key` ascending on ties), capped
    /// at `limit` groups. Restricted to experiences at or after `since_ms`
    /// when given.
    ///
    /// Outcome counts, the `metadata.duration_ms` average and the
    /// `problems` union are SQL aggregates like [`Self::category_stats`].
    /// `top_errors`/`tool_usage` expand the JSON-array `errors`/
    /// `tools_used` columns through `json_each` (guarded by `json_valid`,
    /// so a malformed row contributes nothing rather than failing the
    /// query) and count each name once per experience via
    /// `COUNT(DISTINCT id)`.
    pub fn performance_stats(
        &self,
        dimension: PerformanceDimension,
        since_ms: Option<u64>,
        limit: usize,
    ) -> Result<Vec<PerformanceStats>> {
        let key_col = match dimension {
            PerformanceDimension::Agent => "agent_id",
            PerformanceDimension::Category => "task_category",
        };
        let since = since_ms.map(|ms| ms as i64);
        self.with_conn(|conn| {
            let groups_sql = format!(
                "SELECT {key_col},
                        COUNT(*),
                        COALESCE(SUM(outcome = 'success'), 0),
                        COALESCE(SUM(outcome = 'failure'), 0),
                        COALESCE(SUM(outcome = 'partial'), 0),
                        COALESCE(SUM(outcome = 'failure' OR errors <> '[]'), 0),
                        AVG(CASE WHEN json_valid(metadata)
                                  AND typeof(json_extract(metadata, '$.{META_DURATION_MS}'))
                                      IN ('integer', 'real')
                                 THEN json_extract(metadata, '$.{META_DURATION_MS}')
                            END)
                 FROM experiences
                 WHERE (?1 IS NULL OR timestamp_ms >= ?1)
                 GROUP BY {key_col}
                 ORDER BY COUNT(*) DESC, {key_col} ASC
                 LIMIT ?2"
            );
            let mut groups_stmt = conn.prepare(&groups_sql).map_err(store_err)?;
            let group_rows = groups_stmt
                .query_map(rusqlite::params![since, limit as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)? as usize,
                        r.get::<_, i64>(2)? as usize,
                        r.get::<_, i64>(3)? as usize,
                        r.get::<_, i64>(4)? as usize,
                        r.get::<_, i64>(5)? as usize,
                        r.get::<_, Option<f64>>(6)?,
                    ))
                })
                .map_err(store_err)?;
            let mut stats: Vec<PerformanceStats> = Vec::new();
            for row in group_rows {
                let (key, total, success, failure, partial, problems, avg_dur) =
                    row.map_err(store_err)?;
                stats.push(PerformanceStats {
                    key,
                    total,
                    success,
                    failure,
                    partial,
                    problems,
                    avg_duration_ms: positive(avg_dur),
                    top_errors: Vec::new(),
                    tool_usage: Vec::new(),
                });
            }
            if stats.is_empty() {
                return Ok(stats);
            }

            // Error strings and tool names per group key. `json_each` over a
            // `CASE`-guarded column yields '[]' for malformed JSON instead of
            // raising; `typeof(...) = 'text'` skips non-string array items.
            for (column, is_errors) in [("errors", true), ("tools_used", false)] {
                let detail_sql = format!(
                    "SELECT {key_col}, e.value, COUNT(DISTINCT experiences.id)
                     FROM experiences,
                          json_each(CASE WHEN json_valid(experiences.{column})
                                         THEN experiences.{column}
                                         ELSE '[]' END) AS e
                     WHERE (?1 IS NULL OR experiences.timestamp_ms >= ?1)
                       AND typeof(e.value) = 'text'
                     GROUP BY {key_col}, e.value
                     ORDER BY {key_col} ASC, 3 DESC, e.value ASC"
                );
                let mut detail_stmt = conn.prepare(&detail_sql).map_err(store_err)?;
                let detail_rows = detail_stmt
                    .query_map(rusqlite::params![since], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)? as usize,
                        ))
                    })
                    .map_err(store_err)?;
                let mut by_key: std::collections::HashMap<String, Vec<NamedCount>> =
                    std::collections::HashMap::new();
                for row in detail_rows {
                    let (key, name, count) = row.map_err(store_err)?;
                    by_key
                        .entry(key)
                        .or_default()
                        .push(NamedCount { name, count });
                }
                for group in stats.iter_mut() {
                    let Some(items) = by_key.remove(&group.key) else {
                        continue;
                    };
                    let items = items
                        .into_iter()
                        .take(TOP_PERFORMANCE_ITEMS)
                        .collect::<Vec<_>>();
                    if is_errors {
                        group.top_errors = items;
                    } else {
                        group.tool_usage = items;
                    }
                }
            }
            Ok(stats)
        })
    }

    /// Per-agent success-rate trend over two adjacent windows of `window_ms`:
    /// the most recent `window_ms` versus the `window_ms` before it.
    /// Experiences older than two windows are ignored. One row per `agent_id`
    /// seen in either window, ordered by `agent_id`.
    ///
    /// All counts are SQL aggregates over `experiences`; the trend
    /// classification is [`AgentTrend::classify`] — `InsufficientData`
    /// unless both windows hold at least [`MIN_SAMPLE`] experiences.
    pub fn agent_trends(&self, window_ms: u64) -> Result<Vec<AgentTrend>> {
        let now = now_ms();
        let recent_since = now.saturating_sub(window_ms);
        let prior_since = now.saturating_sub(window_ms.saturating_mul(2));
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT agent_id,
                            COALESCE(SUM(timestamp_ms >= ?1), 0),
                            COALESCE(SUM(outcome = 'success' AND timestamp_ms >= ?1), 0),
                            COALESCE(SUM(timestamp_ms >= ?2 AND timestamp_ms < ?1), 0),
                            COALESCE(SUM(outcome = 'success'
                                         AND timestamp_ms >= ?2 AND timestamp_ms < ?1), 0)
                     FROM experiences
                     WHERE timestamp_ms >= ?2
                     GROUP BY agent_id
                     ORDER BY agent_id ASC",
                )
                .map_err(store_err)?;
            let rows = stmt
                .query_map(
                    rusqlite::params![recent_since as i64, prior_since as i64],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)? as usize,
                            r.get::<_, i64>(2)? as usize,
                            r.get::<_, i64>(3)? as usize,
                            r.get::<_, i64>(4)? as usize,
                        ))
                    },
                )
                .map_err(store_err)?;
            let mut out = Vec::new();
            for row in rows {
                let (agent_id, recent_total, recent_success, prior_total, prior_success) =
                    row.map_err(store_err)?;
                let recent_rate = AgentTrend::rate(recent_total, recent_success);
                let prior_rate = AgentTrend::rate(prior_total, prior_success);
                let delta = match (recent_rate, prior_rate) {
                    (Some(r), Some(p)) => r - p,
                    _ => 0.0,
                };
                out.push(AgentTrend {
                    agent_id,
                    recent_total,
                    recent_success,
                    prior_total,
                    prior_success,
                    recent_success_rate: recent_rate,
                    prior_success_rate: prior_rate,
                    trend: AgentTrend::classify(recent_total, prior_total, delta),
                });
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

    fn perf_exp(
        id: &str,
        agent: &str,
        category: &str,
        outcome: ExperienceOutcome,
        tools: Vec<String>,
        errors: Vec<String>,
        duration_ms: Option<u64>,
    ) -> Experience {
        Experience {
            id: id.to_string(),
            timestamp_ms: now_ms(),
            agent_id: agent.to_string(),
            project_id: None,
            task_category: category.to_string(),
            user_request: "req".to_string(),
            outcome,
            tools_used: tools,
            errors,
            corrections: vec![],
            feedback: None,
            reusable: false,
            sensitivity: Sensitivity::Public,
            metadata: duration_ms
                .map(|d| serde_json::json!({ META_DURATION_MS: d }))
                .unwrap_or_else(|| serde_json::json!({})),
        }
    }

    #[test]
    fn performance_stats_group_by_agent_and_category() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let s = |v: &str| vec![v.to_string()];
        store
            .save_experience(&perf_exp(
                "e1",
                "a1",
                "coding",
                ExperienceOutcome::Success,
                vec!["read".to_string(), "bash".to_string()],
                vec![],
                Some(1000),
            ))
            .unwrap();
        store
            .save_experience(&perf_exp(
                "e2",
                "a1",
                "coding",
                ExperienceOutcome::Failure,
                s("bash"),
                vec!["timeout".to_string(), "denied".to_string()],
                Some(3000),
            ))
            .unwrap();
        // Same error string twice in one experience must count once.
        store
            .save_experience(&perf_exp(
                "e3",
                "a2",
                "coding",
                ExperienceOutcome::Partial,
                vec![],
                vec!["timeout".to_string(), "timeout".to_string()],
                None,
            ))
            .unwrap();
        store
            .save_experience(&perf_exp(
                "e4",
                "a2",
                "shell",
                ExperienceOutcome::Success,
                s("read"),
                vec![],
                Some(500),
            ))
            .unwrap();

        let agents = store
            .performance_stats(PerformanceDimension::Agent, None, 10)
            .unwrap();
        assert_eq!(agents.len(), 2);
        // a1 and a2 tie on volume; key ascending breaks the tie.
        assert_eq!(agents[0].key, "a1");
        assert_eq!(agents[0].total, 2);
        assert_eq!(agents[0].success, 1);
        assert_eq!(agents[0].failure, 1);
        assert_eq!(agents[0].partial, 0);
        assert_eq!(agents[0].problems, 1);
        assert_eq!(agents[0].success_rate(), Some(0.5));
        assert_eq!(agents[0].failure_rate(), Some(0.5));
        assert_eq!(agents[0].avg_duration_ms, Some(2000));
        // Tied error counts order by name ascending.
        assert_eq!(
            agents[0].top_errors,
            vec![
                NamedCount {
                    name: "denied".to_string(),
                    count: 1
                },
                NamedCount {
                    name: "timeout".to_string(),
                    count: 1
                },
            ]
        );
        assert_eq!(
            agents[0].tool_usage,
            vec![
                NamedCount {
                    name: "bash".to_string(),
                    count: 2
                },
                NamedCount {
                    name: "read".to_string(),
                    count: 1
                },
            ]
        );

        assert_eq!(agents[1].key, "a2");
        assert_eq!(agents[1].total, 2);
        assert_eq!(agents[1].partial, 1);
        // e3 recorded errors without failing: still a problem.
        assert_eq!(agents[1].problems, 1);
        assert_eq!(agents[1].problem_rate(), Some(0.5));
        // Only e4 recorded a duration.
        assert_eq!(agents[1].avg_duration_ms, Some(500));
        assert_eq!(
            agents[1].top_errors,
            vec![NamedCount {
                name: "timeout".to_string(),
                count: 1
            }]
        );
        assert_eq!(
            agents[1].tool_usage,
            vec![NamedCount {
                name: "read".to_string(),
                count: 1
            }]
        );

        let categories = store
            .performance_stats(PerformanceDimension::Category, None, 10)
            .unwrap();
        assert_eq!(categories.len(), 2);
        assert_eq!(categories[0].key, "coding");
        assert_eq!(categories[0].total, 3);
        assert_eq!(categories[0].problems, 2);
        // e1+e2 recorded durations; e3 did not and must not dilute the mean.
        assert_eq!(categories[0].avg_duration_ms, Some(2000));
        assert_eq!(categories[1].key, "shell");
        assert_eq!(categories[1].total, 1);
        assert_eq!(categories[1].tool_usage[0].name, "read");

        // `limit` caps the group list, `since_ms` filters rows.
        assert_eq!(
            store
                .performance_stats(PerformanceDimension::Agent, None, 1)
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .performance_stats(PerformanceDimension::Agent, Some(u64::MAX / 2), 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn performance_stats_empty_store_is_empty() {
        let store = EvolutionStore::open_in_memory().unwrap();
        assert!(
            store
                .performance_stats(PerformanceDimension::Agent, None, 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .performance_stats(PerformanceDimension::Category, None, 10)
                .unwrap()
                .is_empty()
        );
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

    fn seed_trend(
        store: &EvolutionStore,
        id: &str,
        agent: &str,
        outcome: ExperienceOutcome,
        timestamp_ms: u64,
    ) {
        let mut e = perf_exp(id, agent, "coding", outcome, vec![], vec![], None);
        e.timestamp_ms = timestamp_ms;
        store.save_experience(&e).unwrap();
    }

    /// Seed `n` experiences for `agent` all with `outcome` at `timestamp_ms`.
    fn seed_trend_batch(
        store: &EvolutionStore,
        agent: &str,
        outcome: ExperienceOutcome,
        timestamp_ms: u64,
        n: usize,
    ) {
        for i in 0..n {
            seed_trend(
                store,
                &format!("{agent}-{i}-{}", timestamp_ms),
                agent,
                outcome,
                timestamp_ms,
            );
        }
    }

    #[test]
    fn agent_trends_empty_store_is_empty() {
        let store = EvolutionStore::open_in_memory().unwrap();
        assert!(store.agent_trends(TREND_WINDOW_MS).unwrap().is_empty());
    }

    #[test]
    fn agent_trends_classify_improving_declining_stable_and_thin_windows() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let now = now_ms();
        let w = TREND_WINDOW_MS;
        let recent = now;
        let prior = now - w - 1_000; // inside the prior window
        let ancient = now - w * 2 - 1_000; // older than both windows

        // a1: 5/5 recent successes vs 0/5 prior -> improving.
        seed_trend_batch(&store, "a1", ExperienceOutcome::Success, recent, 5);
        seed_trend_batch(&store, "a1", ExperienceOutcome::Failure, prior, 5);
        // a2: 5/5 vs 5/5 -> stable.
        seed_trend_batch(&store, "a2", ExperienceOutcome::Success, recent, 5);
        seed_trend_batch(&store, "a2", ExperienceOutcome::Success, prior, 5);
        // a3: 0/5 vs 5/5 -> declining.
        seed_trend_batch(&store, "a3", ExperienceOutcome::Failure, recent, 5);
        seed_trend_batch(&store, "a3", ExperienceOutcome::Success, prior, 5);
        // a4: recent only -> prior window empty -> insufficient data.
        seed_trend_batch(&store, "a4", ExperienceOutcome::Success, recent, 5);
        // a5: only an ancient row -> outside both windows, must not appear.
        seed_trend_batch(&store, "a5", ExperienceOutcome::Success, ancient, 5);
        // a6: 3 recent successes -> below MIN_SAMPLE -> insufficient data.
        seed_trend_batch(&store, "a6", ExperienceOutcome::Success, recent, 3);
        seed_trend_batch(&store, "a6", ExperienceOutcome::Failure, prior, 5);

        let trends = store.agent_trends(w).unwrap();
        let by_agent: std::collections::HashMap<_, _> =
            trends.iter().map(|t| (t.agent_id.as_str(), t)).collect();
        assert_eq!(trends.len(), 5, "a5 must be excluded: {trends:?}");

        let a1 = by_agent["a1"];
        assert_eq!(a1.recent_total, 5);
        assert_eq!(a1.recent_success, 5);
        assert_eq!(a1.prior_total, 5);
        assert_eq!(a1.prior_success, 0);
        assert_eq!(a1.recent_success_rate, Some(1.0));
        assert_eq!(a1.prior_success_rate, Some(0.0));
        assert_eq!(a1.trend, TrendDirection::Improving);

        assert_eq!(by_agent["a2"].trend, TrendDirection::Stable);
        assert_eq!(by_agent["a3"].trend, TrendDirection::Declining);

        let a4 = by_agent["a4"];
        assert_eq!(a4.prior_total, 0);
        assert_eq!(a4.prior_success_rate, None);
        assert_eq!(a4.trend, TrendDirection::InsufficientData);

        assert_eq!(by_agent["a6"].trend, TrendDirection::InsufficientData);

        // Rows come back ordered by agent_id.
        let ids: Vec<_> = trends.iter().map(|t| t.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["a1", "a2", "a3", "a4", "a6"]);
    }

    #[test]
    fn agent_trends_small_delta_is_stable() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let now = now_ms();
        let w = TREND_WINDOW_MS;
        // recent: 39/40 success (0.975); prior: 40/40 (1.0) -> delta -0.025,
        // inside TREND_EPSILON, so stable rather than declining.
        seed_trend_batch(&store, "b1", ExperienceOutcome::Success, now, 39);
        seed_trend(&store, "b1-f", "b1", ExperienceOutcome::Failure, now);
        seed_trend_batch(
            &store,
            "b1",
            ExperienceOutcome::Success,
            now - w - 1_000,
            40,
        );
        let trends = store.agent_trends(w).unwrap();
        assert_eq!(trends.len(), 1);
        assert_eq!(trends[0].trend, TrendDirection::Stable);
    }
}
