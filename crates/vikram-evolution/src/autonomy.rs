//! Autonomy Manager: Implement autonomy levels and safety controls.
//!
//! This engine classifies actions by risk and determines approval requirements.

use crate::error::Result;
use crate::store::EvolutionStore;
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
}

pub struct AutonomyManager {
    store: EvolutionStore,
    action_history: Vec<ActionRecord>,
    pending_approvals: Vec<ApprovalRequest>,
}

impl AutonomyManager {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            action_history: Vec::new(),
            pending_approvals: Vec::new(),
        }
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
    pub fn request_approval(
        &mut self,
        action: ActionType,
        description: &str,
        reason: &str,
    ) -> ApprovalRequest {
        let level = self.determine_autonomy_level(&action);

        let request = ApprovalRequest {
            id: gen_id("approval"),
            action_type: action,
            description: description.to_string(),
            reason: reason.to_string(),
            risk_level: level,
            timestamp_ms: now_ms(),
            status: ApprovalStatus::Pending,
        };

        self.pending_approvals.push(request.clone());
        request
    }

    /// Execute an automatic action.
    pub fn execute_automatic(
        &mut self,
        action: ActionType,
        description: &str,
    ) -> Result<ActionRecord> {
        let record = ActionRecord {
            id: gen_id("action"),
            action_type: action,
            description: description.to_string(),
            autonomy_level: AutonomyLevel::Automatic,
            approved: true,
            executed: true,
            approved_by: Some("system".to_string()),
            timestamp_ms: now_ms(),
        };

        self.action_history.push(record.clone());
        self.record_event(&record)?;
        Ok(record)
    }

    /// Execute a controlled development action.
    pub fn execute_controlled(
        &mut self,
        action: ActionType,
        description: &str,
        approved: bool,
    ) -> Result<ActionRecord> {
        let record = ActionRecord {
            id: gen_id("action"),
            action_type: action,
            description: description.to_string(),
            autonomy_level: AutonomyLevel::ControlledDevelopment,
            approved,
            executed: approved,
            approved_by: if approved {
                Some("user".to_string())
            } else {
                None
            },
            timestamp_ms: now_ms(),
        };

        self.action_history.push(record.clone());
        self.record_event(&record)?;
        Ok(record)
    }

    /// Approve a pending request.
    pub fn approve_request(&mut self, request_id: &str, approved_by: &str) -> Result<()> {
        if let Some(req) = self
            .pending_approvals
            .iter_mut()
            .find(|r| r.id == request_id)
        {
            req.status = ApprovalStatus::Approved;
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
        }
        Ok(())
    }

    /// Reject a pending request.
    pub fn reject_request(&mut self, request_id: &str) -> Result<()> {
        if let Some(req) = self
            .pending_approvals
            .iter_mut()
            .find(|r| r.id == request_id)
        {
            req.status = ApprovalStatus::Rejected;
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
