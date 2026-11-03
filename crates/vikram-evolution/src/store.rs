use crate::error::{EvolutionError, Result};
use crate::types::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone)]
pub struct EvolutionStore {
    conn: Arc<Mutex<Connection>>,
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

    // ---- experiences ----
    pub fn save_experience(&self, e: &Experience) -> Result<()> {
        let tools = serde_json::to_string(&e.tools_used).unwrap_or_default();
        let errs = serde_json::to_string(&e.errors).unwrap_or_default();
        let corr = serde_json::to_string(&e.corrections).unwrap_or_default();
        let meta = e.metadata.to_string();
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO experiences (id, timestamp_ms, agent_id, project_id, task_category, user_request, outcome, tools_used, errors, corrections, feedback, reusable, sensitivity, metadata)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![e.id, e.timestamp_ms as i64, e.agent_id, e.project_id, e.task_category, e.user_request, e.outcome, tools, errs, corr, e.feedback, e.reusable as i64, e.sensitivity, meta],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn get_experience(&self, id: &str) -> Result<Option<Experience>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,timestamp_ms,agent_id,project_id,task_category,user_request,outcome,tools_used,errors,corrections,feedback,reusable,sensitivity,metadata FROM experiences WHERE id=?1")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let row = stmt
            .query_row(params![id], |r| {
                Ok(Experience {
                    id: r.get(0)?,
                    timestamp_ms: r.get::<_, i64>(1)? as u64,
                    agent_id: r.get(2)?,
                    project_id: r.get(3)?,
                    task_category: r.get(4)?,
                    user_request: r.get(5)?,
                    outcome: r.get(6)?,
                    tools_used: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                    errors: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
                    corrections: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default(),
                    feedback: r.get(10)?,
                    reusable: r.get::<_, i64>(11)? != 0,
                    sensitivity: r.get(12)?,
                    metadata: serde_json::from_str(&r.get::<_, String>(13)?)
                        .unwrap_or(serde_json::json!({})),
                })
            })
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
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| EvolutionError::Store(e.to_string()))?);
        }
        Ok(out)
    }
    pub fn count_experiences(&self) -> Result<usize> {
        let conn = self.lock();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM experiences", [], |r| r.get(0))
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(n as usize)
    }

    // ---- memory entries ----
    pub fn save_memory(&self, m: &MemoryEntry) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO memory_entries (id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![m.id, m.layer.as_str(), m.scope, m.key, m.value, m.status.as_str(), m.confidence, m.source_experience, m.provenance, m.created_ms as i64, m.updated_ms as i64, m.expires_ms.map(|x|x as i64), m.superseded_by],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
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
    pub fn find_memory(
        &self,
        layer: MemoryLayer,
        scope: &str,
        key: &str,
    ) -> Result<Option<MemoryEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries WHERE layer=?1 AND scope=?2 AND key=?3")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let row = stmt
            .query_row(params![layer.as_str(), scope, key], map_mem)
            .optional()
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(row)
    }
    pub fn search_memory(
        &self,
        scope: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let conn = self.lock();
        let sql = if scope.is_some() {
            "SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries WHERE scope=?1 ORDER BY updated_ms DESC LIMIT ?2"
        } else {
            "SELECT id,layer,scope,key,value,status,confidence,source_experience,provenance,created_ms,updated_ms,expires_ms,superseded_by FROM memory_entries ORDER BY updated_ms DESC LIMIT ?1"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = if let Some(s) = scope {
            stmt.query_map(params![s, limit as i64], map_mem)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        } else {
            stmt.query_map(params![limit as i64], map_mem)
                .map_err(|e| EvolutionError::Store(e.to_string()))?
        };
        let mut out = Vec::new();
        let ql = query.to_lowercase();
        for r in rows {
            let m = r.map_err(|e| EvolutionError::Store(e.to_string()))?;
            if m.key.to_lowercase().contains(&ql) || m.value.to_lowercase().contains(&ql) {
                out.push(m);
            }
        }
        Ok(out)
    }
    pub fn list_memory(&self, scope: Option<&str>) -> Result<Vec<MemoryEntry>> {
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
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| EvolutionError::Store(e.to_string()))?);
        }
        Ok(out)
    }
    pub fn delete_memory(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn
            .execute("DELETE FROM memory_entries WHERE id=?1", params![id])
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(n > 0)
    }
    pub fn count_memory(&self) -> Result<usize> {
        let conn = self.lock();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(n as usize)
    }

    // ---- proposals ----
    pub fn save_proposal(&self, p: &LearningProposal) -> Result<()> {
        let details = p.details.to_string();
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO proposals (id,experience_id,agent_id,kind,summary,details,confidence,requires_approval,created_ms,status)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![p.id, p.experience_id, p.agent_id, p.kind, p.summary, details, p.confidence, p.requires_approval as i64, p.created_ms as i64, p.status],
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
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| EvolutionError::Store(e.to_string()))?);
        }
        Ok(out)
    }

    // ---- reflection jobs ----
    pub fn save_job(&self, j: &ReflectionJob) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO reflection_jobs (id,experience_id,agent_id,status,attempts,created_ms,updated_ms,error)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![j.id, j.experience_id, j.agent_id, j.status, j.attempts, j.created_ms as i64, j.updated_ms as i64, j.error],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn list_pending_jobs(&self) -> Result<Vec<ReflectionJob>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id,experience_id,agent_id,status,attempts,created_ms,updated_ms,error FROM reflection_jobs WHERE status IN ('pending','running','failed') ORDER BY created_ms ASC")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map([], map_job)
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| EvolutionError::Store(e.to_string()))?);
        }
        Ok(out)
    }

    // ---- hive lessons ----
    pub fn save_hive_lesson(&self, h: &HiveLesson) -> Result<()> {
        let agents = serde_json::to_string(&h.agents).unwrap_or_default();
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
            .query_map([], |r| {
                Ok(HiveLesson {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    agents: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
                    outcome: r.get(3)?,
                    lesson: r.get(4)?,
                    pattern: r.get(5)?,
                    created_ms: r.get::<_, i64>(6)? as u64,
                })
            })
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| EvolutionError::Store(e.to_string()))?);
        }
        Ok(out)
    }

    // ---- candidates & evaluations ----
    pub fn save_candidate(&self, c: &OptimizationCandidate) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO candidates (id,target,parent_version,change_description,content,created_ms,status) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![c.id, c.target, c.parent_version, c.change_description, c.content, c.created_ms as i64, c.status],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn list_candidates(&self, target: Option<&str>) -> Result<Vec<OptimizationCandidate>> {
        let conn = self.lock();
        let sql = if target.is_some() {
            "SELECT id,target,parent_version,change_description,content,created_ms,status FROM candidates WHERE target=?1 ORDER BY created_ms DESC"
        } else {
            "SELECT id,target,parent_version,change_description,content,created_ms,status FROM candidates ORDER BY created_ms DESC"
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
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| EvolutionError::Store(e.to_string()))?);
        }
        Ok(out)
    }
    pub fn save_evaluation(&self, e: &EvaluationResult) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO evaluations (candidate_id,dataset,metrics,score,passed,notes,created_ms) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![e.candidate_id, e.dataset, e.metrics.to_string(), e.score, e.passed as i64, e.notes, e.created_ms as i64],
        ).map_err(|e| EvolutionError::Store(e.to_string()))?;
        Ok(())
    }
    pub fn list_evaluations(&self, candidate_id: &str) -> Result<Vec<EvaluationResult>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT candidate_id,dataset,metrics,score,passed,notes,created_ms FROM evaluations WHERE candidate_id=?1")
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let rows = stmt
            .query_map(params![candidate_id], |r| {
                Ok(EvaluationResult {
                    candidate_id: r.get(0)?,
                    dataset: r.get(1)?,
                    metrics: serde_json::from_str(&r.get::<_, String>(2)?)
                        .unwrap_or(serde_json::json!({})),
                    score: r.get(3)?,
                    passed: r.get::<_, i64>(4)? != 0,
                    notes: r.get(5)?,
                    created_ms: r.get::<_, i64>(6)? as u64,
                })
            })
            .map_err(|e| EvolutionError::Store(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| EvolutionError::Store(e.to_string()))?);
        }
        Ok(out)
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
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| EvolutionError::Store(e.to_string()))?);
        }
        Ok(out)
    }

    // ---- learning events ----
    pub fn save_event(&self, e: &LearningEvent) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO learning_events (id,timestamp_ms,agent_id,kind,summary,payload) VALUES (?1,?2,?3,?4,?5,?6)",
            params![e.id, e.timestamp_ms as i64, e.agent_id, e.kind, e.summary, e.payload.to_string()],
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
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| EvolutionError::Store(e.to_string()))?);
        }
        Ok(out)
    }
}

