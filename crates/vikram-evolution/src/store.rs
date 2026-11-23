//! SQLite persistence for the evolution engine.
//!
//! Single source of truth for evolution data. The runtime's
//! `learning.rs` (`LearningManager`, JSON file) is the *approval surface*
//! only: it stages proposals for the operator. Everything it learns from
//! (experiences, layered memory, reflection jobs, candidates, evaluations,
//! hive lessons, skill versions, learning events) lives here. See the
//! module doc in `vikram-runtime/src/learning.rs`.
//!
//! Conventions (no silent failures):
//! - Every fallible operation returns `Result`; callers must handle errors.
//!   There are no `unwrap_or_default()` JSON parses, no rows dropped on
//!   decode failure, and no `count().unwrap_or(0)` — a corrupt row or a
//!   failed count is an `EvolutionError`, never an empty list / zero.
//! - JSON columns are written with `serde_json::to_string(..)?` and read
//!   with a checked parse that maps failures to `EvolutionError::Serde`.
//! - Memory listings/recall/search only ever return *live* entries:
//!   superseded (`superseded_by IS NOT NULL`), archived (`lifecycle =
//!   'archived'`), and expired (`expires_ms <= now`) rows are filtered in
//!   SQL, in the same query that applies `LIMIT`, so pagination cannot
//!   return a short page that is actually full of dead rows.
//! - `EvaluationResult` has its own primary key (`id`), so re-evaluating
//!   a candidate on the same dataset appends a new row instead of
//!   silently overwriting the previous evaluation.
//!
//! Schema is versioned via `PRAGMA user_version` and migrated
//! incrementally in [`migrate`]; a pre-versioning database is treated as
//! version 0 and upgraded in place. Migrations never drop user data.

use crate::error::{EvolutionError, Result};
use crate::types::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// Current schema version. Bump when adding a migration step.
pub const SCHEMA_VERSION: i64 = 4;

/// Default retention horizon for [`EvolutionStore::prune_older_than`].
/// 30 days is conservative: it bounds growth of the append-only tables
/// without touching anything a live workflow could still reference.
pub const DEFAULT_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Clone)]
pub struct EvolutionStore {
    conn: Arc<Mutex<Connection>>,
}

/// One memory row's lifecycle view for [`crate::curator::Curator`]: the
/// identity and value hash inputs, plus the persisted [`MemoryLifecycle`]
/// and the timestamps needed to decide (and revert) a transition.
#[derive(Clone, Debug)]
pub struct MemoryLifecycleRow {
    pub id: String,
    pub scope: String,
    pub key: String,
    pub value: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub expires_ms: Option<u64>,
    pub superseded: bool,
    pub lifecycle: MemoryLifecycle,
}

impl std::fmt::Debug for EvolutionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvolutionStore").finish_non_exhaustive()
    }
}

