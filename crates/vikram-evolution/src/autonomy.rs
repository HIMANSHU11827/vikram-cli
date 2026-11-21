//! Autonomy Manager: Implement autonomy levels and safety controls.
//!
//! This engine classifies actions by risk and determines approval requirements.
//!
//! Producer: the runtime's learning pipeline requests an approval whenever a
//! skill proposal is staged for operator decision (a pending capability
//! change), so `vikram evolution approvals` lists real work. Approving or
//! rejecting is durable; `execute_controlled` still has no wired caller — the
//! learning proposal remains the gate that performs the skill write.
//!
//! Persistence: every [`ApprovalRequest`] is written to the store's
//! `engine_records` table under [`APPROVAL_NAMESPACE`] on creation and on
//! every status transition, and reloaded by [`AutonomyManager::new`], so an
//! approval granted in one process is still answerable — and still spendable
//! exactly once — after a restart or from another process. `action_history`
//! stays process-local: executed actions already emit `action_executed`
//! learning events, and rebuilding the in-memory list from those events is
//! a reporting concern, not a correctness one.

use crate::error::{EvolutionError, Result};
use crate::store::{EvolutionStore, from_json_value, to_json_value};
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};

/// `engine_records` namespace holding every [`ApprovalRequest`].
const APPROVAL_NAMESPACE: &str = "autonomy.approval";

/// Autonomy levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutonomyLevel {
    Automatic,
    ControlledDevelopment,
    ApprovalRequired,
}

/// Action types for autonomy classification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionType {
    AdjustModelRouting,
    ChangeRuntimeSetting,
    OptimizeContextStrategy,
    ImproveWorkflow,
    UpdateLearningRecord,
    OrganizeInternalData,
    RunDiagnostics,
    CreateSkillDraft,
    CreateTool,
    ModifySkill,
    DevelopIntegration,
    ChangeWorkflow,
    ModifySourceCode,
    RunTests,
    ReviewChange,
    ValidateChange,
    PrepareUpdate,
    ChangeSecurityPolicy,
    ChangePermissions,
    ModifySecrets,
    DisableProtections,
    DestructiveDataOperation,
    MajorArchitectureMigration,
    ProductionDeployment,
    IrreversibleOperation,
    ExposeUserData,
}

/// Action record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionRecord {
    pub id: String,
    pub action_type: ActionType,
    pub description: String,
    pub autonomy_level: AutonomyLevel,
    pub approved: bool,
    pub executed: bool,
    pub approved_by: Option<String>,
    pub timestamp_ms: u64,
}

/// Approval request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub action_type: ActionType,
    pub description: String,
    pub reason: String,
    pub risk_level: AutonomyLevel,
    pub timestamp_ms: u64,
    pub status: ApprovalStatus,
    pub approved_by: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    /// Approved and already spent by one [`AutonomyManager::execute_controlled`]
    /// call. An approval authorizes exactly one execution.
    Executed,
}

pub struct AutonomyManager {
    store: EvolutionStore,
    action_history: Vec<ActionRecord>,
    pending_approvals: Vec<ApprovalRequest>,
}