fn map_exp(r: &rusqlite::Row) -> rusqlite::Result<Experience> {
    Ok(Experience {
        id: r.get(0)?,
        timestamp_ms: r.get::<_, i64>(1)? as u64,
        agent_id: r.get(2)?,
        project_id: r.get(3)?,
        task_category: r.get(4)?,
        user_request: r.get(5)?,
        outcome: r.get(6)?,
        tools_used: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
        errors: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
        corrections: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default(),
        feedback: r.get(10)?,
        reusable: r.get::<_, i64>(11)? != 0,
        sensitivity: r.get(12)?,
        metadata: serde_json::from_str(&r.get::<_, String>(13)?).unwrap_or(serde_json::json!({})),
    })
}

fn map_mem(r: &rusqlite::Row) -> rusqlite::Result<MemoryEntry> {
    Ok(MemoryEntry {
        id: r.get(0)?,
        layer: MemoryLayer::parse(&r.get::<_, String>(1)?).unwrap_or(MemoryLayer::Semantic),
        scope: r.get(2)?,
        key: r.get(3)?,
        value: r.get(4)?,
        status: KnowledgeStatus::parse(&r.get::<_, String>(5)?)
            .unwrap_or(KnowledgeStatus::Observed),
        confidence: r.get(6)?,
        source_experience: r.get(7)?,
        provenance: r.get(8)?,
        created_ms: r.get::<_, i64>(9)? as u64,
        updated_ms: r.get::<_, i64>(10)? as u64,
        expires_ms: r.get::<_, Option<i64>>(11)?.map(|x| x as u64),
        superseded_by: r.get(12)?,
    })
}

