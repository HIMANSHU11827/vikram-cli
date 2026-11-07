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
    ///
    /// Sanitizing every field is fail-closed: if any field fails the
    /// injection scan, the whole record is rejected rather than silently
    /// dropping the offending item (previously `filter_map(|e| ..ok())`
    /// dropped errors/corrections and cleared feedback without a trace).
    pub fn record(&self, mut exp: Experience) -> Result<Experience> {
        if exp.id.is_empty() {
            exp.id = gen_id("exp");
        }
        if exp.timestamp_ms == 0 {
            exp.timestamp_ms = now_ms();
        }
        // Redact (fail-closed: an injection hit anywhere rejects the record)
        exp.user_request = sanitize_for_storage(&exp.user_request)?;
        for e in exp.errors.iter_mut() {
            *e = sanitize_for_storage(e)?;
        }
        for c in exp.corrections.iter_mut() {
            *c = sanitize_for_storage(c)?;
        }
        if let Some(f) = &exp.feedback {
            exp.feedback = Some(sanitize_for_storage(f)?);
        }
        // Sensitivity gating: secret -> drop value
        if exp.sensitivity == crate::types::Sensitivity::Secret {
            exp.user_request = "[REDACTED SECRET EXPERIENCE]".to_string();
            exp.errors.clear();
            exp.corrections.clear();
            exp.feedback = None;
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