impl EvolutionStore {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EvolutionError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Self::from_connection(conn)
    }
    pub fn open_in_memory() -> Result<Self> {
        let conn =
            Connection::open_in_memory().map_err(|e| EvolutionError::Store(e.to_string()))?;
        Self::from_connection(conn)
    }
    fn from_connection(conn: Connection) -> Result<Self> {
        // Pragma failures are surfaced, not ignored: a store that silently
        // fell back to rollback-journal mode or no foreign keys would
        // corrupt/race in ways callers cannot see.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("evolution store mutex")
    }

    /// Run a read-only query closure against the locked connection.
    ///
    /// Provided for [`crate::metrics`], which only ever runs aggregate
    /// `SELECT`s over this schema. Every *write* still goes through a named
    /// method above/below, so invariants (JSON encoding, supersession
    /// guards, validation) cannot be bypassed through this hatch.
    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.lock();
        f(&conn)
    }

    /// Run `f` against the locked connection inside a single `IMMEDIATE`
    /// transaction. Multi-statement read-modify-write sequences (e.g.
    /// memory supersession: find live entry → insert replacement → mark
    /// old entry superseded) use this so no other writer can interleave
    /// between the read and the writes, and a failure rolls everything
    /// back instead of leaving a half-applied change.
    pub(crate) fn transact<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.lock();
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        match f(&conn) {
            Ok(value) => {
                conn.execute_batch("COMMIT")
                    .map_err(|e| EvolutionError::Store(e.to_string()))?;
                Ok(value)
            }
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }

    // ---- experiences ----
    pub fn save_experience(&self, e: &Experience) -> Result<()> {
        let tools = to_json(&e.tools_used)?;
        let errs = to_json(&e.errors)?;
        let corr = to_json(&e.corrections)?;
        let meta = to_json(&e.metadata)?;
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO experiences (id, timestamp_ms, agent_id, project_id, task_category, user_request, outcome, tools_used, errors, corrections, feedback, reusable, sensitivity, metadata)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![e.id, e.timestamp_ms as i64, e.agent_id, e.project_id, e.task_category, e.user_request, e.outcome.as_str(), tools, errs, corr, e.feedback, e.reusable as i64, e.sensitivity.as_str(), meta],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn get_experience(&self, id: &str) -> Result<Option<Experience>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,timestamp_ms,agent_id,project_id,task_category,user_request,outcome,tools_used,errors,corrections,feedback,reusable,sensitivity,metadata FROM experiences WHERE id=?1")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let row = stmt
            .query_row(params![id], map_exp)
            .optional()
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(row)
    }
    pub fn list_experiences(
        &self,
        agent_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Experience>> {
        let conn = self.lock();
        let sql = if agent_id.is_some() {
            "SELECT id,timestamp_ms,agent_id,project_id,task_category,user_request,outcome,tools_used,errors,corrections,feedback,reusable,sensitivity,metadata FROM experiences WHERE agent_id=?1 ORDER BY timestamp_ms DESC LIMIT ?2"
        } else {
            "SELECT id,timestamp_ms,agent_id,project_id,task_category,user_request,outcome,tools_used,errors,corrections,feedback,reusable,sensitivity,metadata FROM experiences ORDER BY timestamp_ms DESC LIMIT ?1"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = if let Some(a) = agent_id {
            stmt.query_map(params![a, limit as i64], map_exp)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        } else {
            stmt.query_map(params![limit as i64], map_exp)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        };
        collect_rows(rows)
    }
    pub fn count_experiences(&self) -> Result<usize> {
        count_rows(&self.lock(), "SELECT COUNT(*) FROM experiences")
    }
    /// Experiences recorded at or after `since_ms`, newest first.
    pub fn list_experiences_since(&self, since_ms: u64, limit: usize) -> Result<Vec<Experience>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT id,timestamp_ms,agent_id,project_id,task_category,user_request,outcome,tools_used,errors,corrections,feedback,reusable,sensitivity,metadata FROM experiences WHERE timestamp_ms >= ?1 ORDER BY timestamp_ms DESC LIMIT ?2")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map(params![since_ms as i64, limit as i64], map_exp)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }
    /// Experiences at or after `since_ms` that either failed outright or
    /// recorded at least one error, newest first.
    pub fn list_failed_experiences_since(
        &self,
        since_ms: u64,
        limit: usize,
    ) -> Result<Vec<Experience>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT id,timestamp_ms,agent_id,project_id,task_category,user_request,outcome,tools_used,errors,corrections,feedback,reusable,sensitivity,metadata FROM experiences WHERE timestamp_ms >= ?1 AND (outcome = 'failure' OR errors <> '[]') ORDER BY timestamp_ms DESC LIMIT ?2")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map(params![since_ms as i64, limit as i64], map_exp)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    // ---- memory entries ----
    pub fn save_memory(&self, m: &MemoryEntry) -> Result<()> {
        let conn = self.lock();
        save_memory_conn(&conn, m)
    }
    pub fn get_memory(&self, id: &str) -> Result<Option<MemoryEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries WHERE id=?1")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let row = stmt
            .query_row(params![id], map_mem)
            .optional()
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(row)
    }
    /// Find the live entry for `(layer, scope, key)`, if any. Superseded
    /// and expired rows are ignored, so a caller never "updates" a dead
    /// entry and resurrects it.
    pub fn find_memory(
        &self,
        layer: MemoryLayer,
        scope: &str,
        key: &str,
    ) -> Result<Option<MemoryEntry>> {
        let conn = self.lock();
        find_memory_conn(&conn, layer, scope, key)
    }
    /// Text search over live memory entries.
    ///
    /// The `LIKE` match and the superseded/expired filter run in SQL
    /// *before* `LIMIT`, so a page is never short merely because dead
    /// rows filled it. Matching is done with `LIKE … ESCAPE` on both
    /// `key` and `value` (case-insensitive for ASCII via `LOWER`).
    pub fn search_memory(
        &self,
        scope: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let conn = self.lock();
        let now = crate::types::now_ms() as i64;
        let pattern = format!("%{}%", escape_like(&query.to_lowercase()));
        let sql = if scope.is_some() {
            "SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries WHERE scope=?1 AND superseded_by IS NULL AND lifecycle <> 'archived' AND (expires_ms IS NULL OR expires_ms > ?2) AND (LOWER(key) LIKE ?3 ESCAPE '\\' OR LOWER(value) LIKE ?3 ESCAPE '\\') ORDER BY updated_ms DESC LIMIT ?4"
        } else {
            "SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries WHERE superseded_by IS NULL AND lifecycle <> 'archived' AND (expires_ms IS NULL OR expires_ms > ?1) AND (LOWER(key) LIKE ?2 ESCAPE '\\' OR LOWER(value) LIKE ?2 ESCAPE '\\') ORDER BY updated_ms DESC LIMIT ?3"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = if let Some(s) = scope {
            stmt.query_map(params![s, now, pattern, limit as i64], map_mem)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        } else {
            stmt.query_map(params![now, pattern, limit as i64], map_mem)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        };
        collect_rows(rows)
    }
    /// All live memory entries for a scope (superseded/expired excluded).
    pub fn list_memory(&self, scope: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let conn = self.lock();
        let now = crate::types::now_ms() as i64;
        let sql = if scope.is_some() {
            "SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries WHERE scope=?1 AND superseded_by IS NULL AND lifecycle <> 'archived' AND (expires_ms IS NULL OR expires_ms > ?2) ORDER BY updated_ms DESC"
        } else {
            "SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries WHERE superseded_by IS NULL AND lifecycle <> 'archived' AND (expires_ms IS NULL OR expires_ms > ?1) ORDER BY updated_ms DESC"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = if let Some(s) = scope {
            stmt.query_map(params![s, now], map_mem)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        } else {
            stmt.query_map(params![now], map_mem)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        };
        collect_rows(rows)
    }
    /// All memory rows including superseded/expired ones (audit/debug).
    pub fn list_memory_all(&self, scope: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let conn = self.lock();
        let sql = if scope.is_some() {
            "SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries WHERE scope=?1 ORDER BY updated_ms DESC"
        } else {
            "SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries ORDER BY updated_ms DESC"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = if let Some(s) = scope {
            stmt.query_map(params![s], map_mem)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        } else {
            stmt.query_map([], map_mem)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        };
        collect_rows(rows)
    }
    pub fn delete_memory(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn
            .execute("DELETE FROM memory_entries WHERE id=?1", params![id])
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(n > 0)
    }
    pub fn count_memory(&self) -> Result<usize> {
        let now = crate::types::now_ms() as i64;
        let conn = self.lock();
        count_rows_with(
            &conn,
            "SELECT COUNT(*) FROM memory_entries WHERE superseded_by IS NULL AND lifecycle <> 'archived' AND (expires_ms IS NULL OR expires_ms > ?1)",
            params![now],
        )
    }

    // ---- memory lifecycle ----
    /// Every memory row's lifecycle view, all scopes or one. Unlike
    /// [`Self::list_memory`] this includes archived, superseded, and expired
    /// rows, because the curator needs to see state it is allowed to move.
    pub fn list_memory_lifecycle(&self, scope: Option<&str>) -> Result<Vec<MemoryLifecycleRow>> {
        let conn = self.lock();
        list_memory_lifecycle_conn(&conn, scope)
    }
    pub fn memory_lifecycle(&self, id: &str) -> Result<Option<MemoryLifecycle>> {
        let conn = self.lock();
        memory_lifecycle_conn(&conn, id)
    }
    pub fn set_memory_lifecycle(&self, id: &str, lifecycle: MemoryLifecycle) -> Result<bool> {
        let conn = self.lock();
        set_memory_lifecycle_conn(&conn, id, lifecycle)
    }

    // ---- proposals ----
    pub fn save_proposal(&self, p: &LearningProposal) -> Result<()> {
        let details = to_json(&p.details)?;
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO proposals (id,experience_id,agent_id,kind,summary,details,confidence,requires_approval,created_ms,status)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![p.id, p.experience_id, p.agent_id, p.kind.as_str(), p.summary, details, p.confidence, p.requires_approval as i64, p.created_ms as i64, p.status.as_str()],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn list_proposals(&self, status: Option<&str>) -> Result<Vec<LearningProposal>> {
        let conn = self.lock();
        let sql = if status.is_some() {
            "SELECT id,experience_id,agent_id,kind,summary,details,confidence,requires_approval,created_ms,status FROM proposals WHERE status=?1 ORDER BY created_ms DESC"
        } else {
            "SELECT id,experience_id,agent_id,kind,summary,details,confidence,requires_approval,created_ms,status FROM proposals ORDER BY created_ms DESC"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = if let Some(s) = status {
            stmt.query_map(params![s], map_prop)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        } else {
            stmt.query_map([], map_prop)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        };
        collect_rows(rows)
    }
    pub fn get_proposal(&self, id: &str) -> Result<Option<LearningProposal>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,experience_id,agent_id,kind,summary,details,confidence,requires_approval,created_ms,status FROM proposals WHERE id=?1")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        stmt.query_row(params![id], map_prop)
            .optional()
            .map_err(|e| EvolutionError::Store(e.to_string()))
    }

    // ---- reflection jobs ----
    pub fn save_job(&self, j: &ReflectionJob) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO reflection_jobs (id,experience_id,agent_id,status,attempts,created_ms,updated_ms,error)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![j.id, j.experience_id, j.agent_id, j.status.as_str(), j.attempts, j.created_ms as i64, j.updated_ms as i64, j.error],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn list_pending_jobs(&self) -> Result<Vec<ReflectionJob>> {
        let conn = self.lock();
        // `failed` is deliberately absent: process_pending retries whatever this returns, and a
        // failed job belongs to list_failed_jobs — counting it as pending retried it forever.
        let mut stmt = conn.prepare("SELECT id,experience_id,agent_id,status,attempts,created_ms,updated_ms,error FROM reflection_jobs WHERE status IN ('pending','running') ORDER BY created_ms ASC")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map([], map_job)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }
    /// Reflection jobs that ended in `failed`, most recently updated first.
    pub fn list_failed_jobs(&self, limit: usize) -> Result<Vec<ReflectionJob>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,experience_id,agent_id,status,attempts,created_ms,updated_ms,error FROM reflection_jobs WHERE status='failed' ORDER BY updated_ms DESC LIMIT ?1")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], map_job)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    // ---- hive lessons ----
    pub fn save_hive_lesson(&self, h: &HiveLesson) -> Result<()> {
        let agents = to_json(&h.agents)?;
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO hive_lessons (id,task_id,agents,outcome,lesson,pattern,created_ms) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![h.id, h.task_id, agents, h.outcome, h.lesson, h.pattern, h.created_ms as i64],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn list_hive_lessons(&self) -> Result<Vec<HiveLesson>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,task_id,agents,outcome,lesson,pattern,created_ms FROM hive_lessons ORDER BY created_ms DESC")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map([], map_hive)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    // ---- candidates & evaluations ----
    pub fn save_candidate(&self, c: &OptimizationCandidate) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO candidates (id,target,parent_version,change_description,content,created_ms,status,rejection_reason) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![c.id, c.target, c.parent_version, c.change_description, c.content, c.created_ms as i64, c.status.as_str(), c.rejection_reason],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn get_candidate(&self, id: &str) -> Result<Option<OptimizationCandidate>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,target,parent_version,change_description,content,created_ms,status,rejection_reason FROM candidates WHERE id=?1")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        stmt.query_row(params![id], map_cand)
            .optional()
            .map_err(|e| EvolutionError::Store(e.to_string()))
    }
    pub fn list_candidates(&self, target: Option<&str>) -> Result<Vec<OptimizationCandidate>> {
        let conn = self.lock();
        let sql = if target.is_some() {
            "SELECT id,target,parent_version,change_description,content,created_ms,status,rejection_reason FROM candidates WHERE target=?1 ORDER BY created_ms DESC"
        } else {
            "SELECT id,target,parent_version,change_description,content,created_ms,status,rejection_reason FROM candidates ORDER BY created_ms DESC"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = if let Some(t) = target {
            stmt.query_map(params![t], map_cand)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        } else {
            stmt.query_map([], map_cand)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        };
        collect_rows(rows)
    }
    pub fn save_evaluation(&self, e: &EvaluationResult) -> Result<()> {
        if e.id.is_empty() {
            return Err(EvolutionError::Validation(
                "evaluation id must not be empty".to_string(),
            ));
        }
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO evaluations (id,candidate_id,dataset,metrics,score,passed,notes,created_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![e.id, e.candidate_id, e.dataset, to_json(&e.metrics)?, e.score, e.passed as i64, e.notes, e.created_ms as i64],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn list_evaluations(&self, candidate_id: &str) -> Result<Vec<EvaluationResult>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,candidate_id,dataset,metrics,score,passed,notes,created_ms FROM evaluations WHERE candidate_id=?1 ORDER BY created_ms ASC")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map(params![candidate_id], map_eval)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    // ---- skill versions ----
    pub fn save_skill_version(&self, v: &crate::types::SkillVersion) -> crate::error::Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO skill_versions (skill_name,version,content,created_ms,parent_version,change_description) VALUES (?1,?2,?3,?4,?5,?6)",
            params![v.skill_name, v.version as i64, v.content, v.created_ms as i64, v.parent_version.map(|x|x as i64), v.change_description],
        ).map_err(|e| crate::error::EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn list_skill_versions(
        &self,
        name: &str,
    ) -> crate::error::Result<Vec<crate::types::SkillVersion>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT skill_name,version,content,created_ms,parent_version,change_description FROM skill_versions WHERE skill_name=?1 ORDER BY version ASC")
            .map_err(|e| crate::error::EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map(params![name], |r| {
                Ok(crate::types::SkillVersion {
                    skill_name: r.get(0)?,
                    version: r.get::<_, i64>(1)? as u32,
                    content: r.get(2)?,
                    created_ms: r.get::<_, i64>(3)? as u64,
                    parent_version: r.get::<_, Option<i64>>(4)?.map(|x| x as u32),
                    change_description: r.get(5)?,
                })
            })
            .map_err(|e| crate::error::EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    /// Newest skill-version rows across *all* skills. [`Self::list_skill_versions`]
    /// is per-skill; this is the cross-skill audit/history view.
    pub fn list_recent_skill_versions(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::types::SkillVersion>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT skill_name,version,content,created_ms,parent_version,change_description FROM skill_versions ORDER BY created_ms DESC, skill_name ASC, version DESC LIMIT ?1")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(crate::types::SkillVersion {
                    skill_name: r.get(0)?,
                    version: r.get::<_, i64>(1)? as u32,
                    content: r.get(2)?,
                    created_ms: r.get::<_, i64>(3)? as u64,
                    parent_version: r.get::<_, Option<i64>>(4)?.map(|x| x as u32),
                    change_description: r.get(5)?,
                })
            })
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    /// Highest recorded version per skill — the "current version" map used
    /// by audits and recovery fingerprints.
    pub fn latest_skill_versions(&self) -> Result<Vec<(String, u32)>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT skill_name, MAX(version) FROM skill_versions GROUP BY skill_name ORDER BY skill_name",
            )
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
            })
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    /// Remove one recorded skill version. Used to compensate a failed
    /// `SKILL.md` materialization so the version table never claims a
    /// version the skill directory does not contain.
    pub fn delete_skill_version(&self, skill_name: &str, version: u32) -> Result<bool> {
        let conn = self.lock();
        let n = conn
            .execute(
                "DELETE FROM skill_versions WHERE skill_name=?1 AND version=?2",
                params![skill_name, version as i64],
            )
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(n > 0)
    }

    // ---- learning events ----
    pub fn save_event(&self, e: &LearningEvent) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO learning_events (id,timestamp_ms,agent_id,kind,summary,payload) VALUES (?1,?2,?3,?4,?5,?6)",
            params![e.id, e.timestamp_ms as i64, e.agent_id, e.kind, e.summary, to_json(&e.payload)?],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn list_events(&self, agent_id: Option<&str>, limit: usize) -> Result<Vec<LearningEvent>> {
        let conn = self.lock();
        let sql = if agent_id.is_some() {
            "SELECT id,timestamp_ms,agent_id,kind,summary,payload FROM learning_events WHERE agent_id=?1 ORDER BY timestamp_ms DESC LIMIT ?2"
        } else {
            "SELECT id,timestamp_ms,agent_id,kind,summary,payload FROM learning_events ORDER BY timestamp_ms DESC LIMIT ?1"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = if let Some(a) = agent_id {
            stmt.query_map(params![a, limit as i64], map_event)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        } else {
            stmt.query_map(params![limit as i64], map_event)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        };
        collect_rows(rows)
    }

    /// Total learning events, uncapped — `list_events` is a page, not a count.
    pub fn count_events(&self) -> Result<usize> {
        let conn = self.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM learning_events", [], |r| r.get(0))
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(count.max(0) as usize)
    }

    /// Events of one `kind` (e.g. `self_check`), uncapped.
    pub fn count_events_of_kind(&self, kind: &str) -> Result<usize> {
        count_rows_with(
            &self.lock(),
            "SELECT COUNT(*) FROM learning_events WHERE kind=?1",
            params![kind],
        )
    }

    /// Text search over learning events (`summary` or `payload`), newest
    /// first. Same escape-then-`LIMIT` discipline as [`Self::search_memory`]:
    /// the `LIKE` match runs in SQL before the limit so a page is never short
    /// because of rows that did not match.
    pub fn search_events(&self, query: &str, limit: usize) -> Result<Vec<LearningEvent>> {
        let conn = self.lock();
        let pattern = format!("%{}%", escape_like(&query.to_lowercase()));
        let mut stmt = conn.prepare("SELECT id,timestamp_ms,agent_id,kind,summary,payload FROM learning_events WHERE LOWER(summary) LIKE ?1 ESCAPE '\\' OR LOWER(payload) LIKE ?1 ESCAPE '\\' ORDER BY timestamp_ms DESC LIMIT ?2")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], map_event)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    // ---- engine state records ----
    /// Persist a JSON state record for an engine whose bookkeeping must
    /// survive a restart (autonomy approvals, evaluation history, MAPE-K
    /// knowledge, improvement executions).
    ///
    /// `namespace` scopes the record to one engine concern and `id`
    /// identifies it inside that namespace. Re-saving the same
    /// `(namespace, id)` replaces the payload and bumps `updated_ms`;
    /// `created_ms` keeps the first write's timestamp. Records are never
    /// deleted here — engines that need removal do it through their own
    /// domain tables.
    pub fn save_engine_record(
        &self,
        namespace: &str,
        id: &str,
        payload: &serde_json::Value,
    ) -> Result<()> {
        let conn = self.lock();
        save_engine_record_conn(&conn, namespace, id, payload)
    }
    /// All records in a namespace as `(id, payload)`, oldest first — the
    /// order engines append to their in-memory lists.
    ///
    /// A row whose payload does not decode is an error, not a skipped row:
    /// silently dropping it would resurrect stale in-memory state on the
    /// next save.
    pub fn list_engine_records(&self, namespace: &str) -> Result<Vec<(String, serde_json::Value)>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id,payload FROM engine_records WHERE namespace=?1 ORDER BY created_ms ASC, id ASC",
            )
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map(params![namespace], |r| {
                let id: String = r.get(0)?;
                let payload: String = r.get(1)?;
                let value: serde_json::Value = serde_json::from_str(&payload).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            e.to_string(),
                        )),
                    )
                })?;
                Ok((id, value))
            })
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        collect_rows(rows)
    }

    /// Bound table growth: delete rows older than `max_age_ms` from the
    /// append-only, high-volume tables. Returns the number of rows removed.
    ///
    /// Only *terminal* rows are eligible — `reflection_jobs` keeps
    /// `pending`/`running` work and `proposals` keeps `pending`/`approved`
    /// ones awaiting an operator, because deleting live work would corrupt
    /// in-flight flows. `memory_entries` keeps live rows and only sheds
    /// superseded ones past the horizon; `skill_versions` and
    /// `evaluations`/`candidates`/`hive_lessons` are bounded by their
    /// parents and are kept as history.
    ///
    /// Mirrors the daemon's `prune_runs` pattern: callers invoke it after
    /// settling work (e.g. [`crate::reflection::ReflectionEngine::process_pending`])
    /// rather than on every insert.
    pub fn prune_older_than(&self, max_age_ms: u64) -> Result<usize> {
        let cutoff = now_ms().saturating_sub(max_age_ms) as i64;
        let conn = self.lock();
        let mut removed = 0usize;
        for sql in [
            "DELETE FROM experiences WHERE timestamp_ms < ?1",
            "DELETE FROM learning_events WHERE timestamp_ms < ?1",
            "DELETE FROM reflection_jobs WHERE status IN ('done','failed') AND updated_ms < ?1",
            "DELETE FROM proposals WHERE status IN ('rejected','applied') AND created_ms < ?1",
            "DELETE FROM memory_entries WHERE superseded_by IS NOT NULL AND updated_ms < ?1",
        ] {
            removed += conn
                .execute(sql, params![cutoff])
                .map_err(|e| EvolutionError::Store(e.to_string()))?;
        }
        Ok(removed)
    }
}

