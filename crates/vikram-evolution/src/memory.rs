use crate::error::Result;
use crate::redact::sanitize_for_storage;
use crate::store::EvolutionStore;
use crate::types::{KnowledgeStatus, MemoryEntry, MemoryLayer, gen_id, now_ms};

pub struct LayeredMemory {
    store: EvolutionStore,
}

impl LayeredMemory {
    pub fn new(store: EvolutionStore) -> Self {
        Self { store }
    }
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Remember a fact. Dedups by (layer, scope, key). On conflict, updates if new confidence higher or status more authoritative.
    pub fn remember(
        &self,
        layer: MemoryLayer,
        scope: &str,
        key: &str,
        value: &str,
        status: KnowledgeStatus,
        confidence: f32,
        source: Option<&str>,
        provenance: &str,
    ) -> Result<MemoryEntry> {
        let value = sanitize_for_storage(value)?;
        let key = sanitize_for_storage(key)?;
        let now = now_ms();
        if let Some(existing) = self.store.find_memory(layer, scope, &key)? {
            // Conflict handling
            if confidence > existing.confidence + 0.05
                || status_rank(status) > status_rank(existing.status)
            {
                let mut updated = existing.clone();
                updated.value = value;
                updated.status = status;
                updated.confidence = confidence.max(existing.confidence);
                updated.updated_ms = now;
                updated.provenance = format!("{} | superseded {}", provenance, existing.provenance);
                updated.source_experience =
                    source.map(|s| s.to_string()).or(existing.source_experience);
                self.store.save_memory(&updated)?;
                return Ok(updated);
            } else {
                // keep existing, maybe bump confidence slightly
                return Ok(existing);
            }
        }
        let entry = MemoryEntry {
            id: gen_id("mem"),
            layer,
            scope: scope.to_string(),
            key,
            value,
            status,
            confidence,
            source_experience: source.map(|s| s.to_string()),
            provenance: provenance.to_string(),
            created_ms: now,
            updated_ms: now,
            expires_ms: None,
            superseded_by: None,
        };
        self.store.save_memory(&entry)?;
        Ok(entry)
    }

    pub fn recall(
        &self,
        scope: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        self.store.search_memory(scope, query, limit)
    }

    pub fn list(&self, scope: Option<&str>) -> Result<Vec<MemoryEntry>> {
        self.store.list_memory(scope)
    }

    pub fn forget(&self, id: &str) -> Result<bool> {
        self.store.delete_memory(id)
    }

    /// Consolidation: find duplicate keys across scopes? Here: merge entries with same (layer,scope,key) keeping highest confidence; remove superseded.
    pub fn consolidate(&self) -> Result<usize> {
        let all = self.store.list_memory(None)?;
        // group by (layer,scope,key)
        use std::collections::HashMap;
        let mut groups: HashMap<(String, String, String), Vec<MemoryEntry>> = HashMap::new();
        for m in all {
            groups
                .entry((m.layer.as_str().to_string(), m.scope.clone(), m.key.clone()))
                .or_default()
                .push(m);
        }
        let mut removed = 0usize;
        for (_k, mut v) in groups {
            if v.len() <= 1 {
                continue;
            }
            // keep highest confidence
            v.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let keep = v[0].id.clone();
            for m in &v[1..] {
                // mark superseded
                if let Some(mut old) = self.store.get_memory(&m.id)? {
                    old.superseded_by = Some(keep.clone());
                    self.store.save_memory(&old)?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    pub fn count(&self) -> Result<usize> {
        self.store.count_memory()
    }
}

fn status_rank(s: KnowledgeStatus) -> u8 {
    match s {
        KnowledgeStatus::Confirmed => 6,
        KnowledgeStatus::UserStated => 5,
        KnowledgeStatus::Observed => 4,
        KnowledgeStatus::Inferred => 3,
        KnowledgeStatus::Hypothesis => 2,
        KnowledgeStatus::Temporary => 1,
    }
}
