//! Long-term memory curation: age unused facts out of live memory without
//! ever deleting a row.
//!
//! Lifecycle: `active` -> `stale` (14 days unused) -> `archived` (30 days
//! unused), and back to `active` when the entry is used again. `archived`
//! rows stay readable through `get_memory`/`list_memory_all` but every live
//! listing, search, and count excludes them, so they are never offered to a
//! prompt as current.
//!
//! "Unused" is measured from `memory_entries.updated_ms` — the newest
//! timestamp the row carries. The schema has no dedicated last-used column,
//! and `updated_ms` is exactly what a write bumps, so no column was added for
//! it. The lifecycle *state* does need one (`lifecycle`, schema v4): it
//! cannot live in `status`, which is the typed knowledge vocabulary, and it
//! must be filterable in the same SQL query as supersession and expiry.
//!
//! Every transition is one row in the `engine_records` namespace
//! [`LEDGER_NAMESPACE`], the same durability mechanism
//! [`crate::self_recovery`] uses for its operations. A ledger row records
//! the value hash before and after (the value never changes on a transition,
//! so the hashes match; they exist to prove the row was not rewritten) and
//! can be rolled back by id exactly once.

use crate::error::{EvolutionError, Result};
use crate::store::{
    EvolutionStore, find_engine_record_conn, find_memory_lifecycle_conn, from_json_value,
    list_memory_lifecycle_conn, mark_memory_used_conn, save_engine_record_conn,
    set_memory_lifecycle_conn, to_json_value,
};
use crate::types::{MemoryLifecycle, gen_id, now_ms};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// `engine_records` namespace holding one row per lifecycle transition.
const LEDGER_NAMESPACE: &str = "curator.ledger";

/// An `active` entry unused for this long becomes `stale`.
pub const STALE_AFTER_MS: u64 = 14 * 24 * 60 * 60 * 1000;
/// A `stale` entry unused for this long becomes `archived`.
pub const ARCHIVE_AFTER_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Counts from one [`Curator::sweep`], plus the ledger ids it wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CuratorSweep {
    pub staled: usize,
    pub archived: usize,
    pub reactivated: usize,
    pub ledger_ids: Vec<String>,
}

/// One persisted transition, newest first from [`Curator::ledger`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CuratorLedgerEntry {
    pub id: String,
    pub memory_id: String,
    pub memory_key: String,
    pub memory_scope: String,
    pub from_state: MemoryLifecycle,
    pub to_state: MemoryLifecycle,
    pub before_hash: String,
    pub after_hash: String,
    pub at_ms: u64,
    pub reverted_by: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LedgerRecord {
    #[serde(flatten)]
    entry: CuratorLedgerEntry,
    before_updated_ms: u64,
    after_updated_ms: u64,
}

pub struct Curator {
    store: EvolutionStore,
}