// ---------- checked (de)serialisation helpers ----------

fn to_json<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| EvolutionError::Serde(e.to_string()))
}

fn from_json<T: for<'de> serde::Deserialize<'de>>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| EvolutionError::Serde(e.to_string()))
}

/// Serialize to a `serde_json::Value` for [`EvolutionStore::save_engine_record`].
pub(crate) fn to_json_value<T: serde::Serialize>(v: &T) -> Result<serde_json::Value> {
    serde_json::to_value(v).map_err(|e| EvolutionError::Serde(e.to_string()))
}

/// Deserialize a `serde_json::Value` read back from [`EvolutionStore::list_engine_records`].
pub(crate) fn from_json_value<T: for<'de> serde::Deserialize<'de>>(
    v: serde_json::Value,
) -> Result<T> {
    serde_json::from_value(v).map_err(|e| EvolutionError::Serde(e.to_string()))
}

/// Insert or update a memory entry on an already-locked connection.
///
/// `pub(crate)` so [`EvolutionStore::transact`] callers (e.g.
/// [`crate::memory::LayeredMemory::remember`]) apply the same SQL and the
/// same self-supersession guard as [`EvolutionStore::save_memory`]. The
/// update arm leaves `lifecycle` alone: rewriting a row's content is not a
/// curation decision, so an archived row stays archived until
/// [`crate::curator::Curator`] moves it.
pub(crate) fn save_memory_conn(conn: &Connection, m: &MemoryEntry) -> Result<()> {
    // Guard the supersession chain: an entry must never supersede
    // itself (a self-loop makes the entry invisible to live listings
    // while pointing at itself as its replacement).
    if let Some(by) = &m.superseded_by
        && by == &m.id
    {
        return Err(EvolutionError::Validation(format!(
            "memory entry {} cannot supersede itself",
            m.id
        )));
    }
    conn.execute(
        "INSERT INTO memory_entries (id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(id) DO UPDATE SET layer=excluded.layer, scope=excluded.scope, key=excluded.key,
             value=excluded.value, status=excluded.status, confidence=excluded.confidence,
             source_experience=excluded.source_experience, provenance=excluded.provenance,
             created_ms=excluded.created_ms, updated_ms=excluded.updated_ms,
             expires_ms=excluded.expires_ms, superseded_by=excluded.superseded_by",
        params![m.id, m.layer.as_str(), m.scope, m.key, m.value, m.status.as_str(), m.confidence, m.source_experience, m.provenance, m.created_ms as i64, m.updated_ms as i64, m.expires_ms.map(|x|x as i64), m.superseded_by],
    ).map_err(|e| EvolutionError::Store(e.to_string()))?;
    Ok(())
}

