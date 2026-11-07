//! Memory consolidation lifecycle: capture -> stage -> dedup -> contradiction
//! detection -> importance+recency scoring -> supersession -> aging/expiration.
//!
//! Design decisions (no external model/LLM calls; pure heuristics only):
//! - Observations are `key/value` pairs scoped per `MemoryStore` conventions.
//! - Provenance (source, confidence, timestamps) is mandatory on every write and
//!   preserved through supersession (the superseded record is never deleted
//!   silently; it is marked `Superseded` with a pointer to its successor).
//! - Scoring is `score = importance * recency_decay * confidence_factor * access_boost`.
//! - Contradiction = same `(scope, key)` with different normalized value.
//!   Winner = highest `(confidence, updated_ms, score)` tuple; losers are
//!   superseded, never deleted, so history remains auditable.
//!
//! The pure `consolidate()` function operates on `&[Observation]` so it is
//! testable without a database; `ConsolidationStore` is the SQLite-backed
//! persistence layer used by the `memory.consolidate` tool.

#![allow(clippy::too_many_arguments)]

use crate::error::{Result, RuntimeError};
use crate::types::now_ms;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationStatus {
    Staged,
    Active,
    Superseded,
    Expired,
}

impl ObservationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Expired => "expired",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "staged" => Ok(Self::Staged),
            "active" => Ok(Self::Active),
            "superseded" => Ok(Self::Superseded),
            "expired" => Ok(Self::Expired),
            other => Err(RuntimeError::InvalidState(format!(
                "unknown observation status `{other}`"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub id: String,
    pub key: String,
    pub value: String,
    pub scope: String,
    pub importance: f64,
    pub confidence: f64,
    pub source: String,
    pub status: ObservationStatus,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub last_accessed_ms: u64,
    pub access_count: u64,
    pub supersedes: Option<String>,
    pub superseded_by: Option<String>,
}

impl Observation {
    pub fn new(
        id: impl Into<String>,
        key: impl Into<String>,
        value: impl Into<String>,
        scope: impl Into<String>,
        source: impl Into<String>,
        confidence: f64,
        importance: f64,
        now_ms: u64,
    ) -> Self {
        Self {
            id: id.into(),
            key: key.into(),
            value: value.into(),
            scope: scope.into(),
            importance: clamp01(importance),
            confidence: clamp01(confidence),
            source: source.into(),
            status: ObservationStatus::Staged,
            created_ms: now_ms,
            updated_ms: now_ms,
            last_accessed_ms: now_ms,
            access_count: 0,
            supersedes: None,
            superseded_by: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConsolidationPolicy {
    /// Observations older than this with score below `min_keep_score` expire.
    pub max_age_ms: u64,
    /// Half-life for recency decay. Zero disables decay (recency = 1.0).
    pub half_life_ms: u64,
    /// Active observations scoring below this are candidates for expiration
    /// once past `max_age_ms`.
    pub min_keep_score: f64,
    /// First conflict with older staged copy: mark older as superseded
    /// rather than keeping both.
    pub supersession_on_conflict: bool,
}

impl Default for ConsolidationPolicy {
    fn default() -> Self {
        Self {
            max_age_ms: 90 * 24 * 3_600_000,
            half_life_ms: 30 * 24 * 3_600_000,
            min_keep_score: 0.05,
            supersession_on_conflict: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsolidationReport {
    pub captured: usize,
    pub deduplicated: usize,
    pub contradictions_detected: usize,
    pub superseded: usize,
    pub expired: usize,
    pub active_after: usize,
    pub staged_after: usize,
}

fn clamp01(v: f64) -> f64 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Importance + recency + confidence scoring. Returns 0.0..=~1.0+.
pub fn score(obs: &Observation, now_ms: u64, policy: &ConsolidationPolicy) -> f64 {
    let anchor = obs.last_accessed_ms.max(obs.updated_ms);
    let age = now_ms.saturating_sub(anchor) as f64;
    let recency = if policy.half_life_ms == 0 {
        1.0
    } else {
        (0.5_f64).powf(age / policy.half_life_ms as f64)
    };
    let access_boost = (1.0 + obs.access_count as f64).ln() / 2.0 + 1.0;
    (obs.importance * recency * obs.confidence * access_boost).clamp(0.0, 2.0)
}

/// Pure consolidation: dedup, contradiction -> supersession, aging -> expiration.
///
/// Input is not mutated; output observations are returned in stable order
/// (scope, key, created_ms). Superseded/Expired records are retained in the
/// output so provenance history is preserved.
pub fn consolidate(
    observations: &[Observation],
    now_ms: u64,
    policy: &ConsolidationPolicy,
) -> (Vec<Observation>, ConsolidationReport) {
    let mut report = ConsolidationReport {
        captured: observations.len(),
        ..Default::default()
    };
    if observations.is_empty() {
        return (Vec::new(), report);
    }

    // ---- 1. Dedup exact (scope, key, normalized value) --------------------
    // Keep winner = highest (confidence, updated_ms); merge access counts and
    // keep earliest created_ms. Losers dropped entirely (they are identical,
    // not contradictory), count reported.
    let mut dedup_map: HashMap<(String, String, String), Observation> = HashMap::new();
    let mut dedup_order: Vec<(String, String, String)> = Vec::new();
    for obs in observations {
        let k = (obs.scope.clone(), obs.key.clone(), normalize(&obs.value));
        match dedup_map.get(&k) {
            None => {
                dedup_order.push(k.clone());
                dedup_map.insert(k, obs.clone());
            }
            Some(existing) => {
                report.deduplicated += 1;
                let new_wins =
                    (obs.confidence, obs.updated_ms) > (existing.confidence, existing.updated_ms);
                if new_wins {
                    let mut winner = obs.clone();
                    winner.created_ms = winner.created_ms.min(existing.created_ms);
                    winner.access_count += existing.access_count;
                    winner.supersedes = winner.supersedes.or(existing.supersedes.clone());
                    dedup_map.insert(k, winner);
                } else {
                    let mut keeper = existing.clone();
                    keeper.access_count += obs.access_count;
                    dedup_map.insert(k, keeper);
                }
            }
        }
    }
    let mut deduped: Vec<Observation> = dedup_order
        .iter()
        .filter_map(|k| dedup_map.remove(k))
        .collect();

    // ---- 2. Contradiction detection per (scope, key) -----------------------
    if policy.supersession_on_conflict {
        // Index by (scope, key)
        let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (i, obs) in deduped.iter().enumerate() {
            groups
                .entry((obs.scope.clone(), obs.key.clone()))
                .or_default()
                .push(i);
        }
        for (gk, idxs) in groups {
            if idxs.len() < 2 {
                continue;
            }
            // Only contradictory if values actually differ (after dedup they do).
            let distinct_count = {
                let mut s = std::collections::HashSet::new();
                for &i in &idxs {
                    s.insert(normalize(&deduped[i].value));
                }
                s.len()
            };
            if distinct_count < 2 {
                continue;
            }
            report.contradictions_detected += 1;
            let _ = gk;
            // Winner = max (confidence, score, updated_ms)
            let winner_idx = *idxs
                .iter()
                .max_by(|&&a, &&b| {
                    let oa = &deduped[a];
                    let ob = &deduped[b];
                    let sa = score(oa, now_ms, policy);
                    let sb = score(ob, now_ms, policy);
                    (oa.confidence, sa, oa.updated_ms)
                        .partial_cmp(&(ob.confidence, sb, ob.updated_ms))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap();
            let winner_id = deduped[winner_idx].id.clone();
            for &i in &idxs {
                if i == winner_idx {
                    deduped[i].status = ObservationStatus::Active;
                    deduped[i].updated_ms = now_ms;
                    continue;
                }
                deduped[i].status = ObservationStatus::Superseded;
                deduped[i].superseded_by = Some(winner_id.clone());
                deduped[i].updated_ms = now_ms;
                report.superseded += 1;
            }
            // Track lineage: winner supersedes each loser (first only stored
            // to keep single pointer; full history via superseded_by).
            let first_loser = idxs.iter().find(|&&i| i != winner_idx).copied();
            if let Some(li) = first_loser {
                deduped[winner_idx].supersedes = Some(deduped[li].id.clone());
            }
        }
    }

    // ---- 3. Aging / expiration ---------------------------------------------
    for obs in deduped.iter_mut() {
        if obs.status == ObservationStatus::Superseded {
            continue;
        }
        // Staged obs are promoted to Active if not expired/superseded.
        let anchor = obs.last_accessed_ms.max(obs.updated_ms);
        let age = now_ms.saturating_sub(anchor);
        let s = score(obs, now_ms, policy);
        if age > policy.max_age_ms && s < policy.min_keep_score {
            obs.status = ObservationStatus::Expired;
            obs.updated_ms = now_ms;
            report.expired += 1;
        } else if obs.status == ObservationStatus::Staged {
            obs.status = ObservationStatus::Active;
            obs.updated_ms = now_ms;
        }
    }

    report.active_after = deduped
        .iter()
        .filter(|o| o.status == ObservationStatus::Active)
        .count();
    report.staged_after = deduped
        .iter()
        .filter(|o| o.status == ObservationStatus::Staged)
        .count();

    deduped.sort_by(|a, b| (&a.scope, &a.key, a.created_ms).cmp(&(&b.scope, &b.key, b.created_ms)));
    (deduped, report)
}

/// Deterministic observation id: scope + key + created_ms + short hash of value.
/// Not cryptographic; uniqueness within a store is enforced by the PRIMARY KEY
/// with a counter suffix on collision (handled in `capture`).
pub fn observation_id(scope: &str, key: &str, value: &str, created_ms: u64) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in value.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{scope}\u{1f}{key}\u{1f}{created_ms}\u{1f}{h:016x}")
}

// ---------------------------------------------------------------------------
// SQLite persistence
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ConsolidationStore {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for ConsolidationStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsolidationStore").finish_non_exhaustive()
    }
}

impl ConsolidationStore {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| RuntimeError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path).map_err(|e| RuntimeError::Store(e.to_string()))?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| RuntimeError::Store(e.to_string()))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .expect("consolidation store mutex not poisoned")
    }

    /// Capture a new observation (staged). Provenance is mandatory.
    pub fn capture(
        &self,
        key: &str,
        value: &str,
        scope: &str,
        source: &str,
        confidence: f64,
        importance: f64,
    ) -> Result<Observation> {
        if key.trim().is_empty() {
            return Err(RuntimeError::InvalidState(
                "observation key must not be empty".into(),
            ));
        }
        if value.trim().is_empty() {
            return Err(RuntimeError::InvalidState(
                "observation value must not be empty".into(),
            ));
        }
        let now = now_ms();
        let base = observation_id(scope, key, value, now);
        // Resolve id collision with counter suffix.
        let mut candidate = base.clone();
        let mut n = 0u32;
        let obs = {
            let conn = self.lock();
            loop {
                let exists: Option<String> = conn
                    .query_row(
                        "SELECT id FROM memory_observations WHERE id = ?1",
                        params![candidate],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| RuntimeError::Store(e.to_string()))?;
                if exists.is_none() {
                    break;
                }
                n += 1;
                candidate = format!("{base}\u{1f}{n}");
            }
            let obs = Observation::new(
                candidate.clone(),
                key,
                value,
                scope,
                source,
                confidence,
                importance,
                now,
            );
            conn.execute(
                "INSERT INTO memory_observations (id, key, value, scope, importance, confidence, source, status, created_ms, updated_ms, last_accessed_ms, access_count, supersedes, superseded_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    obs.id, obs.key, obs.value, obs.scope, obs.importance, obs.confidence, obs.source,
                    obs.status.as_str(), obs.created_ms as i64, obs.updated_ms as i64, obs.last_accessed_ms as i64,
                    obs.access_count as i64, obs.supersedes, obs.superseded_by
                ],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
            obs
        };
        Ok(obs)
    }

    pub fn load_scope(&self, scope: &str) -> Result<Vec<Observation>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, key, value, scope, importance, confidence, source, status, created_ms, updated_ms, last_accessed_ms, access_count, supersedes, superseded_by
                 FROM memory_observations WHERE scope = ?1 ORDER BY key ASC, created_ms ASC",
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let rows = stmt
            .query_map(params![scope], row_to_observation)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    pub fn load_all(&self) -> Result<Vec<Observation>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, key, value, scope, importance, confidence, source, status, created_ms, updated_ms, last_accessed_ms, access_count, supersedes, superseded_by
                 FROM memory_observations ORDER BY scope ASC, key ASC, created_ms ASC",
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let rows = stmt
            .query_map([], row_to_observation)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    pub fn touch(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn
            .execute(
                "UPDATE memory_observations SET last_accessed_ms = ?1, access_count = access_count + 1 WHERE id = ?2",
                params![now_ms() as i64, id],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        Ok(n > 0)
    }

    /// Load -> pure consolidate -> persist status/lineage back. Returns report
    /// plus the post-consolidation observations ordered by score desc.
    pub fn run_consolidation(
        &self,
        scope: Option<&str>,
        policy: &ConsolidationPolicy,
    ) -> Result<(ConsolidationReport, Vec<Observation>)> {
        let input = match scope {
            Some(s) => self.load_scope(s)?,
            None => self.load_all()?,
        };
        let now = now_ms();
        let (mut output, report) = consolidate(&input, now, policy);
        let mut conn = self.lock();
        let tx = conn
            .transaction()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        for obs in &output {
            tx.execute(
                "UPDATE memory_observations SET status = ?1, updated_ms = ?2, last_accessed_ms = ?3, access_count = ?4, supersedes = ?5, superseded_by = ?6 WHERE id = ?7",
                params![
                    obs.status.as_str(),
                    obs.updated_ms as i64,
                    obs.last_accessed_ms as i64,
                    obs.access_count as i64,
                    obs.supersedes,
                    obs.superseded_by,
                    obs.id
                ],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        }
        tx.commit()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;

        // Order by score desc for the tool response.
        output.sort_by(|a, b| {
            let sa = score(a, now, policy);
            let sb = score(b, now, policy);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok((report, output))
    }
}

fn row_to_observation(row: &rusqlite::Row) -> std::result::Result<Observation, rusqlite::Error> {
    let status_str: String = row.get(7)?;
    Ok(Observation {
        id: row.get(0)?,
        key: row.get(1)?,
        value: row.get(2)?,
        scope: row.get(3)?,
        importance: row.get(4)?,
        confidence: row.get(5)?,
        source: row.get(6)?,
        status: ObservationStatus::parse(&status_str).unwrap_or(ObservationStatus::Staged),
        created_ms: (row.get::<_, i64>(8)?) as u64,
        updated_ms: (row.get::<_, i64>(9)?) as u64,
        last_accessed_ms: (row.get::<_, i64>(10)?) as u64,
        access_count: (row.get::<_, i64>(11)?) as u64,
        supersedes: row.get(12)?,
        superseded_by: row.get(13)?,
    })
}

fn collect_rows<I>(rows: I) -> Result<Vec<Observation>>
where
    I: Iterator<Item = std::result::Result<Observation, rusqlite::Error>>,
{
    rows.map(|r| r.map_err(|e| RuntimeError::Store(e.to_string())))
        .collect()
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS memory_observations (
            id                TEXT PRIMARY KEY,
            key               TEXT NOT NULL,
            value             TEXT NOT NULL,
            scope             TEXT NOT NULL,
            importance        REAL NOT NULL DEFAULT 0.5,
            confidence        REAL NOT NULL DEFAULT 0.5,
            source            TEXT NOT NULL DEFAULT '',
            status            TEXT NOT NULL DEFAULT 'staged',
            created_ms        INTEGER NOT NULL,
            updated_ms        INTEGER NOT NULL,
            last_accessed_ms  INTEGER NOT NULL,
            access_count      INTEGER NOT NULL DEFAULT 0,
            supersedes        TEXT,
            superseded_by     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_mem_obs_scope_key
            ON memory_observations(scope, key);
        "#,
    )
    .map_err(|e| RuntimeError::Store(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(
        id: &str,
        key: &str,
        val: &str,
        scope: &str,
        conf: f64,
        imp: f64,
        created: u64,
    ) -> Observation {
        let mut o = Observation::new(id, key, val, scope, "test", conf, imp, created);
        o.status = ObservationStatus::Staged;
        o
    }

    #[test]
    fn dedup_collapses_identical_values_and_merges_access() {
        let policy = ConsolidationPolicy::default();
        let now = 10_000_000u64;
        let a = obs("a", "k", "v", "global", 0.8, 0.5, now - 100);
        let mut b = obs("b", "k", "  V  ", "global", 0.9, 0.5, now - 50);
        b.access_count = 3;
        let (out, report) = consolidate(&[a, b], now, &policy);
        assert_eq!(report.deduplicated, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "b", "higher confidence wins dedup");
        assert_eq!(out[0].access_count, 3);
        assert_eq!(
            out[0].status,
            ObservationStatus::Active,
            "staged promotes to active"
        );
    }

    #[test]
    fn contradiction_supersedes_loser_and_links_lineage() {
        let policy = ConsolidationPolicy {
            half_life_ms: 0,
            ..Default::default()
        };
        let now = 20_000_000u64;
        let old = obs("old", "editor", "vim", "global", 0.6, 0.5, now - 1000);
        let new = obs("new", "editor", "emacs", "global", 0.9, 0.5, now);
        let (out, report) = consolidate(&[old, new], now, &policy);
        assert_eq!(report.contradictions_detected, 1);
        assert_eq!(report.superseded, 1);
        assert!(
            out.iter()
                .any(|o| o.id == "new" && o.status == ObservationStatus::Active)
        );
        let loser = out.iter().find(|o| o.id == "old").unwrap();
        assert_eq!(loser.status, ObservationStatus::Superseded);
        assert_eq!(loser.superseded_by.as_deref(), Some("new"));
        let winner = out.iter().find(|o| o.id == "new").unwrap();
        assert_eq!(winner.supersedes.as_deref(), Some("old"));
    }

    #[test]
    fn scoring_rewards_importance_recency_confidence() {
        let policy = ConsolidationPolicy {
            half_life_ms: 1_000,
            ..Default::default()
        };
        let now = 50_000u64;
        let fresh_high = obs("f", "k", "v", "global", 1.0, 1.0, now);
        let stale_low = {
            let mut o = obs("s", "k2", "v2", "global", 0.2, 0.2, now - 10_000);
            o.updated_ms = now - 10_000;
            o.last_accessed_ms = now - 10_000;
            o
        };
        assert!(score(&fresh_high, now, &policy) > score(&stale_low, now, &policy));
    }

    #[test]
    fn aging_expires_stale_low_scores_but_keeps_fresh() {
        let policy = ConsolidationPolicy {
            max_age_ms: 1_000,
            half_life_ms: 500,
            min_keep_score: 0.4,
            supersession_on_conflict: true,
        };
        let now = 100_000u64;
        let stale = {
            let mut o = obs("stale", "k", "v", "global", 0.2, 0.2, now - 5_000);
            o.updated_ms = now - 5_000;
            o.last_accessed_ms = now - 5_000;
            o
        };
        let fresh = obs("fresh", "k2", "v2", "global", 0.9, 0.9, now);
        let (out, report) = consolidate(&[stale, fresh], now, &policy);
        assert_eq!(report.expired, 1);
        assert!(
            out.iter()
                .any(|o| o.id == "stale" && o.status == ObservationStatus::Expired)
        );
        assert!(
            out.iter()
                .any(|o| o.id == "fresh" && o.status == ObservationStatus::Active)
        );
    }

    #[test]
    fn store_round_trip_captures_provenance() {
        let store = ConsolidationStore::open_in_memory().unwrap();
        let o = store
            .capture("key", "value", "global", "unit-test", 0.7, 0.8)
            .unwrap();
        assert_eq!(o.source, "unit-test");
        assert_eq!((o.confidence, o.importance), (0.7, 0.8));
        assert_eq!(o.status, ObservationStatus::Staged);
        assert!(o.created_ms > 0);
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].source, "unit-test");
    }

    #[test]
    fn store_consolidation_promotes_and_dedups() {
        let store = ConsolidationStore::open_in_memory().unwrap();
        store.capture("k", "v", "global", "s", 0.5, 0.5).unwrap();
        store.capture("k", "v", "global", "s", 0.5, 0.5).unwrap();
        store.capture("k2", "v2", "global", "s", 0.9, 0.9).unwrap();
        let (report, out) = store
            .run_consolidation(Some("global"), &ConsolidationPolicy::default())
            .unwrap();
        assert!(report.deduplicated >= 1, "{report:?}");
        assert!(
            out.iter().all(|o| o.status == ObservationStatus::Active),
            "{out:?}"
        );
    }

    #[test]
    fn store_contradiction_marks_superseded() {
        let store = ConsolidationStore::open_in_memory().unwrap();
        store
            .capture("editor", "vim", "global", "s", 0.5, 0.5)
            .unwrap();
        // Ensure updated_ms differs for stable winner: second has higher conf.
        store
            .capture("editor", "emacs", "global", "s", 0.95, 0.5)
            .unwrap();
        let (report, out) = store
            .run_consolidation(Some("global"), &ConsolidationPolicy::default())
            .unwrap();
        assert_eq!(report.contradictions_detected, 1);
        assert_eq!(report.superseded, 1);
        let loser = out.iter().find(|o| o.value == "vim").unwrap();
        assert_eq!(loser.status, ObservationStatus::Superseded);
        assert!(loser.superseded_by.is_some());
    }

    #[test]
    fn capture_rejects_empty_key_or_value() {
        let store = ConsolidationStore::open_in_memory().unwrap();
        assert!(store.capture("", "v", "global", "s", 0.5, 0.5).is_err());
        assert!(store.capture("k", "", "global", "s", 0.5, 0.5).is_err());
    }

    #[test]
    fn confidence_and_importance_are_clamped() {
        let o = Observation::new("x", "k", "v", "global", "s", 5.0, -1.0, 0);
        assert_eq!(o.confidence, 1.0);
        assert_eq!(o.importance, 0.0);
    }

    #[test]
    fn empty_input_yields_zero_report() {
        let (out, report) = consolidate(&[], 0, &ConsolidationPolicy::default());
        assert!(out.is_empty());
        assert_eq!(report.captured, 0);
    }
}
