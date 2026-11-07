use crate::error::Result;
use crate::store::EvolutionStore;
use crate::types::{HiveLesson, gen_id, now_ms};

pub struct HiveLearning {
    store: EvolutionStore,
}

impl HiveLearning {
    pub fn new(store: EvolutionStore) -> Self {
        Self { store }
    }
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }
    pub fn record(
        &self,
        task_id: &str,
        agents: Vec<String>,
        outcome: &str,
        lesson: &str,
        pattern: &str,
    ) -> Result<HiveLesson> {
        // Sanitize *everything* that will be persisted — including the
        // lesson text itself. Previously `lesson` was stored raw while
        // every other field went through the injection scan + redaction.
        let h = HiveLesson {
            id: gen_id("hive"),
            task_id: crate::redact::sanitize_for_storage(task_id)?,
            agents: agents
                .into_iter()
                .map(|a| crate::redact::sanitize_for_storage(&a))
                .collect::<Result<Vec<_>>>()?,
            outcome: crate::redact::sanitize_for_storage(outcome)?,
            lesson: crate::redact::sanitize_for_storage(lesson)?,
            pattern: crate::redact::sanitize_for_storage(pattern)?,
            created_ms: now_ms(),
        };
        self.store.save_hive_lesson(&h)?;
        Ok(h)
    }
    pub fn list(&self) -> Result<Vec<HiveLesson>> {
        self.store.list_hive_lessons()
    }
}