/// Find the live entry for `(layer, scope, key)` on an already-locked
/// connection — the same query as [`EvolutionStore::find_memory`], for use
/// inside [`EvolutionStore::transact`].
pub(crate) fn find_memory_conn(
    conn: &Connection,
    layer: MemoryLayer,
    scope: &str,
    key: &str,
) -> Result<Option<MemoryEntry>> {
    let now = crate::types::now_ms() as i64;
    let mut stmt = conn.prepare("SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries WHERE layer=?1 AND scope=?2 AND key=?3 AND superseded_by IS NULL AND lifecycle <> 'archived' AND (expires_ms IS NULL OR expires_ms > ?4) ORDER BY updated_ms DESC LIMIT 1")
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    let row = stmt
        .query_row(params![layer.as_str(), scope, key, now], map_mem)
        .optional()
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    Ok(row)
}

/// Persist an engine record on an already-locked connection.
pub(crate) fn save_engine_record_conn(
    conn: &Connection,
    namespace: &str,
    id: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    if namespace.is_empty() || id.is_empty() {
        return Err(EvolutionError::Validation(
            "engine record namespace and id must not be empty".to_string(),
        ));
    }
    let now = now_ms() as i64;
    conn.execute(
        "INSERT INTO engine_records (namespace,id,payload,created_ms,updated_ms) VALUES (?1,?2,?3,?4,?4)
         ON CONFLICT(namespace,id) DO UPDATE SET payload=excluded.payload, updated_ms=excluded.updated_ms",
        params![namespace, id, to_json(payload)?, now],
    )
    .map_err(|e| EvolutionError::Store(e.to_string()))?;
    Ok(())
}

