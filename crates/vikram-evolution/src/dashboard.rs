use crate::error::Result;
use crate::store::EvolutionStore;
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct LearningDashboard {
    pub experiences: usize,
    pub memories: usize,
    pub proposals_pending: usize,
    pub proposals_total: usize,
    pub candidates: usize,
    pub hive_lessons: usize,
    pub learning_events: usize,
    pub reflection_jobs_pending: usize,
}

pub struct Dashboard {
    store: EvolutionStore,
}

impl Dashboard {
    pub fn new(store: EvolutionStore) -> Self {
        Self { store }
    }
    pub fn snapshot(&self) -> Result<LearningDashboard> {
        let proposals = self.store.list_proposals(None)?;
        let pending = proposals.iter().filter(|p| p.status == "pending").count();
        let jobs = self.store.list_pending_jobs()?;
        Ok(LearningDashboard {
            experiences: self.store.count_experiences()?,
            memories: self.store.count_memory()?,
            proposals_pending: pending,
            proposals_total: proposals.len(),
            candidates: self.store.list_candidates(None)?.len(),
            hive_lessons: self.store.list_hive_lessons()?.len(),
            learning_events: self.store.list_events(None, 10000)?.len(),
            reflection_jobs_pending: jobs.len(),
        })
    }
}
