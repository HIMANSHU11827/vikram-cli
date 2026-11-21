//! Self-Development Engine: Controlled development of new capabilities.
//!
//! This engine handles skill creation, tool development, agent development,
//! and controlled source code modifications through a safe lifecycle.
//!
//! STORE-SCOPED / partially real: nothing in the runtime or CLI calls this
//! engine, but projects are durable — every [`DevelopmentProject`] is
//! persisted to `engine_records` under `self_development.project` on
//! creation and after every stage advance, and rehydrated on the first
//! mutating call.
//!
//! Stage honesty is per-stage: stages that can do real work over the store
//! do it and say so — `Research` reports recorded experience tallies,
//! `Inspection` reports recorded skill versions and memory counts,
//! `Planning` persists a plan artifact under `self_development.plan`,
//! `IndependentReview` evaluates the recorded stage results,
//! `Recording`/`RollbackPath` document the durable record and rollback
//! path. Stages that need tooling this crate does not have (implementation,
//! formatting, test runs, comparison, integration) still record
//! `success: false` — they never fabricate work, so a project cannot
//! reach `Completed` until real executors exist.

use crate::error::{EvolutionError, Result};
use crate::metrics;
use crate::store::{EvolutionStore, from_json_value, to_json_value};
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};

/// `engine_records` namespace holding every persisted [`DevelopmentProject`].
const PROJECT_NAMESPACE: &str = "self_development.project";
/// `engine_records` namespace holding the plan artifact each project
/// emits at its `Planning` stage.
const PLAN_NAMESPACE: &str = "self_development.plan";

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
    /// Whether `projects` has been rehydrated from `engine_records`.
    hydrated: bool,
}