/// Read one engine record's payload from an already-locked connection.
pub(crate) fn find_engine_record_conn(
    conn: &Connection,
    namespace: &str,
    id: &str,
) -> Result<Option<serde_json::Value>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM engine_records WHERE namespace=?1 AND id=?2",
            params![namespace, id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    payload
        .map(|p| serde_json::from_str(&p).map_err(|e| EvolutionError::Serde(e.to_string())))
        .transpose()
}

pub(crate) fn list_memory_lifecycle_conn(
    conn: &Connection,
    scope: Option<&str>,
) -> Result<Vec<MemoryLifecycleRow>> {
    let sql = if scope.is_some() {
        "SELECT id,scope,key,value,created_ms,updated_ms,expires_ms,superseded_by,lifecycle FROM memory_entries WHERE scope=?1 ORDER BY updated_ms DESC, id ASC"
    } else {
        "SELECT id,scope,key,value,created_ms,updated_ms,expires_ms,superseded_by,lifecycle FROM memory_entries ORDER BY updated_ms DESC, id ASC"
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    let rows = if let Some(s) = scope {
        stmt.query_map(params![s], map_memory_lifecycle)
            .map_err(|e| EvolutionError::Store(e.to_string()))?
    } else {
        stmt.query_map([], map_memory_lifecycle)
            .map_err(|e| EvolutionError::Store(e.to_string()))?
    };
    collect_rows(rows)
}

pub(crate) fn find_memory_lifecycle_conn(
    conn: &Connection,
    id: &str,
) -> Result<Option<MemoryLifecycleRow>> {
    let mut stmt = conn
        .prepare("SELECT id,scope,key,value,created_ms,updated_ms,expires_ms,superseded_by,lifecycle FROM memory_entries WHERE id=?1")
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    stmt.query_row(params![id], map_memory_lifecycle)
        .optional()
        .map_err(|e| EvolutionError::Store(e.to_string()))
}

pub(crate) fn memory_lifecycle_conn(
    conn: &Connection,
    id: &str,
) -> Result<Option<MemoryLifecycle>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT lifecycle FROM memory_entries WHERE id=?1",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    match raw {
        Some(raw) => MemoryLifecycle::parse(&raw)
            .map(Some)
            .ok_or_else(|| EvolutionError::Validation(format!("unknown memory lifecycle: {raw}"))),
        None => Ok(None),
    }
}

pub(crate) fn set_memory_lifecycle_conn(
    conn: &Connection,
    id: &str,
    lifecycle: MemoryLifecycle,
) -> Result<bool> {
    let n = conn
        .execute(
            "UPDATE memory_entries SET lifecycle=?2 WHERE id=?1",
            params![id, lifecycle.as_str()],
        )
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    Ok(n > 0)
}

