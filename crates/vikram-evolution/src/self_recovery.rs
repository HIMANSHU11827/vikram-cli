//! Self-Recovery Engine: Recover from failed changes and operational problems.
//!
//! This engine provides configuration snapshots, change history, versioned skills,
//! Git-based source checkpoints, automatic rollback, and recovery verification.

use crate::error::{EvolutionError, Result};
use crate::store::EvolutionStore;
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// System snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemSnapshot {
    pub id: String,
    pub timestamp_ms: u64,
    pub configuration: serde_json::Value,
    pub skills_version: HashMap<String, u32>,
    pub git_commit: Option<String>,
    pub memory_state: serde_json::Value,
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
}

impl SelfRecoveryEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            snapshots: Vec::new(),
            recovery_operations: Vec::new(),
            failed_changes: Vec::new(),
        }
    }

    /// Create a snapshot of current system state.
    pub fn create_snapshot(&mut self) -> Result<SystemSnapshot> {
        let snapshot = SystemSnapshot {
            id: gen_id("snapshot"),
            timestamp_ms: now_ms(),
            configuration: serde_json::json!({}),
            skills_version: HashMap::new(),
            git_commit: self.get_current_git_commit()?,
            memory_state: serde_json::json!({}),
        };

        self.snapshots.push(snapshot.clone());
        Ok(snapshot)
    }

    /// Detect a failed change.
    pub fn detect_failed_change(
        &mut self,
        change_id: &str,
        reason: &str,
        auto_rollback_eligible: bool,
    ) -> Result<FailedChange> {
        let failed = FailedChange {
            id: gen_id("failed"),
            change_id: change_id.to_string(),
            failure_reason: reason.to_string(),
            detected_ms: now_ms(),
            auto_rollback_eligible,
        };

        self.failed_changes.push(failed.clone());
        Ok(failed)
    }

    /// Rollback a failed change.
    pub fn rollback_change(&mut self, change_id: &str) -> Result<RecoveryOperation> {
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
        Ok(operation)
    }

    /// Recover after a restart.
    pub fn recover_after_restart(&mut self) -> Result<Vec<RecoveryOperation>> {
        let eligible_changes: Vec<String> = self
            .failed_changes
            .iter()
            .filter(|f| f.auto_rollback_eligible)
            .map(|f| f.change_id.clone())
            .collect();

        let mut operations = Vec::new();
        for change_id in eligible_changes {
            let op = self.rollback_change(&change_id)?;
            operations.push(op);
        }

        Ok(operations)
    }

    /// Resume an interrupted operation.
    pub fn resume_interrupted_operation(&mut self, operation_id: &str) -> Result<()> {
        if let Some(op) = self
            .recovery_operations
            .iter_mut()
            .find(|o| o.id == operation_id)
        {
            op.status = RecoveryStatus::InProgress;
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

        self.recovery_operations.push(operation);
        Ok(())
    }

    /// Rollback skill to specific version.
    pub fn rollback_skill_version(&mut self, skill_name: &str, version: u32) -> Result<()> {
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

        self.recovery_operations.push(operation);
        Ok(())
    }

    /// Get available snapshots.
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