impl Curator {
    pub fn new(store: EvolutionStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Apply the lifecycle rules as of `now_ms`. When `scope_filter` is set
    /// only rows in that scope are considered. Superseded and expired rows
    /// are skipped: they are already dead and moving them would only add
    /// ledger noise.
    ///
    /// Running the sweep twice for the same `now_ms` transitions nothing the
    /// second time — the target state is a function of the age and the
    /// current state, and the first run already reached it.
    pub fn sweep(&self, now_ms: u64, scope_filter: Option<&str>) -> Result<CuratorSweep> {
        self.store.transact(|conn| {
            let mut sweep = CuratorSweep::default();
            for row in list_memory_lifecycle_conn(conn, scope_filter)? {
                if row.superseded || row.expires_ms.is_some_and(|exp| exp <= now_ms) {
                    continue;
                }
                let target = lifecycle_for(now_ms, row.updated_ms);
                if target == row.lifecycle {
                    continue;
                }
                if !set_memory_lifecycle_conn(conn, &row.id, target)? {
                    continue;
                }
                let ledger_id = write_transition(conn, &row, target, now_ms, row.updated_ms)?;
                match target {
                    MemoryLifecycle::Active => sweep.reactivated += 1,
                    MemoryLifecycle::Stale => sweep.staled += 1,
                    MemoryLifecycle::Archived => sweep.archived += 1,
                }
                sweep.ledger_ids.push(ledger_id);
            }
            Ok(sweep)
        })
    }

    /// Undo exactly one transition: restore the previous lifecycle state and,
    /// if the transition moved `updated_ms`, that timestamp too. Rolling back
    /// an already-reverted entry is a no-op returning `Ok(())`.
    ///
    /// An entry whose state moved on since the ledger row was written is a
    /// [`EvolutionError::Conflict`], not a silent overwrite.
    pub fn rollback(&self, ledger_id: &str) -> Result<()> {
        self.store.transact(|conn| {
            let payload =
                find_engine_record_conn(conn, LEDGER_NAMESPACE, ledger_id)?.ok_or_else(|| {
                    EvolutionError::NotFound(format!("curator ledger entry {ledger_id}"))
                })?;
            let mut record: LedgerRecord = from_json_value(payload)?;
            if record.entry.reverted_by.is_some() {
                return Ok(());
            }
            let current =
                find_memory_lifecycle_conn(conn, &record.entry.memory_id)?.ok_or_else(|| {
                    EvolutionError::NotFound(format!("memory entry {}", record.entry.memory_id))
                })?;
            if current.lifecycle != record.entry.to_state {
                return Err(EvolutionError::Conflict(format!(
                    "curator ledger entry {ledger_id} recorded {} -> {} but the entry is now {}",
                    record.entry.from_state.as_str(),
                    record.entry.to_state.as_str(),
                    current.lifecycle.as_str()
                )));
            }
            set_memory_lifecycle_conn(conn, &current.id, record.entry.from_state)?;
            if record.before_updated_ms != record.after_updated_ms {
                mark_memory_used_conn(conn, &current.id, record.before_updated_ms)?;
            }
            record.entry.reverted_by = Some(gen_id("curator-rollback"));
            save_engine_record_conn(conn, LEDGER_NAMESPACE, ledger_id, &to_json_value(&record)?)?;
            Ok(())
        })
    }

    /// Persisted transitions, newest first.
    pub fn ledger(&self, limit: usize) -> Result<Vec<CuratorLedgerEntry>> {
        let mut records: Vec<LedgerRecord> = self
            .store
            .list_engine_records(LEDGER_NAMESPACE)?
            .into_iter()
            .map(|(_, payload)| from_json_value(payload))
            .collect::<Result<_>>()?;
        records.sort_by(|a, b| {
            b.entry
                .at_ms
                .cmp(&a.entry.at_ms)
                .then_with(|| b.entry.id.cmp(&a.entry.id))
        });
        Ok(records.into_iter().take(limit).map(|r| r.entry).collect())
    }

    /// Record a use of `memory_id` and recover it to `active`. Returns
    /// `Ok(false)` when the row does not exist or is already superseded.
    pub fn touch(&self, memory_id: &str) -> Result<bool> {
        self.store.transact(|conn| {
            let Some(row) = find_memory_lifecycle_conn(conn, memory_id)? else {
                return Ok(false);
            };
            if row.superseded {
                return Ok(false);
            }
            let at_ms = now_ms();
            mark_memory_used_conn(conn, memory_id, at_ms)?;
            if row.lifecycle != MemoryLifecycle::Active {
                set_memory_lifecycle_conn(conn, memory_id, MemoryLifecycle::Active)?;
                write_transition(conn, &row, MemoryLifecycle::Active, at_ms, at_ms)?;
            }
            Ok(true)
        })
    }
}

fn lifecycle_for(now_ms: u64, used_ms: u64) -> MemoryLifecycle {
    let idle = now_ms.saturating_sub(used_ms);
    if idle >= ARCHIVE_AFTER_MS {
        MemoryLifecycle::Archived
    } else if idle >= STALE_AFTER_MS {
        MemoryLifecycle::Stale
    } else {
        MemoryLifecycle::Active
    }
}

fn write_transition(
    conn: &rusqlite::Connection,
    row: &crate::store::MemoryLifecycleRow,
    to_state: MemoryLifecycle,
    at_ms: u64,
    after_updated_ms: u64,
) -> Result<String> {
    let hash = hash_value(&row.value);
    let record = LedgerRecord {
        entry: CuratorLedgerEntry {
            id: gen_id("curator"),
            memory_id: row.id.clone(),
            memory_key: row.key.clone(),
            memory_scope: row.scope.clone(),
            from_state: row.lifecycle,
            to_state,
            before_hash: hash.clone(),
            after_hash: hash,
            at_ms,
            reverted_by: None,
        },
        before_updated_ms: row.updated_ms,
        after_updated_ms,
    };
    save_engine_record_conn(
        conn,
        LEDGER_NAMESPACE,
        &record.entry.id,
        &to_json_value(&record)?,
    )?;
    Ok(record.entry.id)
}

fn hash_value(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EvolutionEngine;
    use crate::memory::LayeredMemory;
    use crate::types::{KnowledgeStatus, MemoryEntry, MemoryLayer};

    const NOW: u64 = 1_700_000_000_000;
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    fn seed(
        store: &EvolutionStore,
        id: &str,
        scope: &str,
        key: &str,
        value: &str,
        updated_ms: u64,
    ) {
        store
            .save_memory(&MemoryEntry {
                id: id.to_string(),
                layer: MemoryLayer::Semantic,
                scope: scope.to_string(),
                key: key.to_string(),
                value: value.to_string(),
                status: KnowledgeStatus::Observed,
                confidence: 0.8,
                source_experience: None,
                provenance: "curator-test".to_string(),
                created_ms: updated_ms,
                updated_ms,
                expires_ms: None,
                superseded_by: None,
            })
            .unwrap();
    }

    #[test]
    fn an_entry_unused_for_fifteen_days_becomes_stale() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let curator = Curator::new(store.clone());
        seed(
            &store,
            "mem-a",
            "project:p1",
            "db",
            "postgres",
            NOW - 15 * DAY_MS,
        );

        let sweep = curator.sweep(NOW, None).unwrap();

        assert_eq!(sweep.staled, 1);
        assert_eq!(sweep.archived, 0);
        assert_eq!(sweep.reactivated, 0);
        assert_eq!(sweep.ledger_ids.len(), 1);
        assert_eq!(
            store.memory_lifecycle("mem-a").unwrap(),
            Some(MemoryLifecycle::Stale)
        );
    }

