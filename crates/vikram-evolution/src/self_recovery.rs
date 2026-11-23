//! Self-Recovery Engine: Recover from failed changes and operational problems.
//!
//! PARTIALLY LIVE: `vikram evolution recover` calls
//! [`SelfRecoveryEngine::recover_after_restart`]. Snapshots are real: each
//! [`SelfRecoveryEngine::create_snapshot`] captures a [`StateFingerprint`]
//! of the shared store (experience/memory/event counts, proposal and
//! reflection-job tallies, per-skill head versions) and persists the whole
//! [`SystemSnapshot`] to the store's `engine_records` table, so snapshots
//! survive restarts and [`SelfRecoveryEngine::detect_state_drift`] can
//! honestly report "state changed while away" by diffing the newest
//! persisted fingerprint against the live store.
//!
//! Durable history too: every [`FailedChange`] and [`RecoveryOperation`] is
//! written as one `engine_records` row at creation and at every status
//! transition, under [`FAILED_CHANGE_NAMESPACE`] and
//! [`OPERATION_NAMESPACE`], and reloaded by
//! [`SelfRecoveryEngine::hydrate`]. So
//! [`SelfRecoveryEngine::recover_after_restart`] reports the operations
//! recorded before a restart instead of an empty list. No rollback actually
//! restores prior state (operations are recorded as `Pending` and never
//! executed). Drift detection reports; it does not remediate.

use crate::error::{EvolutionError, Result};
use crate::metrics::{ProposalStats, ReflectionStats};
use crate::store::{EvolutionStore, from_json_value, to_json_value};
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `engine_records` namespace holding every persisted [`SystemSnapshot`].
const SNAPSHOT_NAMESPACE: &str = "self_recovery.snapshot";
/// `engine_records` namespace holding every persisted [`RecoveryOperation`].
const OPERATION_NAMESPACE: &str = "self_recovery.operation";
/// `engine_records` namespace holding every persisted [`FailedChange`].
const FAILED_CHANGE_NAMESPACE: &str = "self_recovery.failed_change";

/// Store-derived state captured at snapshot time. Every field is a count or
/// version read back from the [`EvolutionStore`]; the fingerprint exists so
/// [`SelfRecoveryEngine::detect_state_drift`] can diff "then" against "now"
/// without pretending to know *why* anything changed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateFingerprint {
    pub captured_ms: u64,
    pub experiences: usize,
    pub live_memories: usize,
    pub learning_events: usize,
    pub self_check_events: usize,
    pub proposals: ProposalStats,
    pub reflection_jobs: ReflectionStats,
    /// Highest recorded version per skill name.
    pub skill_versions: HashMap<String, u32>,
}

/// What changed between the newest persisted snapshot and the live store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateDrift {
    pub snapshot_id: String,
    pub snapshot_ms: u64,
    pub detected_ms: u64,
    /// Human-readable diffs, e.g. `"experiences: 3 -> 5"` or
    /// `"skill 'demo': v1 -> v2"`.
    pub changes: Vec<String>,
}

/// System snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemSnapshot {
    pub id: String,
    pub timestamp_ms: u64,
    pub configuration: serde_json::Value,
    pub skills_version: HashMap<String, u32>,
    pub git_commit: Option<String>,
    pub memory_state: serde_json::Value,
    /// The store-state fingerprint captured alongside the snapshot fields.
    /// `#[serde(default)]` keeps payloads written before this field existed
    /// readable — they simply carry no fingerprint to diff against.
    #[serde(default)]
    pub state_fingerprint: Option<StateFingerprint>,
}

/// Recovery operation types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryType {
    ConfigurationRollback,
    SkillVersionRollback,
    SourceCodeRollback,
    MemoryStateRestoration,
    ProviderReconfiguration,
    ToolReenablement,
}

/// Recovery operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryOperation {
    pub id: String,
    pub recovery_type: RecoveryType,
    pub target: String,
    pub from_snapshot: String,
    pub reason: String,
    pub status: RecoveryStatus,
    pub started_ms: u64,
    pub completed_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Failed change detection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailedChange {
    pub id: String,
    pub change_id: String,
    pub failure_reason: String,
    pub detected_ms: u64,
    pub auto_rollback_eligible: bool,
}