impl AutonomyManager {
    pub fn new(store: EvolutionStore) -> Result<Self> {
        let pending_approvals = store
            .list_engine_records(APPROVAL_NAMESPACE)?
            .into_iter()
            .map(|(_, payload)| from_json_value::<ApprovalRequest>(payload))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            store,
            action_history: Vec::new(),
            pending_approvals,
        })
    }

    /// Write one request's current state to the store. Called at every
    /// mutation boundary (`request_approval`, `approve_request`,
    /// `reject_request`, `execute_controlled`) so the durable copy never
    /// lags the in-memory one.
    fn persist_request(store: &EvolutionStore, request: &ApprovalRequest) -> Result<()> {
        store.save_engine_record(APPROVAL_NAMESPACE, &request.id, &to_json_value(request)?)
    }

    /// Determine autonomy level for an action.
    pub fn determine_autonomy_level(&self, action: &ActionType) -> AutonomyLevel {
        match action {
            ActionType::AdjustModelRouting
            | ActionType::ChangeRuntimeSetting
            | ActionType::OptimizeContextStrategy
            | ActionType::ImproveWorkflow
            | ActionType::UpdateLearningRecord
            | ActionType::OrganizeInternalData
            | ActionType::RunDiagnostics
            | ActionType::CreateSkillDraft => AutonomyLevel::Automatic,

            ActionType::CreateTool
            | ActionType::ModifySkill
            | ActionType::DevelopIntegration
            | ActionType::ChangeWorkflow
            | ActionType::ModifySourceCode
            | ActionType::RunTests
            | ActionType::ReviewChange
            | ActionType::ValidateChange
            | ActionType::PrepareUpdate => AutonomyLevel::ControlledDevelopment,

            ActionType::ChangeSecurityPolicy
            | ActionType::ChangePermissions
            | ActionType::ModifySecrets
            | ActionType::DisableProtections
            | ActionType::DestructiveDataOperation
            | ActionType::MajorArchitectureMigration
            | ActionType::ProductionDeployment
            | ActionType::IrreversibleOperation
            | ActionType::ExposeUserData => AutonomyLevel::ApprovalRequired,
        }
    }

    /// Request approval for an action.
    ///
    /// The request is persisted before it is returned: an approval that only
    /// exists in memory would be unanswerable after a restart.
    pub fn request_approval(
        &mut self,
        action: ActionType,
        description: &str,
        reason: &str,
    ) -> Result<ApprovalRequest> {
        let level = self.determine_autonomy_level(&action);

        let request = ApprovalRequest {
            id: gen_id("approval"),
            action_type: action,
            description: description.to_string(),
            reason: reason.to_string(),
            risk_level: level,
            timestamp_ms: now_ms(),
            status: ApprovalStatus::Pending,
            approved_by: None,
        };

        self.pending_approvals.push(request.clone());
        if let Err(e) = Self::persist_request(&self.store, &request) {
            self.pending_approvals.pop();
            return Err(e);
        }
        Ok(request)
    }

    /// Execute an automatic action.
    ///
    /// Only actions classified [`AutonomyLevel::Automatic`] by
    /// [`Self::determine_autonomy_level`] may run through this path. Anything
    /// requiring controlled development or approval is rejected; use
    /// [`Self::request_approval`] followed by [`Self::execute_controlled`]
    /// instead.
    pub fn execute_automatic(
        &mut self,
        action: ActionType,
        description: &str,
    ) -> Result<ActionRecord> {
        let level = self.determine_autonomy_level(&action);
        if level != AutonomyLevel::Automatic {
            return Err(EvolutionError::Security(format!(
                "action {:?} requires {:?} autonomy; automatic execution is only \
                 permitted for Automatic-level actions",
                action, level
            )));
        }

        let record = ActionRecord {
            id: gen_id("action"),
            action_type: action,
            description: description.to_string(),
            autonomy_level: level,
            approved: true,
            executed: true,
            approved_by: Some("system".to_string()),
            timestamp_ms: now_ms(),
        };

        self.action_history.push(record.clone());
        self.record_event(&record)?;
        Ok(record)
    }

    /// Execute a controlled development or approval-required action.
    ///
    /// `approval_request_id` must reference an [`ApprovalRequest`] previously
    /// created by [`Self::request_approval`] and marked
    /// [`ApprovalStatus::Approved`] via [`Self::approve_request`], and its
    /// `action_type` must match `action`. A bare caller assertion is not
    /// sufficient — there is no `approved: bool` shortcut.
    pub fn execute_controlled(
        &mut self,
        action: ActionType,
        description: &str,
        approval_request_id: &str,
    ) -> Result<ActionRecord> {
        let idx = self
            .pending_approvals
            .iter()
            .position(|r| r.id == approval_request_id)
            .ok_or_else(|| {
                EvolutionError::NotFound(format!(
                    "approval request {approval_request_id} not found"
                ))
            })?;

        {
            let request = &self.pending_approvals[idx];
            if request.status != ApprovalStatus::Approved {
                return Err(EvolutionError::Security(format!(
                    "approval request {approval_request_id} is {:?}, not Approved",
                    request.status
                )));
            }

            if request.action_type != action {
                return Err(EvolutionError::Validation(format!(
                    "approval request {approval_request_id} covers {:?}, not {:?}",
                    request.action_type, action
                )));
            }
        }

        let request = &self.pending_approvals[idx];
        let level = self.determine_autonomy_level(&action);
        let approved_by = request.approved_by.clone();

        let record = ActionRecord {
            id: gen_id("action"),
            action_type: action,
            description: description.to_string(),
            autonomy_level: level,
            approved: true,
            executed: true,
            approved_by,
            timestamp_ms: now_ms(),
        };

        self.action_history.push(record.clone());
        self.record_event(&record)?;
        // The approval is consumed by this execution: a second
        // `execute_controlled` with the same id fails the Approved check.
        // Marked only after the event is recorded, so a store failure
        // leaves the approval usable for a retry. The consumed state is
        // persisted too — if that write fails the in-memory status is
        // rolled back so a restart cannot resurrect a spent approval.
        self.pending_approvals[idx].status = ApprovalStatus::Executed;
        if let Err(e) = Self::persist_request(&self.store, &self.pending_approvals[idx]) {
            self.pending_approvals[idx].status = ApprovalStatus::Approved;
            return Err(e);
        }
        Ok(record)
    }

    /// Approve a pending request.
    pub fn approve_request(&mut self, request_id: &str, approved_by: &str) -> Result<()> {
        let req = self
            .pending_approvals
            .iter_mut()
            .find(|r| r.id == request_id)
            .ok_or_else(|| {
                EvolutionError::NotFound(format!("approval request {request_id} not found"))
            })?;

        if req.status != ApprovalStatus::Pending {
            return Err(EvolutionError::Conflict(format!(
                "approval request {request_id} is already {:?}",
                req.status
            )));
        }

        let prev_status = req.status.clone();
        let prev_approved_by = req.approved_by.clone();
        req.status = ApprovalStatus::Approved;
        req.approved_by = Some(approved_by.to_string());
        // Persist before recording history: a failed write rolls the
        // in-memory approval back so memory and store never disagree about
        // whether this request was granted.
        if let Err(e) = Self::persist_request(&self.store, req) {
            req.status = prev_status;
            req.approved_by = prev_approved_by;
            return Err(e);
        }
        let record = ActionRecord {
            id: gen_id("action"),
            action_type: req.action_type.clone(),
            description: req.description.clone(),
            autonomy_level: req.risk_level,
            approved: true,
            executed: false,
            approved_by: Some(approved_by.to_string()),
            timestamp_ms: now_ms(),
        };
        self.action_history.push(record);
        Ok(())
    }

    /// Reject a pending request.
    pub fn reject_request(&mut self, request_id: &str) -> Result<()> {
        let req = self
            .pending_approvals
            .iter_mut()
            .find(|r| r.id == request_id)
            .ok_or_else(|| {
                EvolutionError::NotFound(format!("approval request {request_id} not found"))
            })?;

        if req.status != ApprovalStatus::Pending {
            return Err(EvolutionError::Conflict(format!(
                "approval request {request_id} is already {:?}",
                req.status
            )));
        }

        let prev_status = req.status.clone();
        req.status = ApprovalStatus::Rejected;
        if let Err(e) = Self::persist_request(&self.store, req) {
            req.status = prev_status;
            return Err(e);
        }
        Ok(())
    }

    /// Audit an action.
    pub fn audit_action(&self, action: &ActionRecord) -> Result<()> {
        self.store.save_event(&crate::types::LearningEvent {
            id: gen_id("evt"),
            timestamp_ms: now_ms(),
            agent_id: "system".to_string(),
            kind: "action_audit".to_string(),
            summary: format!("Action audited: {:?}", action.action_type),
            payload: serde_json::json!({
                "action_id": action.id,
                "autonomy_level": format!("{:?}", action.autonomy_level),
                "approved": action.approved,
                "executed": action.executed,
            }),
        })?;
        Ok(())
    }

    /// Get action history.
    pub fn get_action_history(&self) -> &[ActionRecord] {
        &self.action_history
    }

    /// Get pending approvals.
    pub fn get_pending_approvals(&self) -> &[ApprovalRequest] {
        &self.pending_approvals
    }

    fn record_event(&self, action: &ActionRecord) -> Result<()> {
        self.store.save_event(&crate::types::LearningEvent {
            id: gen_id("evt"),
            timestamp_ms: now_ms(),
            agent_id: "system".to_string(),
            kind: "action_executed".to_string(),
            summary: format!("Action executed: {:?}", action.action_type),
            payload: serde_json::json!({
                "action_id": action.id,
                "autonomy_level": format!("{:?}", action.autonomy_level),
            }),
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::EvolutionStore;

    fn manager() -> AutonomyManager {
        AutonomyManager::new(EvolutionStore::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn automatic_execution_rejects_non_automatic_actions() {
        let mut manager = manager();

        for action in [
            ActionType::ModifySourceCode,
            ActionType::ChangeSecurityPolicy,
            ActionType::ModifySecrets,
            ActionType::DisableProtections,
            ActionType::ExposeUserData,
        ] {
            let err = manager
                .execute_automatic(action.clone(), "should not run")
                .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("automatic execution is only"),
                "unexpected error for {action:?}: {msg}"
            );
        }
        assert!(manager.get_action_history().is_empty());
    }

    #[test]
    fn controlled_execution_requires_an_approved_matching_request() {
        let mut manager = manager();

        // No approval request at all.
        assert!(
            manager
                .execute_controlled(ActionType::ModifySourceCode, "change", "approval-missing",)
                .is_err()
        );

        // A pending (unapproved) request is not enough.
        let pending = manager
            .request_approval(ActionType::ModifySourceCode, "change", "need approval")
            .unwrap();
        assert!(
            manager
                .execute_controlled(ActionType::ModifySourceCode, "change", &pending.id,)
                .is_err()
        );

        // An approval for a different action does not cover this one.
        let other = manager
            .request_approval(ActionType::RunTests, "run tests", "need approval")
            .unwrap();
        manager.approve_request(&other.id, "user").unwrap();
        assert!(
            manager
                .execute_controlled(ActionType::ModifySourceCode, "change", &other.id,)
                .is_err()
        );

        // The matching approved request executes and records the real tier.
        manager.approve_request(&pending.id, "user").unwrap();
        let record = manager
            .execute_controlled(ActionType::ModifySourceCode, "change", &pending.id)
            .unwrap();
        assert!(record.approved);
        assert!(record.executed);
        assert_eq!(record.autonomy_level, AutonomyLevel::ControlledDevelopment);
        assert_eq!(record.approved_by.as_deref(), Some("user"));
    }

    #[test]
    fn approval_required_action_executes_only_after_approval() {
        let mut manager = manager();

        let request = manager
            .request_approval(
                ActionType::ChangeSecurityPolicy,
                "loosen policy",
                "requested",
            )
            .unwrap();
        assert_eq!(request.risk_level, AutonomyLevel::ApprovalRequired);
        assert!(
            manager
                .execute_controlled(
                    ActionType::ChangeSecurityPolicy,
                    "loosen policy",
                    &request.id,
                )
                .is_err()
        );

        manager.approve_request(&request.id, "admin").unwrap();
        let record = manager
            .execute_controlled(
                ActionType::ChangeSecurityPolicy,
                "loosen policy",
                &request.id,
            )
            .unwrap();
        assert_eq!(record.autonomy_level, AutonomyLevel::ApprovalRequired);
        assert_eq!(record.approved_by.as_deref(), Some("admin"));
    }

    #[test]
    fn an_approval_authorizes_exactly_one_execution() {
        let mut manager = manager();

        let request = manager
            .request_approval(ActionType::ModifySourceCode, "change", "need approval")
            .unwrap();
        manager.approve_request(&request.id, "user").unwrap();

        manager
            .execute_controlled(ActionType::ModifySourceCode, "change", &request.id)
            .unwrap();

        let err = manager
            .execute_controlled(ActionType::ModifySourceCode, "change", &request.id)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Executed") && err.contains("not Approved"),
            "second use of a spent approval must fail: {err}"
        );
        assert_eq!(manager.get_action_history().len(), 2);
    }

    #[test]
    fn approve_and_reject_fail_for_unknown_ids() {
        let mut manager = manager();

        assert!(manager.approve_request("approval-nope", "user").is_err());
        assert!(manager.reject_request("approval-nope").is_err());
        assert!(manager.get_action_history().is_empty());
    }

    #[test]
    fn approve_and_reject_fail_for_already_resolved_requests() {
        let mut manager = manager();

        let request = manager
            .request_approval(ActionType::RunTests, "tests", "reason")
            .unwrap();
        manager.approve_request(&request.id, "user").unwrap();
        assert!(manager.approve_request(&request.id, "user").is_err());
        assert!(manager.reject_request(&request.id).is_err());

        let rejected = manager
            .request_approval(ActionType::RunTests, "tests", "reason")
            .unwrap();
        manager.reject_request(&rejected.id).unwrap();
        assert!(manager.approve_request(&rejected.id, "user").is_err());
    }

    #[test]
    fn approvals_survive_a_store_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evolution.db");

        let request_id;
        {
            let store = EvolutionStore::open(&path).unwrap();
            let mut manager = AutonomyManager::new(store).unwrap();
            let pending = manager
                .request_approval(ActionType::ModifySourceCode, "change", "need approval")
                .unwrap();
            request_id = pending.id.clone();
            manager.approve_request(&pending.id, "user").unwrap();
        }

        let store = EvolutionStore::open(&path).unwrap();
        let mut manager = AutonomyManager::new(store).unwrap();
        let restored = manager
            .get_pending_approvals()
            .iter()
            .find(|r| r.id == request_id)
            .expect("approved request must survive a reopen");
        assert_eq!(restored.status, ApprovalStatus::Approved);
        assert_eq!(restored.approved_by.as_deref(), Some("user"));

        // The restored approval is still spendable exactly once.
        let record = manager
            .execute_controlled(ActionType::ModifySourceCode, "change", &request_id)
            .unwrap();
        assert_eq!(record.approved_by.as_deref(), Some("user"));
        assert!(
            manager
                .execute_controlled(ActionType::ModifySourceCode, "change", &request_id)
                .is_err()
        );

        // And the spent state is durable too.
        let store = EvolutionStore::open(&path).unwrap();
        let manager = AutonomyManager::new(store).unwrap();
        let spent = manager
            .get_pending_approvals()
            .iter()
            .find(|r| r.id == request_id)
            .unwrap();
        assert_eq!(spent.status, ApprovalStatus::Executed);
    }

    #[test]
    fn a_fresh_store_starts_with_no_approvals() {
        let manager = manager();
        assert!(manager.get_pending_approvals().is_empty());
        assert!(manager.get_action_history().is_empty());
    }
}