    #[test]
    fn a_stale_entry_unused_for_thirty_one_days_is_archived_not_deleted() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let curator = Curator::new(store.clone());
        seed(
            &store,
            "mem-a",
            "project:p1",
            "db",
            "postgres",
            NOW - 15 * DAY_MS,
        );

        assert_eq!(curator.sweep(NOW, None).unwrap().staled, 1);
        let sweep = curator.sweep(NOW + 16 * DAY_MS, None).unwrap();

        assert_eq!(sweep.archived, 1);
        assert_eq!(sweep.staled, 0);
        assert_eq!(
            store.memory_lifecycle("mem-a").unwrap(),
            Some(MemoryLifecycle::Archived)
        );
        let row = store
            .get_memory("mem-a")
            .unwrap()
            .expect("archived rows are never deleted");
        assert!(!row.is_superseded());
        assert_eq!(store.list_memory_all(None).unwrap().len(), 1);
    }

    #[test]
    fn a_sweep_is_idempotent() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let curator = Curator::new(store.clone());
        seed(
            &store,
            "mem-a",
            "project:p1",
            "db",
            "postgres",
            NOW - 15 * DAY_MS,
        );

        assert_eq!(curator.sweep(NOW, None).unwrap().staled, 1);
        let second = curator.sweep(NOW, None).unwrap();

        assert_eq!(
            (second.staled, second.archived, second.reactivated),
            (0, 0, 0)
        );
        assert!(second.ledger_ids.is_empty());
        assert_eq!(curator.ledger(10).unwrap().len(), 1);
        assert_eq!(
            store.memory_lifecycle("mem-a").unwrap(),
            Some(MemoryLifecycle::Stale)
        );
    }

    #[test]
    fn a_transition_can_be_rolled_back_by_id() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let curator = Curator::new(store.clone());
        seed(
            &store,
            "mem-a",
            "project:p1",
            "db",
            "postgres",
            NOW - 15 * DAY_MS,
        );

        let sweep = curator.sweep(NOW, None).unwrap();
        let ledger_id = sweep.ledger_ids[0].clone();
        let entry = &curator.ledger(1).unwrap()[0];
        assert_eq!(entry.id, ledger_id);
        assert_eq!(entry.memory_id, "mem-a");
        assert_eq!(entry.memory_key, "db");
        assert_eq!(entry.memory_scope, "project:p1");
        assert_eq!(entry.from_state, MemoryLifecycle::Active);
        assert_eq!(entry.to_state, MemoryLifecycle::Stale);
        assert_eq!(entry.before_hash, entry.after_hash);
        assert_eq!(entry.before_hash, hash_value("postgres"));
        assert_eq!(entry.at_ms, NOW);
        assert!(entry.reverted_by.is_none());

        curator.rollback(&ledger_id).unwrap();
        assert_eq!(
            store.memory_lifecycle("mem-a").unwrap(),
            Some(MemoryLifecycle::Active)
        );
        assert!(curator.ledger(1).unwrap()[0].reverted_by.is_some());

        curator.rollback(&ledger_id).unwrap();
        assert_eq!(
            store.memory_lifecycle("mem-a").unwrap(),
            Some(MemoryLifecycle::Active)
        );
        assert_eq!(curator.ledger(10).unwrap().len(), 1);
        assert!(curator.rollback("curator-missing").is_err());
    }

    #[test]
    fn archived_entries_are_not_offered_as_live_memory() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let curator = Curator::new(store.clone());
        seed(
            &store,
            "mem-a",
            "project:p1",
            "db",
            "postgres",
            NOW - 31 * DAY_MS,
        );

        assert_eq!(curator.sweep(NOW, None).unwrap().archived, 1);

        let memory = LayeredMemory::new(store.clone());
        assert!(memory.list(None).unwrap().is_empty());
        assert!(memory.recall(None, "postgres", 10).unwrap().is_empty());
        assert!(store.list_memory(None).unwrap().is_empty());
        assert!(
            store
                .search_memory(None, "postgres", 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.count_memory().unwrap(), 0);
        assert!(store.get_memory("mem-a").unwrap().is_some());
        assert_eq!(store.list_memory_all(None).unwrap().len(), 1);
    }

    #[test]
    fn a_scoped_sweep_leaves_other_scopes_alone() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let curator = Curator::new(store.clone());
        seed(
            &store,
            "mem-p1",
            "project:p1",
            "db",
            "postgres",
            NOW - 15 * DAY_MS,
        );
        seed(
            &store,
            "mem-other",
            "agent:other",
            "db",
            "mysql",
            NOW - 15 * DAY_MS,
        );

        let sweep = curator.sweep(NOW, Some("project:p1")).unwrap();

        assert_eq!(sweep.staled, 1);
        assert_eq!(
            store.memory_lifecycle("mem-p1").unwrap(),
            Some(MemoryLifecycle::Stale)
        );
        assert_eq!(
            store.memory_lifecycle("mem-other").unwrap(),
            Some(MemoryLifecycle::Active)
        );
    }

    #[test]
    fn touching_an_entry_recovers_it_to_active_and_is_ledgered() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let curator = Curator::new(store.clone());
        seed(
            &store,
            "mem-a",
            "project:p1",
            "db",
            "postgres",
            NOW - 15 * DAY_MS,
        );
        curator.sweep(NOW, None).unwrap();

        assert!(curator.touch("mem-a").unwrap());

        assert_eq!(
            store.memory_lifecycle("mem-a").unwrap(),
            Some(MemoryLifecycle::Active)
        );
        let entries = curator.ledger(10).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].to_state, MemoryLifecycle::Active);
        assert_eq!(entries[0].from_state, MemoryLifecycle::Stale);
        assert_eq!(entries[1].to_state, MemoryLifecycle::Stale);
        assert!(!curator.touch("mem-missing").unwrap());
    }

    #[test]
    fn a_sweep_reactivates_an_entry_whose_timestamp_moved() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let curator = Curator::new(store.clone());
        seed(
            &store,
            "mem-a",
            "project:p1",
            "db",
            "postgres",
            NOW - 15 * DAY_MS,
        );
        curator.sweep(NOW, None).unwrap();

        let mut row = store.get_memory("mem-a").unwrap().unwrap();
        row.updated_ms = NOW;
        store.save_memory(&row).unwrap();
        assert_eq!(
            store.memory_lifecycle("mem-a").unwrap(),
            Some(MemoryLifecycle::Stale),
            "rewriting content is not a lifecycle decision"
        );

        let sweep = curator.sweep(NOW, None).unwrap();

        assert_eq!(sweep.reactivated, 1);
        assert_eq!(
            store.memory_lifecycle("mem-a").unwrap(),
            Some(MemoryLifecycle::Active)
        );
    }

    #[test]
    fn a_superseded_row_is_not_aged() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let curator = Curator::new(store.clone());
        let mut row = MemoryEntry {
            id: "mem-dead".to_string(),
            layer: MemoryLayer::Semantic,
            scope: "project:p1".to_string(),
            key: "db".to_string(),
            value: "postgres".to_string(),
            status: KnowledgeStatus::Observed,
            confidence: 0.8,
            source_experience: None,
            provenance: "curator-test".to_string(),
            created_ms: NOW - 40 * DAY_MS,
            updated_ms: NOW - 40 * DAY_MS,
            expires_ms: None,
            superseded_by: Some("mem-live".to_string()),
        };
        store.save_memory(&row).unwrap();
        row.id = "mem-live".to_string();
        row.superseded_by = None;
        store.save_memory(&row).unwrap();

        let sweep = curator.sweep(NOW, None).unwrap();

        assert_eq!(sweep.archived, 1);
        assert_eq!(
            store.memory_lifecycle("mem-dead").unwrap(),
            Some(MemoryLifecycle::Active)
        );
        assert_eq!(
            store.memory_lifecycle("mem-live").unwrap(),
            Some(MemoryLifecycle::Archived)
        );
    }

    #[test]
    fn the_engine_exposes_the_curator() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = EvolutionEngine::open_in_memory(tmp.path().join("skills")).unwrap();

        assert!(engine.curator.ledger(10).unwrap().is_empty());
        assert!(
            engine
                .curator
                .sweep(now_ms(), None)
                .unwrap()
                .ledger_ids
                .is_empty()
        );
    }
}