pub struct SelfRecoveryEngine {
    store: EvolutionStore,
    snapshots: Vec<SystemSnapshot>,
    recovery_operations: Vec<RecoveryOperation>,
    failed_changes: Vec<FailedChange>,
    /// Set by [`SelfRecoveryEngine::recover_after_restart`]; the drift found
    /// between the newest persisted snapshot and the live store, if any.
    last_drift: Option<StateDrift>,
    /// Whether `snapshots`, `recovery_operations`, and `failed_changes` have
    /// been hydrated from `engine_records`.
    hydrated: bool,
}

impl SelfRecoveryEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            snapshots: Vec::new(),
            recovery_operations: Vec::new(),
            failed_changes: Vec::new(),
            last_drift: None,
            hydrated: false,
        }
    }

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Load persisted snapshots, recovery operations, and failed changes into
    /// memory once. Mutating entry points call this first so post-restart
    /// callers see the durable history; a corrupt payload is an error, not a
    /// silently-empty list.
    fn hydrate(&mut self) -> Result<()> {
        if self.hydrated {
            return Ok(());
        }
        let mut snapshots = Vec::new();
        for (id, payload) in self.store.list_engine_records(SNAPSHOT_NAMESPACE)? {
            let mut snapshot: SystemSnapshot = from_json_value(payload)?;
            if snapshot.id.is_empty() {
                snapshot.id = id;
            }
            snapshots.push(snapshot);
        }
        let recovery_operations = self.load_operations()?;
        let failed_changes = self.load_failed_changes()?;
        self.snapshots = snapshots;
        self.recovery_operations = recovery_operations;
        self.failed_changes = failed_changes;
        self.hydrated = true;
        Ok(())
    }

    fn load_operations(&self) -> Result<Vec<RecoveryOperation>> {
        self.store
            .list_engine_records(OPERATION_NAMESPACE)?
            .into_iter()
            .map(|(_, payload)| from_json_value(payload))
            .collect()
    }

    fn load_failed_changes(&self) -> Result<Vec<FailedChange>> {
        self.store
            .list_engine_records(FAILED_CHANGE_NAMESPACE)?
            .into_iter()
            .map(|(_, payload)| from_json_value(payload))
            .collect()
    }

    fn persist_operation(&self, operation: &RecoveryOperation) -> Result<()> {
        self.store.save_engine_record(
            OPERATION_NAMESPACE,
            &operation.id,
            &to_json_value(operation)?,
        )
    }

    fn persist_failed_change(&self, change: &FailedChange) -> Result<()> {
        self.store
            .save_engine_record(FAILED_CHANGE_NAMESPACE, &change.id, &to_json_value(change)?)
    }

    /// Fingerprint the store as it stands right now.
    fn current_fingerprint(&self) -> Result<StateFingerprint> {
        let proposals = self.store.proposal_stats()?;
        let reflection_jobs = self.store.reflection_stats()?;
        let skill_versions: HashMap<String, u32> =
            self.store.latest_skill_versions()?.into_iter().collect();
        Ok(StateFingerprint {
            captured_ms: now_ms(),
            experiences: self.store.count_experiences()?,
            live_memories: self.store.count_memory()?,
            learning_events: self.store.count_events()?,
            self_check_events: self.store.count_events_of_kind("self_check")?,
            proposals,
            reflection_jobs,
            skill_versions,
        })
    }

    /// Create a snapshot of current system state and persist it.
    pub fn create_snapshot(&mut self) -> Result<SystemSnapshot> {
        self.hydrate()?;
        let fingerprint = self.current_fingerprint()?;
        let snapshot = SystemSnapshot {
            id: gen_id("snapshot"),
            timestamp_ms: now_ms(),
            configuration: serde_json::json!({
                "experiences": fingerprint.experiences,
                "learning_events": fingerprint.learning_events,
                "self_check_events": fingerprint.self_check_events,
                "proposals_pending": fingerprint.proposals.pending,
                "proposals_applied": fingerprint.proposals.applied,
                "reflection_backlog": fingerprint.reflection_jobs.backlog(),
                "reflection_failed": fingerprint.reflection_jobs.failed,
            }),
            skills_version: fingerprint.skill_versions.clone().into_iter().collect(),
            git_commit: self.get_current_git_commit()?,
            memory_state: serde_json::json!({ "live_entries": fingerprint.live_memories }),
            state_fingerprint: Some(fingerprint),
        };

        self.store.save_engine_record(
            SNAPSHOT_NAMESPACE,
            &snapshot.id,
            &to_json_value(&snapshot)?,
        )?;
        self.snapshots.push(snapshot.clone());
        Ok(snapshot)
    }

    /// Detect drift between the newest persisted snapshot's fingerprint and
    /// the live store. `Ok(None)` means no snapshot exists yet or nothing
    /// changed; `Some` lists exactly which tallies moved.
    pub fn detect_state_drift(&mut self) -> Result<Option<StateDrift>> {
        self.hydrate()?;
        let Some(snapshot) = self.snapshots.last() else {
            return Ok(None);
        };
        let Some(old) = &snapshot.state_fingerprint else {
            return Ok(None);
        };
        let new = self.current_fingerprint()?;
        let changes = diff_fingerprints(old, &new);
        if changes.is_empty() {
            return Ok(None);
        }
        Ok(Some(StateDrift {
            snapshot_id: snapshot.id.clone(),
            snapshot_ms: snapshot.timestamp_ms,
            detected_ms: now_ms(),
            changes,
        }))
    }

    /// Drift found by the most recent [`SelfRecoveryEngine::recover_after_restart`].
    pub fn last_drift_report(&self) -> Option<&StateDrift> {
        self.last_drift.as_ref()
    }

    /// Detect a failed change.
    pub fn detect_failed_change(
        &mut self,
        change_id: &str,
        reason: &str,
        auto_rollback_eligible: bool,
    ) -> Result<FailedChange> {
        self.hydrate()?;
        let failed = FailedChange {
            id: gen_id("failed"),
            change_id: change_id.to_string(),
            failure_reason: reason.to_string(),
            detected_ms: now_ms(),
            auto_rollback_eligible,
        };

        self.failed_changes.push(failed.clone());
        if let Err(e) = self.persist_failed_change(&failed) {
            self.failed_changes.pop();
            return Err(e);
        }
        Ok(failed)
    }

    /// Rollback a failed change.
    pub fn rollback_change(&mut self, change_id: &str) -> Result<RecoveryOperation> {
        self.hydrate()?;
        let snapshot = self
            .snapshots
            .last()
            .ok_or_else(|| EvolutionError::NotFound("No snapshot available".to_string()))?;

        let operation = RecoveryOperation {
            id: gen_id("recovery"),
            recovery_type: RecoveryType::ConfigurationRollback,
            target: change_id.to_string(),
            from_snapshot: snapshot.id.clone(),
            reason: "Failed change detected".to_string(),
            status: RecoveryStatus::Pending,
            started_ms: now_ms(),
            completed_ms: None,
        };

        self.recovery_operations.push(operation.clone());
        if let Err(e) = self.persist_operation(&operation) {
            self.recovery_operations.pop();
            return Err(e);
        }
        Ok(operation)
    }

    /// Recover after a restart.
    ///
    /// Persisted snapshots, failed changes, and recovery operations are
    /// reloaded first; snapshots are diffed against the live store
    /// ([`SelfRecoveryEngine::last_drift_report`] exposes the result) and
    /// every reloaded failed change marked auto-rollback-eligible gets a
    /// `Pending` recovery operation — intent is recorded, nothing is
    /// restored. Returns the persisted recovery history plus whatever this
    /// pass queued; it is empty only when nothing was ever recorded.
    pub fn recover_after_restart(&mut self) -> Result<Vec<RecoveryOperation>> {
        self.last_drift = self.detect_state_drift()?;

        let eligible_changes: Vec<String> = self
            .failed_changes
            .iter()
            .filter(|f| f.auto_rollback_eligible)
            .map(|f| f.change_id.clone())
            .collect();

        for change_id in eligible_changes {
            self.rollback_change(&change_id)?;
        }

        Ok(self.recovery_operations.clone())
    }

    /// Resume an interrupted operation.
    pub fn resume_interrupted_operation(&mut self, operation_id: &str) -> Result<()> {
        self.hydrate()?;
        let Some(index) = self
            .recovery_operations
            .iter()
            .position(|o| o.id == operation_id)
        else {
            return Ok(());
        };
        let previous = self.recovery_operations[index].status.clone();
        self.recovery_operations[index].status = RecoveryStatus::InProgress;
        let updated = self.recovery_operations[index].clone();
        if let Err(e) = self.persist_operation(&updated) {
            self.recovery_operations[index].status = previous;
            return Err(e);
        }
        Ok(())
    }

    /// Verify that recovery was successful.
    pub fn verify_recovery(&self, recovery_id: &str) -> Result<bool> {
        if let Some(op) = self
            .recovery_operations
            .iter()
            .find(|o| o.id == recovery_id)
        {
            Ok(op.status == RecoveryStatus::Completed)
        } else {
            Ok(false)
        }
    }

    /// Restore configuration from snapshot.
    pub fn restore_configuration(&mut self, snapshot_id: &str) -> Result<()> {
        self.hydrate()?;
        let snapshot = self
            .snapshots
            .iter()
            .find(|s| s.id == snapshot_id)
            .ok_or_else(|| EvolutionError::NotFound("Snapshot not found".to_string()))?;

        let operation = RecoveryOperation {
            id: gen_id("recovery"),
            recovery_type: RecoveryType::ConfigurationRollback,
            target: "configuration".to_string(),
            from_snapshot: snapshot.id.clone(),
            reason: "Manual configuration restore".to_string(),
            status: RecoveryStatus::Pending,
            started_ms: now_ms(),
            completed_ms: None,
        };

        self.recovery_operations.push(operation.clone());
        if let Err(e) = self.persist_operation(&operation) {
            self.recovery_operations.pop();
            return Err(e);
        }
        Ok(())
    }

    /// Rollback skill to specific version.
    pub fn rollback_skill_version(&mut self, skill_name: &str, version: u32) -> Result<()> {
        self.hydrate()?;
        let operation = RecoveryOperation {
            id: gen_id("recovery"),
            recovery_type: RecoveryType::SkillVersionRollback,
            target: format!("{}:{}", skill_name, version),
            from_snapshot: "skill_system".to_string(),
            reason: "Skill rollback requested".to_string(),
            status: RecoveryStatus::Pending,
            started_ms: now_ms(),
            completed_ms: None,
        };

        self.recovery_operations.push(operation.clone());
        if let Err(e) = self.persist_operation(&operation) {
            self.recovery_operations.pop();
            return Err(e);
        }
        Ok(())
    }

    /// Get available snapshots.
    ///
    /// In-memory view: populated by [`SelfRecoveryEngine::create_snapshot`]
    /// and by the first mutating call after construction (which hydrates
    /// from the store). A freshly constructed engine that has only been
    /// read reports an empty list even if snapshots exist on disk — call
    /// [`SelfRecoveryEngine::detect_state_drift`] or
    /// [`SelfRecoveryEngine::recover_after_restart`] first to hydrate.
    pub fn get_snapshots(&self) -> &[SystemSnapshot] {
        &self.snapshots
    }

    /// Get recovery history.
    pub fn get_recovery_history(&self) -> &[RecoveryOperation] {
        &self.recovery_operations
    }

    /// Get failed changes.
    pub fn get_failed_changes(&self) -> &[FailedChange] {
        &self.failed_changes
    }

    fn get_current_git_commit(&self) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Field-by-field diff of two fingerprints. Returns one human-readable
