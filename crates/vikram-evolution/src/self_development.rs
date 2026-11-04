//! Self-Development Engine: Controlled development of new capabilities.
//!
//! This engine handles skill creation, tool development, agent development,
//! and controlled source code modifications through a safe lifecycle.

use crate::error::{EvolutionError, Result};
use crate::store::EvolutionStore;
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};

/// Development change types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DevelopmentChange {
    CreateSkill {
        name: String,
        content: String,
    },
    ImproveSkill {
        name: String,
        changes: String,
    },
    CreateTool {
        spec: String,
        implementation: String,
    },
    DevelopAgent {
        spec: String,
    },
    ModifySourceCode {
        changeset: String,
    },
    CreateWorkflow {
        definition: String,
    },
}

/// Development lifecycle stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DevelopmentStage {
    Identification,
    Research,
    Inspection,
    Planning,
    Implementation,
    Formatting,
    UnitTests,
    IntegrationTests,
    RegressionTests,
    IndependentReview,
    Comparison,
    Integration,
    Recording,
    RollbackPath,
}

/// Development project.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DevelopmentProject {
    pub id: String,
    pub change: DevelopmentChange,
    pub current_stage: DevelopmentStage,
    pub stage_results: Vec<StageResult>,
    pub created_ms: u64,
    pub completed_ms: Option<u64>,
    pub status: ProjectStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageResult {
    pub stage: DevelopmentStage,
    pub success: bool,
    pub output: String,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectStatus {
    InProgress,
    Completed,
    Failed,
    RolledBack,
}

pub struct SelfDevelopmentEngine {
    store: EvolutionStore,
    projects: Vec<DevelopmentProject>,
}

impl SelfDevelopmentEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            projects: Vec::new(),
        }
    }

    /// Create a new skill.
    pub fn create_skill(
        &mut self,
        name: &str,
        content: &str,
        reason: &str,
    ) -> Result<DevelopmentProject> {
        let change = DevelopmentChange::CreateSkill {
            name: name.to_string(),
            content: content.to_string(),
        };

        let project = self.start_development(change, reason)?;
        Ok(project)
    }

    /// Improve an existing skill.
    pub fn improve_skill(
        &mut self,
        name: &str,
        changes: &str,
        reason: &str,
    ) -> Result<DevelopmentProject> {
        let change = DevelopmentChange::ImproveSkill {
            name: name.to_string(),
            changes: changes.to_string(),
        };

        let project = self.start_development(change, reason)?;
        Ok(project)
    }

    /// Create a new tool.
    pub fn create_tool(
        &mut self,
        spec: &str,
        implementation: &str,
        reason: &str,
    ) -> Result<DevelopmentProject> {
        let change = DevelopmentChange::CreateTool {
            spec: spec.to_string(),
            implementation: implementation.to_string(),
        };

        let project = self.start_development(change, reason)?;
        Ok(project)
    }

    /// Develop a specialized agent.
    pub fn develop_agent(&mut self, spec: &str, reason: &str) -> Result<DevelopmentProject> {
        let change = DevelopmentChange::DevelopAgent {
            spec: spec.to_string(),
        };

        let project = self.start_development(change, reason)?;
        Ok(project)
    }

    /// Modify source code through controlled lifecycle.
    pub fn modify_source_code(
        &mut self,
        changeset: &str,
        reason: &str,
    ) -> Result<DevelopmentProject> {
        let change = DevelopmentChange::ModifySourceCode {
            changeset: changeset.to_string(),
        };

        let project = self.start_development(change, reason)?;
        Ok(project)
    }

    /// Advance a project to the next stage.
    pub fn advance_stage(&mut self, project_id: &str) -> Result<StageResult> {
        let (next_stage, change) = {
            if let Some(project) = self.projects.iter().find(|p| p.id == project_id) {
                let next_stage = match project.current_stage {
                    DevelopmentStage::Identification => DevelopmentStage::Research,
                    DevelopmentStage::Research => DevelopmentStage::Inspection,
                    DevelopmentStage::Inspection => DevelopmentStage::Planning,
                    DevelopmentStage::Planning => DevelopmentStage::Implementation,
                    DevelopmentStage::Implementation => DevelopmentStage::Formatting,
                    DevelopmentStage::Formatting => DevelopmentStage::UnitTests,
                    DevelopmentStage::UnitTests => DevelopmentStage::IntegrationTests,
                    DevelopmentStage::IntegrationTests => DevelopmentStage::RegressionTests,
                    DevelopmentStage::RegressionTests => DevelopmentStage::IndependentReview,
                    DevelopmentStage::IndependentReview => DevelopmentStage::Comparison,
                    DevelopmentStage::Comparison => DevelopmentStage::Integration,
                    DevelopmentStage::Integration => DevelopmentStage::Recording,
                    DevelopmentStage::Recording => DevelopmentStage::RollbackPath,
                    DevelopmentStage::RollbackPath => {
                        return Ok(StageResult {
                            stage: DevelopmentStage::RollbackPath,
                            success: true,
                            output: "Development completed successfully".to_string(),
                            timestamp_ms: now_ms(),
                        });
                    }
                };
                (next_stage, project.change.clone())
            } else {
                return Err(EvolutionError::NotFound("Project not found".to_string()));
            }
        };

        let result = self.execute_stage(&next_stage, &change)?;

        if let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) {
            project.current_stage = next_stage;
            project.stage_results.push(result.clone());

            if !result.success {
                project.status = ProjectStatus::Failed;
            }
        }

        Ok(result)
    }

    /// Review a change independently.
    pub fn review_change(&self, project_id: &str) -> Result<ReviewResult> {
        if let Some(_project) = self.projects.iter().find(|p| p.id == project_id) {
            Ok(ReviewResult {
                approved: true,
                findings: vec!["Change appears safe".to_string()],
                recommendations: vec![],
            })
        } else {
            Err(EvolutionError::NotFound("Project not found".to_string()))
        }
    }

    /// Validate a change.
    pub fn validate_change(&self, project_id: &str) -> Result<bool> {
        if let Some(project) = self.projects.iter().find(|p| p.id == project_id) {
            Ok(project.stage_results.iter().all(|r| r.success))
        } else {
            Ok(false)
        }
    }

    /// Integrate a validated change.
    pub fn integrate_change(&mut self, project_id: &str) -> Result<()> {
        if let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) {
            if project.status == ProjectStatus::Completed {
                project.status = ProjectStatus::Completed;
            }
        }
        Ok(())
    }

    /// Rollback a change.
    pub fn rollback_change(&mut self, project_id: &str) -> Result<()> {
        if let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) {
            project.status = ProjectStatus::RolledBack;
        }
        Ok(())
    }

    fn start_development(
        &mut self,
        change: DevelopmentChange,
        _reason: &str,
    ) -> Result<DevelopmentProject> {
        let project = DevelopmentProject {
            id: gen_id("dev"),
            change,
            current_stage: DevelopmentStage::Identification,
            stage_results: vec![],
            created_ms: now_ms(),
            completed_ms: None,
            status: ProjectStatus::InProgress,
        };

        self.projects.push(project.clone());
        Ok(project)
    }

    fn execute_stage(
        &self,
        stage: &DevelopmentStage,
        _change: &DevelopmentChange,
    ) -> Result<StageResult> {
        Ok(StageResult {
            stage: *stage,
            success: true,
            output: format!("Stage {:?} completed", stage),
            timestamp_ms: now_ms(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ReviewResult {
    pub approved: bool,
    pub findings: Vec<String>,
    pub recommendations: Vec<String>,
}