/// Mark a memory row as used at `at_ms`. This is the only thing that moves
/// an entry back toward `active`; the curator turns the newer timestamp into
/// a lifecycle transition on its next sweep.
pub(crate) fn mark_memory_used_conn(conn: &Connection, id: &str, at_ms: u64) -> Result<bool> {
    let n = conn
        .execute(
            "UPDATE memory_entries SET updated_ms=?2 WHERE id=?1",
            params![id, at_ms as i64],
        )
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    Ok(n > 0)
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| EvolutionError::Store(e.to_string()))?);
    }
    Ok(out)
}

fn count_rows(conn: &Connection, sql: &str) -> Result<usize> {
    let n: i64 = conn
        .query_row(sql, [], |r| r.get(0))
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    Ok(n as usize)
}

fn count_rows_with(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<usize> {
    let n: i64 = conn
        .query_row(sql, params, |r| r.get(0))
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    Ok(n as usize)
}

/// Escape `%`, `_` and `\` so a user query cannot act as a LIKE pattern.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn map_exp(r: &rusqlite::Row) -> rusqlite::Result<Experience> {
    let outcome_s: String = r.get(6)?;
    let sensitivity_s: String = r.get(12)?;
    Ok(Experience {
        id: r.get(0)?,
        timestamp_ms: r.get::<_, i64>(1)? as u64,
        agent_id: r.get(2)?,
        project_id: r.get(3)?,
        task_category: r.get(4)?,
        user_request: r.get(5)?,
        outcome: ExperienceOutcome::parse(&outcome_s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown experience outcome: {outcome_s}"),
                )),
            )
        })?,
        tools_used: from_json(&r.get::<_, String>(7)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?,
        errors: from_json(&r.get::<_, String>(8)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?,
        corrections: from_json(&r.get::<_, String>(9)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?,
        feedback: r.get(10)?,
        reusable: r.get::<_, i64>(11)? != 0,
        sensitivity: Sensitivity::parse(&sensitivity_s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                12,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown sensitivity: {sensitivity_s}"),
                )),
            )
        })?,
        metadata: from_json(&r.get::<_, String>(13)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                13,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?,
    })
}

fn map_mem(r: &rusqlite::Row) -> rusqlite::Result<MemoryEntry> {
    let layer_s: String = r.get(1)?;
    let status_s: String = r.get(5)?;
    Ok(MemoryEntry {
        id: r.get(0)?,
        layer: MemoryLayer::parse(&layer_s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown memory layer: {layer_s}"),
                )),
            )
        })?,
        scope: r.get(2)?,
        key: r.get(3)?,
        value: r.get(4)?,
        status: KnowledgeStatus::parse(&status_s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown knowledge status: {status_s}"),
                )),
            )
        })?,
        confidence: r.get(6)?,
        source_experience: r.get(7)?,
        provenance: r.get(8)?,
        created_ms: r.get::<_, i64>(9)? as u64,
        updated_ms: r.get::<_, i64>(10)? as u64,
        expires_ms: r.get::<_, Option<i64>>(11)?.map(|x| x as u64),
        superseded_by: r.get(12)?,
    })
}

fn map_memory_lifecycle(r: &rusqlite::Row) -> rusqlite::Result<MemoryLifecycleRow> {
    let lifecycle_s: String = r.get(8)?;
    Ok(MemoryLifecycleRow {
        id: r.get(0)?,
        scope: r.get(1)?,
        key: r.get(2)?,
        value: r.get(3)?,
        created_ms: r.get::<_, i64>(4)? as u64,
        updated_ms: r.get::<_, i64>(5)? as u64,
        expires_ms: r.get::<_, Option<i64>>(6)?.map(|x| x as u64),
        superseded: r.get::<_, Option<String>>(7)?.is_some(),
        lifecycle: MemoryLifecycle::parse(&lifecycle_s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown memory lifecycle: {lifecycle_s}"),
                )),
            )
        })?,
    })
}

fn map_prop(r: &rusqlite::Row) -> rusqlite::Result<LearningProposal> {
    let kind_s: String = r.get(3)?;
    let status_s: String = r.get(9)?;
    Ok(LearningProposal {
        id: r.get(0)?,
        experience_id: r.get(1)?,
        agent_id: r.get(2)?,
        kind: ProposalKind::parse(&kind_s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown proposal kind: {kind_s}"),
                )),
            )
        })?,
        summary: r.get(4)?,
        details: from_json(&r.get::<_, String>(5)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?,
        confidence: r.get(6)?,
        requires_approval: r.get::<_, i64>(7)? != 0,
        created_ms: r.get::<_, i64>(8)? as u64,
        status: ProposalStatus::parse(&status_s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown proposal status: {status_s}"),
                )),
            )
        })?,
    })
}

fn map_job(r: &rusqlite::Row) -> rusqlite::Result<ReflectionJob> {
    let status_s: String = r.get(3)?;
    Ok(ReflectionJob {
        id: r.get(0)?,
        experience_id: r.get(1)?,
        agent_id: r.get(2)?,
        status: ReflectionJobStatus::parse(&status_s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown reflection job status: {status_s}"),
                )),
            )
        })?,
        attempts: r.get::<_, i64>(4)? as u32,
        created_ms: r.get::<_, i64>(5)? as u64,
        updated_ms: r.get::<_, i64>(6)? as u64,
        error: r.get(7)?,
    })
}

fn map_cand(r: &rusqlite::Row) -> rusqlite::Result<OptimizationCandidate> {
    let status_s: String = r.get(6)?;
    Ok(OptimizationCandidate {
        id: r.get(0)?,
        target: r.get(1)?,
        parent_version: r.get(2)?,
        change_description: r.get(3)?,
        content: r.get(4)?,
        created_ms: r.get::<_, i64>(5)? as u64,
        status: CandidateStatus::parse(&status_s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown candidate status: {status_s}"),
                )),
            )
        })?,
        rejection_reason: r.get(7)?,
    })
}

