//! Self-Development Engine: Controlled development of new capabilities.
//!
//! This engine handles skill creation, tool development, agent development,
//! and controlled source code modifications through a safe lifecycle.
//!
//! SCAFFOLD / dormant-awaiting-caller: nothing in the runtime or CLI calls
//! this engine, and `projects` is an in-memory `Vec` — every
//! [`DevelopmentProject`] is lost on restart. Stage advancement records
//! intent; it does not produce deployable artifacts on its own.

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

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
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
    ///
    /// Advancing past [`DevelopmentStage::RollbackPath`] finalizes the
    /// lifecycle: the project is marked [`ProjectStatus::Completed`] only if
    /// every recorded stage result succeeded, and
    /// [`ProjectStatus::Failed`] otherwise.
    pub fn advance_stage(&mut self, project_id: &str) -> Result<StageResult> {
        let next = {
            let project = self
                .projects
                .iter()
                .find(|p| p.id == project_id)
                .ok_or_else(|| EvolutionError::NotFound("Project not found".to_string()))?;
            next_stage(project.current_stage).map(|stage| (stage, project.change.clone()))
        };

        let Some((next_stage, change)) = next else {
            return self.finish_project(project_id);
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
    ///
    /// This is a real check over the recorded stage results, not a rubber
    /// stamp: approval requires that the project has progressed past
    /// [`DevelopmentStage::IndependentReview`] and that every recorded stage
    /// succeeded. Failed stages are reported as findings.
    pub fn review_change(&self, project_id: &str) -> Result<ReviewResult> {
        let project = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .ok_or_else(|| EvolutionError::NotFound("Project not found".to_string()))?;

        let past_review = matches!(
            project.current_stage,
            DevelopmentStage::Comparison
                | DevelopmentStage::Integration
                | DevelopmentStage::Recording
                | DevelopmentStage::RollbackPath
        );
        let stages_passed =
            !project.stage_results.is_empty() && project.stage_results.iter().all(|r| r.success);

        let mut findings: Vec<String> = project
            .stage_results
            .iter()
            .filter(|r| !r.success)
            .map(|r| format!("stage {:?} did not succeed: {}", r.stage, r.output))
            .collect();
        if project.stage_results.is_empty() {
            findings.push("no stages have been executed".to_string());
        }
        if !past_review {
            findings
                .push("project has not progressed past the independent review stage".to_string());
        }

        let approved = stages_passed && past_review;
        Ok(ReviewResult {
            approved,
            findings,
            recommendations: if approved {
                vec![]
            } else {
                vec!["complete every lifecycle stage successfully before review".to_string()]
            },
        })
    }

    /// Validate a change.
    pub fn validate_change(&self, project_id: &str) -> Result<bool> {
        let project = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .ok_or_else(|| EvolutionError::NotFound("Project not found".to_string()))?;
        Ok(!project.stage_results.is_empty() && project.stage_results.iter().all(|r| r.success))
    }

    /// Integrate a validated change.
    ///
    /// Prerequisites are checked honestly: the project must exist and its
    /// lifecycle must be [`ProjectStatus::Completed`]. There is currently no
    /// integration actuator in this crate (no skill/tool/source write path is
    /// wired to a development project), so even a completed project cannot be
    /// applied — this always reports that rather than pretending success.
    pub fn integrate_change(&mut self, project_id: &str) -> Result<()> {
        let project = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .ok_or_else(|| EvolutionError::NotFound("Project not found".to_string()))?;

        if project.status != ProjectStatus::Completed {
            return Err(EvolutionError::Validation(format!(
                "cannot integrate project {project_id}: lifecycle status is {:?}, expected Completed",
                project.status
            )));
        }

        Err(EvolutionError::Validation(
            "project completed its lifecycle, but no integration actuator exists in \
             this crate; the change was not applied"
                .to_string(),
        ))
    }

    /// Roll back a change.
    ///
    /// Stage executors in this crate produce no artifacts, so there is never
    /// anything concrete to restore; marking the project
    /// [`ProjectStatus::RolledBack`] *is* the complete rollback. Unknown or
    /// already-rolled-back projects are rejected.
    pub fn rollback_change(&mut self, project_id: &str) -> Result<()> {
        let project = self
            .projects
            .iter_mut()
            .find(|p| p.id == project_id)
            .ok_or_else(|| EvolutionError::NotFound("Project not found".to_string()))?;

        if project.status == ProjectStatus::RolledBack {
            return Err(EvolutionError::Conflict(format!(
                "project {project_id} is already rolled back"
            )));
        }

        project.status = ProjectStatus::RolledBack;
        Ok(())
    }

    /// Get all development projects.
    pub fn get_projects(&self) -> &[DevelopmentProject] {
        &self.projects
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

    fn finish_project(&mut self, project_id: &str) -> Result<StageResult> {
        let project = self
            .projects
            .iter_mut()
            .find(|p| p.id == project_id)
            .ok_or_else(|| EvolutionError::NotFound("Project not found".to_string()))?;

        let all_passed =
            !project.stage_results.is_empty() && project.stage_results.iter().all(|r| r.success);
        project.status = if all_passed {
            ProjectStatus::Completed
        } else {
            ProjectStatus::Failed
        };
        project.completed_ms = Some(now_ms());

        Ok(StageResult {
            stage: DevelopmentStage::RollbackPath,
            success: all_passed,
            output: if all_passed {
                "Development lifecycle completed".to_string()
            } else {
                "Development lifecycle finished with failed stages".to_string()
            },
            timestamp_ms: now_ms(),
        })
    }

    fn execute_stage(
        &self,
        stage: &DevelopmentStage,
        _change: &DevelopmentChange,
    ) -> Result<StageResult> {
        // There is no stage executor in this crate: no formatting, test,
        // review, or integration tooling is wired to a development project.
        // Reporting `success: true` here would fabricate work that never
        // happened, so every stage is recorded as not performed and the
        // project is marked Failed by `advance_stage`.
        Ok(StageResult {
            stage: *stage,
            success: false,
            output: format!(
                "Stage {:?} has no executor in this crate; recorded as not performed",
                stage
            ),
            timestamp_ms: now_ms(),
        })
    }
}

fn next_stage(stage: DevelopmentStage) -> Option<DevelopmentStage> {
    Some(match stage {
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
        DevelopmentStage::RollbackPath => return None,
    })
}

#[derive(Clone, Debug)]
pub struct ReviewResult {
    pub approved: bool,
    pub findings: Vec<String>,
    pub recommendations: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::EvolutionStore;

    fn engine() -> SelfDevelopmentEngine {
        SelfDevelopmentEngine::new(EvolutionStore::open_in_memory().unwrap())
    }

    #[test]
    fn stages_report_not_performed_instead_of_fake_success() {
        let mut engine = engine();
        let project = engine.create_skill("skill", "content", "reason").unwrap();

        let result = engine.advance_stage(&project.id).unwrap();
        assert!(!result.success);
        assert!(result.output.contains("no executor"));

        // A stage that did nothing must fail the project, not pass it.
        let projects = engine.get_projects();
        assert_eq!(projects[0].status, ProjectStatus::Failed);
    }

    #[test]
    fn review_does_not_approve_without_successful_stages() {
        let mut engine = engine();
        let project = engine.create_skill("skill", "content", "reason").unwrap();

        let review = engine.review_change(&project.id).unwrap();
        assert!(!review.approved);
        assert!(review.findings.iter().any(|f| f.contains("no stages")));

        engine.advance_stage(&project.id).unwrap();
        let review = engine.review_change(&project.id).unwrap();
        assert!(!review.approved);
        assert!(
            review
                .findings
                .iter()
                .any(|f| f.contains("did not succeed"))
        );
    }

    #[test]
    fn lifecycle_reaches_a_terminal_status_and_completed_is_honest() {
        let mut engine = engine();
        let project = engine.create_tool("spec", "impl", "reason").unwrap();

        // Drive the project through every stage to the end of the lifecycle.
        for _ in 0..20 {
            engine.advance_stage(&project.id).unwrap();
            if engine.get_projects()[0].completed_ms.is_some() {
                break;
            }
        }

        let finished = engine.get_projects();
        let project = &finished[0];
        assert_eq!(project.status, ProjectStatus::Failed);
        assert!(project.completed_ms.is_some());
        assert!(!project.stage_results.is_empty());
    }

    #[test]
    fn integrate_requires_completed_status_and_reports_missing_actuator() {
        let mut engine = engine();
        let project = engine.create_skill("skill", "content", "reason").unwrap();

        let err = engine.integrate_change(&project.id).unwrap_err();
        assert!(err.to_string().contains("expected Completed"));

        assert!(engine.integrate_change("dev-nope").is_err());
    }

    #[test]
    fn rollback_rejects_unknown_and_repeated_projects() {
        let mut engine = engine();
        let project = engine.create_skill("skill", "content", "reason").unwrap();

        assert!(engine.rollback_change("dev-nope").is_err());
        engine.rollback_change(&project.id).unwrap();
        assert!(engine.rollback_change(&project.id).is_err());
        assert_eq!(engine.get_projects()[0].status, ProjectStatus::RolledBack);
    }

    #[test]
    fn unknown_project_ids_error_instead_of_silent_defaults() {
        let mut engine = engine();
        assert!(engine.review_change("dev-nope").is_err());
        assert!(engine.validate_change("dev-nope").is_err());
        assert!(engine.advance_stage("dev-nope").is_err());
    }
}