/// string per tally that moved; an empty vec means "no drift detected".
fn diff_fingerprints(old: &StateFingerprint, new: &StateFingerprint) -> Vec<String> {
    let mut changes = Vec::new();
    let mut push = |label: &str, before: usize, after: usize| {
        if before != after {
            changes.push(format!("{label}: {before} -> {after}"));
        }
    };
    push("experiences", old.experiences, new.experiences);
    push("live_memories", old.live_memories, new.live_memories);
    push("learning_events", old.learning_events, new.learning_events);
    push(
        "self_check_events",
        old.self_check_events,
        new.self_check_events,
    );
    push("proposals.total", old.proposals.total, new.proposals.total);
    push(
        "proposals.pending",
        old.proposals.pending,
        new.proposals.pending,
    );
    push(
        "proposals.approved",
        old.proposals.approved,
        new.proposals.approved,
    );
    push(
        "proposals.rejected",
        old.proposals.rejected,
        new.proposals.rejected,
    );
    push(
        "proposals.applied",
        old.proposals.applied,
        new.proposals.applied,
    );
    push(
        "reflection_jobs.total",
        old.reflection_jobs.total,
        new.reflection_jobs.total,
    );
    push(
        "reflection_jobs.pending",
        old.reflection_jobs.pending,
        new.reflection_jobs.pending,
    );
    push(
        "reflection_jobs.running",
        old.reflection_jobs.running,
        new.reflection_jobs.running,
    );
    push(
        "reflection_jobs.done",
        old.reflection_jobs.done,
        new.reflection_jobs.done,
    );
    push(
        "reflection_jobs.failed",
        old.reflection_jobs.failed,
        new.reflection_jobs.failed,
    );

    let mut names: Vec<&String> = old
        .skill_versions
        .keys()
        .chain(new.skill_versions.keys())
        .collect();
    names.sort();
    names.dedup();
    for name in names {
        match (old.skill_versions.get(name), new.skill_versions.get(name)) {
            (Some(before), Some(after)) if before != after => {
                changes.push(format!("skill '{name}': v{before} -> v{after}"));
            }
            (Some(before), None) => {
                changes.push(format!("skill '{name}': v{before} -> removed"));
            }
            (None, Some(after)) => {
                changes.push(format!("skill '{name}': added at v{after}"));
            }
            _ => {}
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{KnowledgeStatus, MemoryEntry, MemoryLayer};

    fn engine() -> (SelfRecoveryEngine, EvolutionStore) {
        let store = EvolutionStore::open_in_memory().unwrap();
        let engine = SelfRecoveryEngine::new(store.clone());
        (engine, store)
    }

    fn seed_memory(store: &EvolutionStore, id: &str) {
        store
            .save_memory(&MemoryEntry {
                id: id.to_string(),
                layer: MemoryLayer::Semantic,
                scope: "global".to_string(),
                key: format!("key-{id}"),
                value: "value".to_string(),
                status: KnowledgeStatus::Observed,
                confidence: 0.9,
                source_experience: None,
                provenance: "test".to_string(),
                created_ms: now_ms(),
                updated_ms: now_ms(),
                expires_ms: None,
                superseded_by: None,
            })
            .unwrap();
    }

    #[test]
    fn snapshot_captures_real_store_counts() {
        let (mut engine, store) = engine();
        seed_memory(&store, "mem-1");
        seed_memory(&store, "mem-2");

        let snapshot = engine.create_snapshot().unwrap();
        let fingerprint = snapshot.state_fingerprint.unwrap();
        assert_eq!(fingerprint.live_memories, 2);
        assert_eq!(fingerprint.experiences, 0);
        assert_eq!(snapshot.memory_state["live_entries"], 2);
    }

    #[test]
    fn snapshots_and_fingerprints_survive_a_store_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evolution.db");
        let snapshot_id;

        {
            let store = EvolutionStore::open(&path).unwrap();
            let mut engine = SelfRecoveryEngine::new(store);
            let snapshot = engine.create_snapshot().unwrap();
            snapshot_id = snapshot.id.clone();
        }

        let store = EvolutionStore::open(&path).unwrap();
        let mut engine = SelfRecoveryEngine::new(store);
        assert!(engine.get_snapshots().is_empty(), "not hydrated yet");
        let drift = engine.detect_state_drift().unwrap();
        assert!(drift.is_none(), "identical store: no drift");
        assert_eq!(engine.get_snapshots().len(), 1);
        assert_eq!(engine.get_snapshots()[0].id, snapshot_id);
        assert!(engine.get_snapshots()[0].state_fingerprint.is_some());
    }

    #[test]
    fn drift_is_detected_when_state_changes_while_away() {
        let (mut engine, store) = engine();
        engine.create_snapshot().unwrap();

        // Simulate "state changed while away": new rows land after the
        // snapshot, then a fresh engine instance diffs against it.
        seed_memory(&store, "mem-new");
        store
            .save_skill_version(&crate::types::SkillVersion {
                skill_name: "demo".to_string(),
                version: 3,
                content: "# demo".to_string(),
                created_ms: now_ms(),
                parent_version: Some(2),
                change_description: "post-snapshot change".to_string(),
            })
            .unwrap();

        let mut restarted = SelfRecoveryEngine::new(store);
        let drift = restarted.detect_state_drift().unwrap().unwrap();
        assert!(drift.changes.iter().any(|c| c == "live_memories: 0 -> 1"));
        assert!(
            drift
                .changes
                .iter()
                .any(|c| c == "skill 'demo': added at v3")
        );

        // recover_after_restart surfaces the same report.
        let ops = restarted.recover_after_restart().unwrap();
        assert!(ops.is_empty(), "no in-session failed changes to roll back");
        let report = restarted.last_drift_report().unwrap();
        assert_eq!(report.changes.len(), drift.changes.len());
    }

    #[test]
    fn no_drift_when_nothing_changed() {
        let (mut engine, store) = engine();
        engine.create_snapshot().unwrap();
        let mut restarted = SelfRecoveryEngine::new(store);
        assert!(restarted.detect_state_drift().unwrap().is_none());
    }

    #[test]
    fn drift_is_none_without_any_snapshot() {
        let (mut engine, _) = engine();
        assert!(engine.detect_state_drift().unwrap().is_none());
    }

    #[test]
    fn recovery_operations_survive_a_store_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evolution.db");
        let operation_id;

        {
            let store = EvolutionStore::open(&path).unwrap();
            let mut engine = SelfRecoveryEngine::new(store);
            engine.create_snapshot().unwrap();
            operation_id = engine.rollback_change("change-1").unwrap().id;
        }

        let store = EvolutionStore::open(&path).unwrap();
        let mut engine = SelfRecoveryEngine::new(store);
        assert!(engine.get_recovery_history().is_empty(), "not hydrated yet");
        let operations = engine.recover_after_restart().unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].id, operation_id);
        assert_eq!(operations[0].status, RecoveryStatus::Pending);
        assert_eq!(engine.get_recovery_history().len(), 1);
    }

    #[test]
    fn failed_changes_survive_a_store_reopen_and_queue_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evolution.db");
        let failed_id;

        {
            let store = EvolutionStore::open(&path).unwrap();
            let mut engine = SelfRecoveryEngine::new(store);
            engine.create_snapshot().unwrap();
            failed_id = engine
                .detect_failed_change("change-2", "boom", true)
                .unwrap()
                .id;
        }

        let store = EvolutionStore::open(&path).unwrap();
        let mut engine = SelfRecoveryEngine::new(store);
        assert!(engine.get_failed_changes().is_empty(), "not hydrated yet");
        let operations = engine.recover_after_restart().unwrap();
        assert_eq!(engine.get_failed_changes().len(), 1);
        assert_eq!(engine.get_failed_changes()[0].id, failed_id);
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].target, "change-2");
        assert_eq!(operations[0].status, RecoveryStatus::Pending);
    }

    #[test]
    fn resumed_operation_status_survives_a_store_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evolution.db");
        let operation_id;

        {
            let store = EvolutionStore::open(&path).unwrap();
            let mut engine = SelfRecoveryEngine::new(store);
            engine.create_snapshot().unwrap();
            operation_id = engine.rollback_change("change-3").unwrap().id;
            engine.resume_interrupted_operation(&operation_id).unwrap();
        }

        let store = EvolutionStore::open(&path).unwrap();
        let mut engine = SelfRecoveryEngine::new(store);
        let operations = engine.recover_after_restart().unwrap();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].id, operation_id);
        assert_eq!(operations[0].status, RecoveryStatus::InProgress);
    }

    #[test]
    fn a_fresh_store_recovers_no_operations() {
        let (mut engine, _) = engine();
        assert!(engine.get_failed_changes().is_empty());
        assert!(engine.recover_after_restart().unwrap().is_empty());
        assert!(engine.get_recovery_history().is_empty());
    }
}
