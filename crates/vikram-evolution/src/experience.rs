use crate::error::Result;
use crate::redact::sanitize_for_storage;
use crate::store::EvolutionStore;
use crate::types::{Experience, gen_id, now_ms};

pub struct ExperienceCollector {
    store: EvolutionStore,
}

impl ExperienceCollector {
    pub fn new(store: EvolutionStore) -> Self {
        Self { store }
    }
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Record an experience with secret redaction + injection scanning.
    pub fn record(&self, mut exp: Experience) -> Result<Experience> {
        if exp.id.is_empty() {
            exp.id = gen_id("exp");
        }
        if exp.timestamp_ms == 0 {
            exp.timestamp_ms = now_ms();
        }
        // Redact
        exp.user_request = sanitize_for_storage(&exp.user_request)?;
        exp.errors = exp
            .errors
            .into_iter()
            .filter_map(|e| sanitize_for_storage(&e).ok())
            .collect();
        exp.corrections = exp
            .corrections
            .into_iter()
            .filter_map(|c| sanitize_for_storage(&c).ok())
            .collect();
        if let Some(f) = &exp.feedback {
            exp.feedback = sanitize_for_storage(f).ok();
        }
        // Sensitivity gating: secret -> drop value
        if exp.sensitivity == "secret" {
            exp.user_request = "[REDACTED SECRET EXPERIENCE]".to_string();
            exp.errors.clear();
            exp.corrections.clear();
        }
        self.store.save_experience(&exp)?;
        Ok(exp)
    }

    pub fn get(&self, id: &str) -> Result<Option<Experience>> {
        self.store.get_experience(id)
    }
    pub fn list(&self, agent_id: Option<&str>, limit: usize) -> Result<Vec<Experience>> {
        self.store.list_experiences(agent_id, limit)
    }
    pub fn count(&self) -> Result<usize> {
        self.store.count_experiences()
    }
}
