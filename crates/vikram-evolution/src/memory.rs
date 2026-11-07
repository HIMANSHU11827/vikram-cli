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

    /// Remember a fact. Dedups by (layer, scope, key) among *live* entries.
    ///
    /// Supersession is a new entry replacing an old one: when an existing
    /// live entry loses the conflict, a fresh entry (new id) is created
    /// and the old one is marked `superseded_by = <new id>`. The old
    /// entry is never edited into its own replacement — previously the
    /// losing entry was overwritten in place while its provenance claimed
    /// it had been superseded, leaving `superseded_by` unset (or, with a
    /// naive fix, pointing at itself). Superseded entries disappear from
    /// `recall`/`list` (filtered in SQL) but stay readable via
    /// `get_by_id` for audit.
    #[allow(clippy::too_many_arguments)]
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
        self.remember_with_expiry(
            layer, scope, key, value, status, confidence, source, provenance, None,
        )
    }

    /// Like [`Self::remember`], with an optional expiry timestamp (ms).
    #[allow(clippy::too_many_arguments)]
    pub fn remember_with_expiry(
        &self,
        layer: MemoryLayer,
        scope: &str,
        key: &str,
        value: &str,
        status: KnowledgeStatus,
        confidence: f32,
        source: Option<&str>,
        provenance: &str,
        expires_ms: Option<u64>,
    ) -> Result<MemoryEntry> {
        let value = sanitize_for_storage(value)?;
        let key = sanitize_for_storage(key)?;
        let now = now_ms();
        if let Some(existing) = self.store.find_memory(layer, scope, &key)? {
            // Conflict handling
            if confidence > existing.confidence + 0.05
                || status_rank(status) > status_rank(existing.status)
            {
                // New entry supersedes the old one. The id is generated
                // *before* saving so the old row can point at it.
                let new_id = gen_id("mem");
                let prov = format!("{provenance} | supersedes {}", existing.id);
                let entry = MemoryEntry {
                    id: new_id.clone(),
                    layer,
                    scope: scope.to_string(),
                    key,
                    value,
                    status,
                    confidence: confidence.max(existing.confidence),
                    source_experience: source
                        .map(|s| s.to_string())
                        .or_else(|| existing.source_experience.clone()),
                    provenance: prov,
                    created_ms: now,
                    updated_ms: now,
                    expires_ms,
                    superseded_by: None,
                };
                self.store.save_memory(&entry)?;
                // Now retire the old entry, pointing at the new one.
                // (Never at itself — `save_memory` rejects self-loops.)
                let mut old = existing.clone();
                old.superseded_by = Some(new_id);
                old.updated_ms = now;
                self.store.save_memory(&old)?;
                return Ok(entry);
            } else {
                // keep existing (superseded/expired rows are already
                // excluded by `find_memory`)
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
            expires_ms,
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
        let now = now_ms();
        // Store already filters superseded/expired in SQL; re-check here as
        // defence in depth so a future store change cannot leak dead rows.
        Ok(self
            .store
            .search_memory(scope, query, limit)?
            .into_iter()
            .filter(|m| m.is_live(now))
            .collect())
    }

    pub fn list(&self, scope: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let now = now_ms();
        Ok(self
            .store
            .list_memory(scope)?
            .into_iter()
            .filter(|m| m.is_live(now))
            .collect())
    }

    /// Read any entry by id, including superseded/expired ones (audit).
    pub fn get_by_id(&self, id: &str) -> Result<Option<MemoryEntry>> {
        self.store.get_memory(id)
    }

    /// All entries including superseded/expired ones (audit/debug).
    pub fn list_all(&self, scope: Option<&str>) -> Result<Vec<MemoryEntry>> {
        self.store.list_memory_all(scope)
    }

    pub fn forget(&self, id: &str) -> Result<bool> {
        self.store.delete_memory(id)
    }

    /// Consolidation: find duplicate live keys across scopes? Here: merge
    /// entries with same (layer,scope,key) keeping highest confidence; the
    /// rest are superseded by the keeper (never by themselves).
    ///
    /// Operates on *all* rows so pre-existing duplicates that were never
    /// superseded are cleaned up too; already-superseded rows are left
    /// alone rather than re-pointed.
    pub fn consolidate(&self) -> Result<usize> {
        let all = self.store.list_memory_all(None)?;
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
        let now = now_ms();
        for (_k, mut v) in groups {
            if v.len() <= 1 {
                continue;
            }
            // keep highest confidence live entry
            v.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let keep = v[0].id.clone();
            for m in &v[1..] {
                if m.is_superseded() {
                    continue;
                }
                // mark superseded (by the keeper, never by itself)
                if let Some(mut old) = self.store.get_memory(&m.id)? {
                    if old.id == keep {
                        continue;
                    }
                    old.superseded_by = Some(keep.clone());
                    old.updated_ms = now;
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