impl SelfDevelopmentEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            projects: Vec::new(),
            hydrated: false,
        }
    }

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Load persisted projects into memory once. Mutating entry points call
    /// this first so post-restart callers see the durable history; a
    /// corrupt payload is an error, not a silently-empty list.
    fn hydrate(&mut self) -> Result<()> {
        if self.hydrated {
            return Ok(());
        }
        let mut projects = Vec::new();
        for (id, payload) in self.store.list_engine_records(PROJECT_NAMESPACE)? {
            let mut project: DevelopmentProject = from_json_value(payload)?;
            if project.id.is_empty() {
                project.id = id;
            }
            projects.push(project);
        }
        projects.sort_by_key(|p| p.created_ms);
        self.projects = projects;
        self.hydrated = true;
        Ok(())
    }

    /// Persist the project record after every mutation, so the durable
    /// copy always reflects the latest stage.
    fn persist_project(&self, project: &DevelopmentProject) -> Result<()> {
        self.store
            .save_engine_record(PROJECT_NAMESPACE, &project.id, &to_json_value(project)?)
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
        self.hydrate()?;
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

        let result = self.execute_stage(&next_stage, &change, project_id)?;

        let snapshot = {
            let project = self
                .projects
                .iter_mut()
                .find(|p| p.id == project_id)
                .ok_or_else(|| EvolutionError::NotFound("Project not found".to_string()))?;
            project.current_stage = next_stage;
            project.stage_results.push(result.clone());

            if !result.success {
                project.status = ProjectStatus::Failed;
            }
            project.clone()
        };
        self.persist_project(&snapshot)?;

        Ok(result)
    }

    /// Review a change independently.
    ///
    /// This is a real check over the recorded stage results, not a rubber
    /// stamp: approval requires that the project has progressed past
    /// [`DevelopmentStage::IndependentReview`] and that every recorded stage
    /// succeeded. Failed stages are reported as findings. Hydrates the
    /// persisted projects first so a fresh engine reviews durable state.
    pub fn review_change(&mut self, project_id: &str) -> Result<ReviewResult> {
        self.hydrate()?;
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
    ///
    /// Hydrates the persisted projects first so a fresh engine validates
    /// durable state.
    pub fn validate_change(&mut self, project_id: &str) -> Result<bool> {
        self.hydrate()?;
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
        self.hydrate()?;
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
    /// No stage produces a deployable artifact (the `Planning` stage
    /// persists a plan record and `Recording` persists the project
    /// record — both audit artifacts, not applied changes), so there is
    /// never anything concrete to restore; marking the project
    /// [`ProjectStatus::RolledBack`] *is* the complete rollback, and the
    /// status is persisted. Unknown or already-rolled-back projects are
    /// rejected.
    pub fn rollback_change(&mut self, project_id: &str) -> Result<()> {
        self.hydrate()?;
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
        let snapshot = project.clone();
        self.persist_project(&snapshot)
    }

    /// Get all development projects known to this engine instance.
    ///
    /// `&self` cannot hydrate, so on a fresh engine this returns only what
    /// earlier mutating calls ([`Self::create_skill`], [`Self::advance_stage`],
    /// …) loaded; use one of those first to see the durable history.
    pub fn get_projects(&self) -> &[DevelopmentProject] {
        &self.projects
    }

    fn start_development(
        &mut self,
        change: DevelopmentChange,
        _reason: &str,
    ) -> Result<DevelopmentProject> {
        self.hydrate()?;
        let project = DevelopmentProject {
            id: gen_id("dev"),
            change: change.clone(),
            current_stage: DevelopmentStage::Identification,
            stage_results: vec![StageResult {
                stage: DevelopmentStage::Identification,
                success: true,
                output: format!(
                    "change identified and recorded: {}",
                    describe_change(&change)
                ),
                timestamp_ms: now_ms(),
            }],
            created_ms: now_ms(),
            completed_ms: None,
            status: ProjectStatus::InProgress,
        };

        self.persist_project(&project)?;
        self.projects.push(project.clone());
        Ok(project)
    }

    fn finish_project(&mut self, project_id: &str) -> Result<StageResult> {
        self.hydrate()?;
        let (snapshot, result) = {
            let project = self
                .projects
                .iter_mut()
                .find(|p| p.id == project_id)
                .ok_or_else(|| EvolutionError::NotFound("Project not found".to_string()))?;

            let all_passed = !project.stage_results.is_empty()
                && project.stage_results.iter().all(|r| r.success);
            project.status = if all_passed {
                ProjectStatus::Completed
            } else {
                ProjectStatus::Failed
            };
            project.completed_ms = Some(now_ms());

            let result = StageResult {
                stage: DevelopmentStage::RollbackPath,
                success: all_passed,
                output: if all_passed {
                    "Development lifecycle completed".to_string()
                } else {
                    "Development lifecycle finished with failed stages".to_string()
                },
                timestamp_ms: now_ms(),
            };
            (project.clone(), result)
        };
        self.persist_project(&snapshot)?;
        Ok(result)
    }

    /// Execute one lifecycle stage.
    ///
    /// Stages fall into three honest groups:
    ///
    /// - **Real work over the store** — `Research` (recorded experience
    ///   tallies), `Inspection` (recorded skill versions + memory counts),
    ///   `Planning` (persists a plan artifact), `IndependentReview`
    ///   (evaluates the recorded stage results), `Recording` (the project
    ///   record is persisted by [`Self::advance_stage`]), and
    ///   `RollbackPath` (the rollback path is `rollback_change`, verified
    ///   against the live project). These succeed only because they
    ///   actually did the work.
    /// - **No executor in this crate** — `Implementation`, `Formatting`,
    ///   `UnitTests`, `IntegrationTests`, `RegressionTests`, `Comparison`,
    ///   `Integration`. There is no formatter, test runner, baseline, or
    ///   integration actuator reachable from the store, so they record
    ///   `success: false` rather than fabricating results.
    fn execute_stage(
        &self,
        stage: &DevelopmentStage,
        change: &DevelopmentChange,
        project_id: &str,
    ) -> Result<StageResult> {
        let result = match stage {
            DevelopmentStage::Research => self.stage_research()?,
            DevelopmentStage::Inspection => self.stage_inspection(change)?,
            DevelopmentStage::Planning => self.stage_planning(project_id, change)?,
            DevelopmentStage::IndependentReview => self.stage_independent_review(project_id)?,
            DevelopmentStage::Recording => StageResult {
                stage: *stage,
                success: true,
                output: format!(
                    "project record persisted under engine_records/{PROJECT_NAMESPACE}/{project_id}"
                ),
                timestamp_ms: now_ms(),
            },
            DevelopmentStage::RollbackPath => StageResult {
                stage: *stage,
                success: true,
                output: format!(
                    "rollback path verified: rollback_change({project_id}) marks the project \
                     RolledBack; no applied artifacts exist to restore"
                ),
                timestamp_ms: now_ms(),
            },
            _ => StageResult {
                stage: *stage,
                success: false,
                output: format!(
                    "Stage {:?} has no executor in this crate; recorded as not performed",
                    stage
                ),
                timestamp_ms: now_ms(),
            },
        };
        Ok(result)
    }

    /// Research: report the recorded experience tallies the change would
    /// be built on. Real aggregates, zeroes on an empty store.
    fn stage_research(&self) -> Result<StageResult> {
        let stats = self.store.experience_stats(metrics::DEFAULT_WINDOW_MS)?;
        Ok(StageResult {
            stage: DevelopmentStage::Research,
            success: true,
            output: format!(
                "research over recorded experience: {} all-time, {} in the last {} ms \
                 ({} failures, {} errors)",
                stats.all_time.total,
                stats.recent.total,
                stats.window_ms,
                stats.recent.failure,
                stats.recent.with_errors
            ),
            timestamp_ms: now_ms(),
        })
    }

    /// Inspection: report what the store already holds for the change's
    /// target — recorded skill versions for skill changes, plus live
    /// memory and skill counts for everything else.
    fn stage_inspection(&self, change: &DevelopmentChange) -> Result<StageResult> {
        let mut findings = Vec::new();
        let skill_name = match change {
            DevelopmentChange::CreateSkill { name, .. }
            | DevelopmentChange::ImproveSkill { name, .. } => Some(name.as_str()),
            _ => None,
        };
        if let Some(name) = skill_name {
            let versions = self.store.list_skill_versions(name)?;
            findings.push(format!(
                "skill '{name}': {} recorded version(s)",
                versions.len()
            ));
            if let Some(latest) = versions.last() {
                findings.push(format!(
                    "latest recorded v{} (parent {:?})",
                    latest.version, latest.parent_version
                ));
            }
        }
        findings.push(format!(
            "store holds {} skill(s) with recorded versions, {} live memory entries",
            self.store.latest_skill_versions()?.len(),
            self.store.count_memory()?
        ));
        Ok(StageResult {
            stage: DevelopmentStage::Inspection,
            success: true,
            output: findings.join("; "),
            timestamp_ms: now_ms(),
        })
    }

    /// Planning: persist the plan as a real `engine_records` artifact —
    /// the change payload plus the remaining lifecycle steps — under
    /// `self_development.plan/<project_id>`.
    fn stage_planning(&self, project_id: &str, change: &DevelopmentChange) -> Result<StageResult> {
        let plan = serde_json::json!({
            "project_id": project_id,
            "change": to_json_value(change)?,
            "remaining_stages": [
                "implementation", "formatting", "unit_tests", "integration_tests",
                "regression_tests", "independent_review", "comparison",
                "integration", "recording", "rollback_path"
            ],
            "note": "plan artifact only — stages without an executor in this crate cannot run",
        });
        self.store
            .save_engine_record(PLAN_NAMESPACE, project_id, &plan)?;
        Ok(StageResult {
            stage: DevelopmentStage::Planning,
            success: true,
            output: format!("plan persisted to engine_records/{PLAN_NAMESPACE}/{project_id}"),
            timestamp_ms: now_ms(),
        })
    }

    /// Independent review: a real check over the stage results recorded
    /// so far — approval requires every prior stage to have succeeded.
    /// Prior failures are reported, not hidden.
    fn stage_independent_review(&self, project_id: &str) -> Result<StageResult> {
        let project = self
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .ok_or_else(|| EvolutionError::NotFound("Project not found".to_string()))?;
        let failed = project.stage_results.iter().filter(|r| !r.success).count();
        let total = project.stage_results.len();
        let success = failed == 0 && total > 0;
        Ok(StageResult {
            stage: DevelopmentStage::IndependentReview,
            success,
            output: if success {
                format!("independent review: all {total} recorded stage(s) succeeded")
            } else {
                format!(
                    "independent review: {failed} of {total} recorded stage(s) did not \
                     succeed; change not approved"
                )
            },
            timestamp_ms: now_ms(),
        })
    }
}

fn describe_change(change: &DevelopmentChange) -> String {
    match change {
        DevelopmentChange::CreateSkill { name, .. } => format!("create skill '{name}'"),
        DevelopmentChange::ImproveSkill { name, .. } => format!("improve skill '{name}'"),
        DevelopmentChange::CreateTool { spec, .. } => {
            format!("create tool: {}", truncate(spec, 60))
        }
        DevelopmentChange::DevelopAgent { spec } => {
            format!("develop agent: {}", truncate(spec, 60))
        }
        DevelopmentChange::ModifySourceCode { changeset } => {
            format!("modify source: {}", truncate(changeset, 60))
        }
        DevelopmentChange::CreateWorkflow { definition } => {
            format!("create workflow: {}", truncate(definition, 60))
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
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
    use crate::types::{Experience, ExperienceOutcome, Sensitivity, SkillVersion};

    fn engine() -> SelfDevelopmentEngine {
        SelfDevelopmentEngine::new(EvolutionStore::open_in_memory().unwrap())
    }

    fn engine_with_store(store: EvolutionStore) -> SelfDevelopmentEngine {
        SelfDevelopmentEngine::new(store)
    }

    #[test]
    fn store_backed_stages_do_real_work_and_tooling_stages_fail_honestly() {
        let mut engine = engine();
        let project = engine.create_skill("skill", "content", "reason").unwrap();

        // Identification was recorded at creation.
        assert_eq!(project.stage_results.len(), 1);
        assert_eq!(
            project.stage_results[0].stage,
            DevelopmentStage::Identification
        );
        assert!(project.stage_results[0].success);

        // Research reports real (empty-store) tallies and succeeds.
        let research = engine.advance_stage(&project.id).unwrap();
        assert!(research.success);
        assert!(
            research
                .output
                .contains("research over recorded experience")
        );

        // Inspection reports real store contents for the skill target.
        let inspection = engine.advance_stage(&project.id).unwrap();
        assert!(inspection.success);
        assert!(inspection.output.contains("skill 'skill'"));

        // Planning persists a real plan artifact.
        let planning = engine.advance_stage(&project.id).unwrap();
        assert!(planning.success);
        assert!(planning.output.contains(PLAN_NAMESPACE));

        // Implementation has no executor — it must fail, not pretend.
        let implementation = engine.advance_stage(&project.id).unwrap();
        assert!(!implementation.success);
        assert!(implementation.output.contains("no executor"));
        assert_eq!(engine.get_projects()[0].status, ProjectStatus::Failed);
    }

    #[test]
    fn planning_stage_leaves_a_persisted_plan_artifact() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let mut engine = engine_with_store(store.clone());
        let project = engine.create_skill("demo", "body", "reason").unwrap();
        engine.advance_stage(&project.id).unwrap(); // research
        engine.advance_stage(&project.id).unwrap(); // inspection
        engine.advance_stage(&project.id).unwrap(); // planning

        let records = store.list_engine_records(PLAN_NAMESPACE).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, project.id);
        let plan = records[0].1.clone();
        assert_eq!(plan["project_id"], project.id);
        assert!(plan["change"]["CreateSkill"]["name"] == "demo");
    }

    #[test]
    fn projects_and_stage_results_survive_a_new_engine() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let project_id;
        {
            let mut engine = engine_with_store(store.clone());
            let project = engine.create_skill("skill", "content", "reason").unwrap();
            project_id = project.id.clone();
            engine.advance_stage(&project.id).unwrap();
            engine.advance_stage(&project.id).unwrap();
            engine.advance_stage(&project.id).unwrap();
        }
        // A fresh engine rehydrates the durable project on first mutation.
        let mut engine = engine_with_store(store);
        let review = engine.review_change(&project_id).unwrap();
        assert!(!review.approved);
        let projects = engine.get_projects();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, project_id);
        // Identification + Research + Inspection + Planning recorded.
        assert_eq!(projects[0].stage_results.len(), 4);
    }

    #[test]
    fn inspection_reports_recorded_skill_versions() {
        let store = EvolutionStore::open_in_memory().unwrap();
        store
            .save_skill_version(&SkillVersion {
                skill_name: "skill".to_string(),
                version: 2,
                content: "v2".to_string(),
                created_ms: now_ms(),
                parent_version: Some(1),
                change_description: "iter".to_string(),
            })
            .unwrap();
        store
            .save_experience(&Experience {
                id: "exp-1".to_string(),
                timestamp_ms: now_ms(),
                agent_id: "a".to_string(),
                project_id: None,
                task_category: "coding".to_string(),
                user_request: "req".to_string(),
                outcome: ExperienceOutcome::Success,
                tools_used: vec![],
                errors: vec![],
                corrections: vec![],
                feedback: None,
                reusable: false,
                sensitivity: Sensitivity::Private,
                metadata: serde_json::json!({}),
            })
            .unwrap();

        let mut engine = engine_with_store(store);
        let project = engine.create_skill("skill", "content", "reason").unwrap();
        engine.advance_stage(&project.id).unwrap(); // research
        let inspection = engine.advance_stage(&project.id).unwrap(); // inspection

        assert!(inspection.output.contains("1 recorded version"));
        assert!(inspection.output.contains("latest recorded v2"));
        assert!(inspection.output.contains("0 live memory entries"));
    }

    #[test]
    fn review_does_not_approve_without_successful_stages() {
        let mut engine = engine();
        let project = engine.create_skill("skill", "content", "reason").unwrap();

        let review = engine.review_change(&project.id).unwrap();
        assert!(!review.approved);
        // Identification succeeded at creation, but the project has not
        // progressed past the independent-review stage.
        assert!(
            review
                .findings
                .iter()
                .any(|f| f.contains("not progressed past"))
        );

        // Drive past the stages that can succeed; Implementation fails.
        for _ in 0..6 {
            engine.advance_stage(&project.id).unwrap();
        }
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
