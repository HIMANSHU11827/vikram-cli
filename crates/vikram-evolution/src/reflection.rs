use crate::error::Result;
use crate::experience::ExperienceCollector;
use crate::memory::LayeredMemory;
use crate::store::EvolutionStore;
use crate::types::{Experience, LearningProposal, ReflectionJob, gen_id, now_ms};
use std::sync::Arc;

pub trait Reflector: Send + Sync {
    fn reflect(&self, exp: &Experience) -> Result<Vec<LearningProposal>>;
}

/// Heuristic reflector: produces structured proposals without an LLM.
pub struct HeuristicReflector;

impl Reflector for HeuristicReflector {
    fn reflect(&self, exp: &Experience) -> Result<Vec<LearningProposal>> {
        let mut out = Vec::new();
        let now = now_ms();
        // No-op for trivial success
        if exp.outcome == "success"
            && exp.errors.is_empty()
            && exp.corrections.is_empty()
            && !exp.reusable
        {
            // still maybe record if feedback
            if exp.feedback.is_none() {
                return Ok(out);
            }
        }
        // User corrections -> user memory proposal
        if !exp.corrections.is_empty() {
            out.push(LearningProposal {
                id: gen_id("prop"),
                experience_id: Some(exp.id.clone()),
                agent_id: exp.agent_id.clone(),
                kind: "add_memory".to_string(),
                summary: "Record user correction as user memory".to_string(),
                details: serde_json::json!({"layer":"user","corrections": exp.corrections}),
                confidence: 0.8,
                requires_approval: false,
                created_ms: now,
                status: "pending".to_string(),
            });
        }
        // Failures -> record issue
        if exp.outcome == "failure" || !exp.errors.is_empty() {
            out.push(LearningProposal {
                id: gen_id("prop"),
                experience_id: Some(exp.id.clone()),
                agent_id: exp.agent_id.clone(),
                kind: "record_issue".to_string(),
                summary: "Record failure and error pattern".to_string(),
                details: serde_json::json!({"errors": exp.errors, "outcome": exp.outcome}),
                confidence: 0.7,
                requires_approval: false,
                created_ms: now,
                status: "pending".to_string(),
            });
        }
        // Reusable success -> skill candidate
        if exp.outcome == "success" && exp.reusable && exp.errors.is_empty() {
            out.push(LearningProposal {
                id: gen_id("prop"),
                experience_id: Some(exp.id.clone()),
                agent_id: exp.agent_id.clone(),
                kind: "create_skill".to_string(),
                summary: "Create reusable skill from successful workflow".to_string(),
                details: serde_json::json!({"tools": exp.tools_used, "category": exp.task_category}),
                confidence: 0.6,
                requires_approval: true,
                created_ms: now,
                status: "pending".to_string(),
            });
        }
        // If nothing produced, no-op (valid)
        Ok(out)
    }
}

pub struct ReflectionEngine {
    store: EvolutionStore,
    collector: ExperienceCollector,
    memory: LayeredMemory,
    reflector: Arc<dyn Reflector>,
}

impl ReflectionEngine {
    pub fn new(store: EvolutionStore, reflector: Arc<dyn Reflector>) -> Self {
        let collector = ExperienceCollector::new(store.clone());
        let memory = LayeredMemory::new(store.clone());
        Self {
            store,
            collector,
            memory,
            reflector,
        }
    }
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }
    pub fn collector(&self) -> &ExperienceCollector {
        &self.collector
    }
    pub fn memory(&self) -> &LayeredMemory {
        &self.memory
    }

    /// Enqueue a reflection job (does not run).
    pub fn enqueue(&self, experience_id: &str, agent_id: &str) -> Result<ReflectionJob> {
        let job = ReflectionJob {
            id: gen_id("job"),
            experience_id: experience_id.to_string(),
            agent_id: agent_id.to_string(),
            status: "pending".to_string(),
            attempts: 0,
            created_ms: now_ms(),
            updated_ms: now_ms(),
            error: None,
        };
        self.store.save_job(&job)?;
        Ok(job)
    }

    /// Run reflection for one experience, producing proposals and applying low-risk ones.
    pub fn run_reflection(&self, experience_id: &str) -> Result<Vec<LearningProposal>> {
        let exp = self
            .collector
            .get(experience_id)?
            .ok_or_else(|| crate::error::EvolutionError::NotFound(experience_id.to_string()))?;
        let proposals = self.reflector.reflect(&exp)?;
        for p in &proposals {
            self.store.save_proposal(p)?;
        }
        // Apply low-risk memory additions automatically
        for p in &proposals {
            if p.kind == "add_memory" && !p.requires_approval {
                if let Some(corr) = p.details.get("corrections") {
                    if let Some(arr) = corr.as_array() {
                        for c in arr {
                            if let Some(s) = c.as_str() {
                                let _ = self.memory.remember(
                                    crate::types::MemoryLayer::User,
                                    &format!("agent:{}", p.agent_id),
                                    &format!("correction-{}", gen_id("k")),
                                    s,
                                    crate::types::KnowledgeStatus::UserStated,
                                    p.confidence,
                                    Some(&exp.id),
                                    "reflection",
                                );
                            }
                        }
                    }
                }
                // mark applied
                let mut pp = p.clone();
                pp.status = "applied".to_string();
                self.store.save_proposal(&pp)?;
            }
        }
        // record event
        let ev = crate::types::LearningEvent {
            id: gen_id("evt"),
            timestamp_ms: now_ms(),
            agent_id: exp.agent_id.clone(),
            kind: "reflection".to_string(),
            summary: format!(
                "Reflected on experience {} -> {} proposals",
                exp.id,
                proposals.len()
            ),
            payload: serde_json::json!({"experience_id": exp.id, "proposal_ids": proposals.iter().map(|p|p.id.clone()).collect::<Vec<_>>() }),
        };
        self.store.save_event(&ev)?;
        Ok(proposals)
    }

    /// Recover interrupted jobs: mark stale running as pending.
    pub fn recover_jobs(&self) -> Result<usize> {
        let jobs = self.store.list_pending_jobs()?;
        let mut n = 0usize;
        for mut j in jobs {
            if j.status == "running" {
                j.status = "pending".to_string();
                j.updated_ms = now_ms();
                self.store.save_job(&j)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Process pending jobs (simple synchronous drain for tests/background).
    pub fn process_pending(&self) -> Result<usize> {
        let jobs = self.store.list_pending_jobs()?;
        let mut done = 0usize;
        for mut j in jobs {
            j.status = "running".to_string();
            j.attempts += 1;
            j.updated_ms = now_ms();
            self.store.save_job(&j)?;
            match self.run_reflection(&j.experience_id) {
                Ok(_) => {
                    j.status = "done".to_string();
                    j.updated_ms = now_ms();
                    self.store.save_job(&j)?;
                    done += 1;
                }
                Err(e) => {
                    j.status = "failed".to_string();
                    j.error = Some(e.to_string());
                    j.updated_ms = now_ms();
                    self.store.save_job(&j)?;
                }
            }
        }
        Ok(done)
    }
}