fn map_hive(r: &rusqlite::Row) -> rusqlite::Result<HiveLesson> {
    Ok(HiveLesson {
        id: r.get(0)?,
        task_id: r.get(1)?,
        agents: from_json(&r.get::<_, String>(2)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?,
        outcome: r.get(3)?,
        lesson: r.get(4)?,
        pattern: r.get(5)?,
        created_ms: r.get::<_, i64>(6)? as u64,
    })
}

fn map_eval(r: &rusqlite::Row) -> rusqlite::Result<EvaluationResult> {
    Ok(EvaluationResult {
        id: r.get(0)?,
        candidate_id: r.get(1)?,
        dataset: r.get(2)?,
        metrics: from_json(&r.get::<_, String>(3)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?,
        score: r.get(4)?,
        passed: r.get::<_, i64>(5)? != 0,
        notes: r.get(6)?,
        created_ms: r.get::<_, i64>(7)? as u64,
    })
}

fn map_event(r: &rusqlite::Row) -> rusqlite::Result<LearningEvent> {
    Ok(LearningEvent {
        id: r.get(0)?,
        timestamp_ms: r.get::<_, i64>(1)? as u64,
        agent_id: r.get(2)?,
        kind: r.get(3)?,
        summary: r.get(4)?,
        payload: from_json(&r.get::<_, String>(5)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?,
    })
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|e| EvolutionError::Store(e.to_string()))?;

    if version < 1 {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS experiences (
                id TEXT PRIMARY KEY,
                timestamp_ms INTEGER NOT NULL,
                agent_id TEXT NOT NULL,
                project_id TEXT,
                task_category TEXT NOT NULL,
                user_request TEXT NOT NULL,
                outcome TEXT NOT NULL,
                tools_used TEXT NOT NULL,
                errors TEXT NOT NULL,
                corrections TEXT NOT NULL,
                feedback TEXT,
                reusable INTEGER NOT NULL,
                sensitivity TEXT NOT NULL,
                metadata TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS memory_entries (
                id TEXT PRIMARY KEY,
                layer TEXT NOT NULL,
                scope TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                status TEXT NOT NULL,
                confidence REAL NOT NULL,
                source_experience TEXT,
                provenance TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                updated_ms INTEGER NOT NULL,
                expires_ms INTEGER,
                superseded_by TEXT,
                lifecycle TEXT NOT NULL DEFAULT 'active'
            );
            CREATE INDEX IF NOT EXISTS idx_mem_scope ON memory_entries(scope);
            CREATE INDEX IF NOT EXISTS idx_mem_layer ON memory_entries(layer);
            CREATE INDEX IF NOT EXISTS idx_mem_live ON memory_entries(scope, superseded_by, expires_ms);
            CREATE TABLE IF NOT EXISTS proposals (
                id TEXT PRIMARY KEY,
                experience_id TEXT,
                agent_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                summary TEXT NOT NULL,
                details TEXT NOT NULL,
                confidence REAL NOT NULL,
                requires_approval INTEGER NOT NULL,
                created_ms INTEGER NOT NULL,
                status TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS reflection_jobs (
                id TEXT PRIMARY KEY,
                experience_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                status TEXT NOT NULL,
                attempts INTEGER NOT NULL,
                created_ms INTEGER NOT NULL,
                updated_ms INTEGER NOT NULL,
                error TEXT
            );
            CREATE TABLE IF NOT EXISTS hive_lessons (
                id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                agents TEXT NOT NULL,
                outcome TEXT NOT NULL,
                lesson TEXT NOT NULL,
                pattern TEXT NOT NULL,
                created_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS candidates (
                id TEXT PRIMARY KEY,
                target TEXT NOT NULL,
                parent_version TEXT,
                change_description TEXT NOT NULL,
                content TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                status TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS evaluations (
                candidate_id TEXT NOT NULL,
                dataset TEXT NOT NULL,
                metrics TEXT NOT NULL,
                score REAL NOT NULL,
                passed INTEGER NOT NULL,
                notes TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                PRIMARY KEY (candidate_id, dataset)
            );
            CREATE TABLE IF NOT EXISTS learning_events (
                id TEXT PRIMARY KEY,
                timestamp_ms INTEGER NOT NULL,
                agent_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                summary TEXT NOT NULL,
                payload TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS skill_versions (
                skill_name TEXT NOT NULL,
                version INTEGER NOT NULL,
                content TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                parent_version INTEGER,
                change_description TEXT NOT NULL,
                PRIMARY KEY (skill_name, version)
            );
            "#,
        )
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
        conn.pragma_update(None, "user_version", 1i64)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
    }

    if version < 2 {
        // v2: evaluations gain their own primary key + `id`; candidates
        // gain `rejection_reason`. Both are additive and preserve rows.
        //
        // Recreate `evaluations` with a surrogate PK. Existing rows are
        // copied with a generated id (`candidate_id || '/' || dataset`)
        // so history is kept instead of dropped.
        let has_eval_id: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('evaluations') WHERE name='id'")
            .and_then(|mut s| s.exists([]))
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        if !has_eval_id {
            conn.execute_batch(
                r#"
                CREATE TABLE evaluations_v2 (
                    id TEXT PRIMARY KEY,
                    candidate_id TEXT NOT NULL,
                    dataset TEXT NOT NULL,
                    metrics TEXT NOT NULL,
                    score REAL NOT NULL,
                    passed INTEGER NOT NULL,
                    notes TEXT NOT NULL,
                    created_ms INTEGER NOT NULL
                );
                INSERT INTO evaluations_v2 (id, candidate_id, dataset, metrics, score, passed, notes, created_ms)
                    SELECT candidate_id || '/' || dataset, candidate_id, dataset, metrics, score, passed, notes, created_ms FROM evaluations;
                DROP TABLE evaluations;
                ALTER TABLE evaluations_v2 RENAME TO evaluations;
                CREATE INDEX IF NOT EXISTS idx_eval_candidate ON evaluations(candidate_id);
                "#,
            )
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        }
        let has_rejection_reason: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('candidates') WHERE name='rejection_reason'")
            .and_then(|mut s| s.exists([]))
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        if !has_rejection_reason {
            conn.execute(
                "ALTER TABLE candidates ADD COLUMN rejection_reason TEXT",
                [],
            )
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        }
        conn.pragma_update(None, "user_version", 2i64)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
    }

    if version < 3 {
        // v3: generic JSON state records for engines whose bookkeeping must
        // survive a restart — autonomy approval requests, self-evaluation
        // benchmarks/results/metrics, MAPE-K knowledge, and improvement
        // executions. Additive only; existing tables are untouched.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS engine_records (
                namespace TEXT NOT NULL,
                id TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                updated_ms INTEGER NOT NULL,
                PRIMARY KEY (namespace, id)
            );
            CREATE INDEX IF NOT EXISTS idx_engine_records_ns ON engine_records(namespace);
            "#,
        )
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
        conn.pragma_update(None, "user_version", 3i64)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
    }

    if version < 4 {
        // v4: memory entries carry a lifecycle (`active`/`stale`/`archived`)
        // so unused facts can be aged out of live prompts without deleting
        // the row. Additive; a fresh v1 schema already has the column.
        let has_lifecycle: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('memory_entries') WHERE name='lifecycle'")
            .and_then(|mut s| s.exists([]))
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        if !has_lifecycle {
            conn.execute(
                "ALTER TABLE memory_entries ADD COLUMN lifecycle TEXT NOT NULL DEFAULT 'active'",
                [],
            )
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        }
        conn.pragma_update(None, "user_version", 4i64)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn experience(id: &str, timestamp_ms: u64) -> Experience {
        Experience {
            id: id.to_string(),
            timestamp_ms,
            agent_id: "a".to_string(),
            project_id: None,
            task_category: "t".to_string(),
            user_request: "u".to_string(),
            outcome: ExperienceOutcome::Success,
            tools_used: vec![],
            errors: vec![],
            corrections: vec![],
            feedback: None,
            reusable: false,
            sensitivity: Sensitivity::Public,
            metadata: serde_json::json!({}),
        }
    }

    fn event(id: &str, timestamp_ms: u64) -> LearningEvent {
        LearningEvent {
            id: id.to_string(),
            timestamp_ms,
            agent_id: "a".to_string(),
            kind: "k".to_string(),
            summary: "s".to_string(),
            payload: serde_json::json!({}),
        }
    }

    fn job(id: &str, status: ReflectionJobStatus, updated_ms: u64) -> ReflectionJob {
        ReflectionJob {
            id: id.to_string(),
            experience_id: "exp".to_string(),
            agent_id: "a".to_string(),
            status,
            attempts: 1,
            created_ms: updated_ms,
            updated_ms,
            error: None,
        }
    }

    fn proposal(id: &str, status: ProposalStatus, created_ms: u64) -> LearningProposal {
        LearningProposal {
            id: id.to_string(),
            experience_id: None,
            agent_id: "a".to_string(),
            kind: ProposalKind::RecordIssue,
            summary: "s".to_string(),
            details: serde_json::json!({}),
            confidence: 0.5,
            requires_approval: false,
            created_ms,
            status,
        }
    }

    fn memory(id: &str, superseded_by: Option<&str>, updated_ms: u64) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            layer: MemoryLayer::Semantic,
            scope: "global".to_string(),
            key: format!("key-{id}"),
            value: "v".to_string(),
            status: KnowledgeStatus::Observed,
            confidence: 0.5,
            source_experience: None,
            provenance: "test".to_string(),
            created_ms: updated_ms,
            updated_ms,
            expires_ms: None,
            superseded_by: superseded_by.map(str::to_string),
        }
    }

    #[test]
    fn an_existing_memory_table_gains_lifecycle_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evolution.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE memory_entries (
                    id TEXT PRIMARY KEY, layer TEXT NOT NULL, scope TEXT NOT NULL,
                    key TEXT NOT NULL, value TEXT NOT NULL, status TEXT NOT NULL,
                    confidence REAL NOT NULL, source_experience TEXT,
                    provenance TEXT NOT NULL, created_ms INTEGER NOT NULL,
                    updated_ms INTEGER NOT NULL, expires_ms INTEGER, superseded_by TEXT
                );
                INSERT INTO memory_entries VALUES
                    ('mem-old','semantic','global','k','v','observed',0.5,NULL,'t',1,1,NULL,NULL);
                "#,
            )
            .unwrap();
        }

        let store = EvolutionStore::open(&path).unwrap();
        assert_eq!(
            store.memory_lifecycle("mem-old").unwrap(),
            Some(MemoryLifecycle::Active),
            "rows written before the column existed default to active"
        );
        store
            .set_memory_lifecycle("mem-old", MemoryLifecycle::Archived)
            .unwrap();
        drop(store);

        let store = EvolutionStore::open(&path).unwrap();
        assert_eq!(
            store.memory_lifecycle("mem-old").unwrap(),
            Some(MemoryLifecycle::Archived),
            "reopening re-runs the migration without resetting state"
        );
        assert!(store.list_memory(None).unwrap().is_empty());
        assert_eq!(store.list_memory_all(None).unwrap().len(), 1);
    }

    #[test]
    fn prune_removes_only_old_terminal_rows() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let now = now_ms();
        let old = now - DEFAULT_RETENTION_MS - 1000;

        store.save_experience(&experience("exp-old", old)).unwrap();
        store.save_experience(&experience("exp-new", now)).unwrap();
        store.save_event(&event("evt-old", old)).unwrap();
        store.save_event(&event("evt-new", now)).unwrap();

        // Terminal + old → pruned; pending/running and recent → kept.
        store
            .save_job(&job("job-old-done", ReflectionJobStatus::Done, old))
            .unwrap();
        store
            .save_job(&job("job-old-failed", ReflectionJobStatus::Failed, old))
            .unwrap();
        store
            .save_job(&job("job-old-pending", ReflectionJobStatus::Pending, old))
            .unwrap();
        store
            .save_job(&job("job-new-done", ReflectionJobStatus::Done, now))
            .unwrap();

        store
            .save_proposal(&proposal(
                "prop-old-rejected",
                ProposalStatus::Rejected,
                old,
            ))
            .unwrap();
        store
            .save_proposal(&proposal("prop-old-pending", ProposalStatus::Pending, old))
            .unwrap();
        store
            .save_proposal(&proposal("prop-new-applied", ProposalStatus::Applied, now))
            .unwrap();

        store.save_memory(&memory("mem-live", None, old)).unwrap();
        store
            .save_memory(&memory("mem-dead", Some("mem-live"), old))
            .unwrap();
        store
            .save_memory(&memory("mem-dead-new", Some("mem-live"), now))
            .unwrap();

        let removed = store.prune_older_than(DEFAULT_RETENTION_MS).unwrap();
        assert_eq!(
            removed, 6,
            "exp + evt + 2 terminal jobs + 1 terminal proposal + 1 dead memory"
        );

        assert!(store.get_experience("exp-old").unwrap().is_none());
        assert!(store.get_experience("exp-new").unwrap().is_some());
        assert_eq!(store.count_events().unwrap(), 1);

        let pending = store.list_pending_jobs().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "job-old-pending");
        assert!(store.list_failed_jobs(10).unwrap().is_empty());

        let proposals = store.list_proposals(None).unwrap();
        assert_eq!(proposals.len(), 2);
        assert!(proposals.iter().all(|p| p.id != "prop-old-rejected"));

        let memories = store.list_memory_all(None).unwrap();
        assert_eq!(memories.len(), 2);
        assert!(memories.iter().all(|m| m.id != "mem-dead"));
        assert!(memories.iter().any(|m| m.id == "mem-dead-new"));
        // Live rows are never pruned even when old.
        assert!(memories.iter().any(|m| m.id == "mem-live"));
    }
}