fn map_prop(r: &rusqlite::Row) -> rusqlite::Result<LearningProposal> {
    Ok(LearningProposal {
        id: r.get(0)?,
        experience_id: r.get(1)?,
        agent_id: r.get(2)?,
        kind: r.get(3)?,
        summary: r.get(4)?,
        details: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(serde_json::json!({})),
        confidence: r.get(6)?,
        requires_approval: r.get::<_, i64>(7)? != 0,
        created_ms: r.get::<_, i64>(8)? as u64,
        status: r.get(9)?,
    })
}

fn map_job(r: &rusqlite::Row) -> rusqlite::Result<ReflectionJob> {
    Ok(ReflectionJob {
        id: r.get(0)?,
        experience_id: r.get(1)?,
        agent_id: r.get(2)?,
        status: r.get(3)?,
        attempts: r.get::<_, i64>(4)? as u32,
        created_ms: r.get::<_, i64>(5)? as u64,
        updated_ms: r.get::<_, i64>(6)? as u64,
        error: r.get(7)?,
    })
}

fn map_cand(r: &rusqlite::Row) -> rusqlite::Result<OptimizationCandidate> {
    Ok(OptimizationCandidate {
        id: r.get(0)?,
        target: r.get(1)?,
        parent_version: r.get(2)?,
        change_description: r.get(3)?,
        content: r.get(4)?,
        created_ms: r.get::<_, i64>(5)? as u64,
        status: r.get(6)?,
    })
}

fn map_event(r: &rusqlite::Row) -> rusqlite::Result<LearningEvent> {
    Ok(LearningEvent {
        id: r.get(0)?,
        timestamp_ms: r.get::<_, i64>(1)? as u64,
        agent_id: r.get(2)?,
        kind: r.get(3)?,
        summary: r.get(4)?,
        payload: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(serde_json::json!({})),
    })
}

fn migrate(conn: &Connection) -> Result<()> {
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
            superseded_by TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_mem_scope ON memory_entries(scope);
        CREATE INDEX IF NOT EXISTS idx_mem_layer ON memory_entries(layer);
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
            created_ms INTEGER NOT NULL
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
    Ok(())
}
