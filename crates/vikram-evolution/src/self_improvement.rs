//! Self-Improvement Engine: Proactively identify and execute improvements.
//!
//! This engine identifies areas where Vikram can improve, prioritizes them
//! based on value and risk, and executes improvements through a controlled lifecycle.
//!
//! [`SelfImprovementEngine::identify_improvement_opportunities`] derives its
//! opportunities from real evidence: the per-`task_category` failure and error
//! tallies in the [`EvolutionStore`] (see [`crate::metrics`]) plus failed
//! reflection jobs. With no such evidence it returns an empty list and records
//! *why* in [`SelfImprovementEngine::last_opportunity_note`] — it never
//! invents "high token usage detected"-style findings.
//!
//! PARTIALLY LIVE: the opportunity scan is wired — the runtime's hourly
//! self-check and `vikram evolution evaluate` both call
//! [`SelfImprovementEngine::identify_improvement_opportunities`]. The same
//! self-check also *acts* on the top opportunity, bounded to one per scan:
//! [`OpportunityAction::AnalysisReport`] opportunities are executed for
//! real through [`SelfImprovementEngine::execute_improvement`], which
//! re-reads the store and records each step's actual output;
//! [`OpportunityAction::RequiresChange`] opportunities are only surfaced
//! as a notice and recorded through
//! [`SelfImprovementEngine::track_improvement`] — an `ImprovementExecution`
//! whose single step says the opportunity was reported, not that an
//! automated change ran. Executions are durable either way:
//! `executions` is persisted to the store's `engine_records` table under
//! [`EXECUTION_NAMESPACE`] and reloaded by [`SelfImprovementEngine::new`].
//! `opportunities` stays process-local: every scan rebuilds it wholesale
//! from store evidence, so persisting it would only duplicate what the
//! next scan recomputes.

use crate::error::Result;
use crate::metrics;
use crate::store::{EvolutionStore, from_json_value, to_json_value};
use crate::types::{ExperienceOutcome, gen_id, now_ms};
use serde::{Deserialize, Serialize};

/// `engine_records` namespace holding every [`ImprovementExecution`].
const EXECUTION_NAMESPACE: &str = "self_improvement.execution";

/// Recency window failure evidence is taken from.
const WINDOW_MS: u64 = metrics::DEFAULT_WINDOW_MS;
/// At most this many per-category opportunities per scan.
const MAX_AREA_OPPORTUNITIES: usize = 5;

/// Improvement opportunity types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImprovementArea {
    PlanningQuality,
    TaskDecomposition,
    CodingPerformance,
    MemoryRetrieval,
    ContextManagement,
    ToolSelection,
    AgentCoordination,
    ErrorHandling,
    Recovery,
    Verification,
    ResearchQuality,
    ExecutionEfficiency,
}

/// What an opportunity asks for. Only [`OpportunityAction::AnalysisReport`]
/// may be auto-executed: it is read-only diagnostic work over recorded
/// evidence. [`OpportunityAction::RequiresChange`] would mutate code,
/// config, or job state, so it is never auto-executed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpportunityAction {
    /// Re-scan the store for `category` and produce a report. The `category`
    /// is the `task_category` the opportunity's evidence came from.
    AnalysisReport { category: String },
    /// The fix needs an actual change; stays pending for a human/agent.
    RequiresChange,
}

/// Improvement opportunity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImprovementOpportunity {
    pub id: String,
    pub area: ImprovementArea,
    pub description: String,
    pub evidence: Vec<String>,
    pub expected_value: f32,
    pub estimated_cost: f32,
    pub risk_level: RiskLevel,
    pub priority: u32,
    pub action: OpportunityAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

/// Improvement execution record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImprovementExecution {
    pub id: String,
    pub opportunity_id: String,
    pub steps: Vec<ImprovementStep>,
    pub status: ExecutionStatus,
    pub started_ms: u64,
    pub completed_ms: Option<u64>,
    pub outcome: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ExecutionStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    RolledBack,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImprovementStep {
    pub description: String,
    pub status: StepStatus,
    pub output: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

pub struct SelfImprovementEngine {
    store: EvolutionStore,
    opportunities: Vec<ImprovementOpportunity>,
    executions: Vec<ImprovementExecution>,
    /// Why the last scan produced what it produced (empty evidence, or a
    /// summary of the evidence it used). `None` before the first scan.
    last_scan_note: Option<String>,
}

impl SelfImprovementEngine {
    pub fn new(store: EvolutionStore) -> Result<Self> {
        let executions = store
            .list_engine_records(EXECUTION_NAMESPACE)?
            .into_iter()
            .map(|(_, payload)| from_json_value::<ImprovementExecution>(payload))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            store,
            opportunities: Vec::new(),
            executions,
            last_scan_note: None,
        })
    }

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Why the last [`Self::identify_improvement_opportunities`] scan returned
    /// what it did — the documented reason when the list is empty.
    pub fn last_opportunity_note(&self) -> Option<&str> {
        self.last_scan_note.as_deref()
    }

    /// Identify improvement opportunities from recorded evidence.
    ///
    /// Evidence is: per-`task_category` failure/error counts over the last
    /// [`WINDOW_MS`], plus failed reflection jobs. Each returned opportunity
    /// quotes the counts it came from in `evidence`, and its
    /// `expected_value`/`priority` are derived from the observed failure share.
    ///
    /// When the store holds no failure evidence — including when it holds no
    /// data at all — the list is empty and
    /// [`Self::last_opportunity_note`] explains why. Opportunities are never
    /// fabricated to fill the list.
    pub fn identify_improvement_opportunities(&mut self) -> Result<Vec<ImprovementOpportunity>> {
        let since = now_ms().saturating_sub(WINDOW_MS);
        let totals = self.store.experience_stats(WINDOW_MS)?;
        let areas = self
            .store
            .category_stats(Some(since), MAX_AREA_OPPORTUNITIES)?;
        let reflection = self.store.reflection_stats()?;

        let mut opportunities = Vec::new();

        for area in areas
            .iter()
            .filter(|area| area.problems > 0)
            .take(MAX_AREA_OPPORTUNITIES)
        {
            let problem_rate = area.problem_rate().unwrap_or(0.0);
            let mut evidence = vec![format!(
                "{} of {} recorded '{}' experience(s) in the last {}h failed or errored",
                area.problems,
                area.total,
                area.category,
                WINDOW_MS / 3_600_000
            )];
            if area.failures > 0 {
                evidence.push(format!("{} failed outright", area.failures));
            }
            if area.errors > 0 {
                evidence.push(format!("{} recorded at least one error", area.errors));
            }
            if area.corrections > 0 {
                evidence.push(format!("{} carried a user correction", area.corrections));
            }
            opportunities.push(ImprovementOpportunity {
                id: gen_id("imp"),
                area: area_for_category(&area.category),
                description: format!("Reduce failures in '{}' work", area.category),
                evidence,
                // Value and priority come from the observed problem share;
                // cost/risk are policy defaults for the chosen area.
                expected_value: 0.1 + 0.9 * problem_rate,
                estimated_cost: 0.3,
                risk_level: RiskLevel::Low,
                priority: (problem_rate * 10.0).round().clamp(1.0, 10.0) as u32,
                action: OpportunityAction::AnalysisReport {
                    category: area.category.clone(),
                },
            });
        }

        if reflection.failed > 0 {
            opportunities.push(ImprovementOpportunity {
                id: gen_id("imp"),
                area: ImprovementArea::Recovery,
                description: "Repair failing reflection jobs".to_string(),
                evidence: vec![format!(
                    "{} of {} finished reflection job(s) failed",
                    reflection.failed,
                    reflection.done + reflection.failed
                )],
                expected_value: 0.1 + 0.9 * reflection.failure_rate().unwrap_or(0.0),
                estimated_cost: 0.4,
                risk_level: RiskLevel::Medium,
                priority: (reflection.failure_rate().unwrap_or(0.0) * 10.0)
                    .round()
                    .clamp(1.0, 10.0) as u32,
                action: OpportunityAction::RequiresChange,
            });
        }

        self.last_scan_note = Some(if opportunities.is_empty() {
            if totals.all_time.total == 0 {
                format!(
                    "no improvement opportunities: the store holds no recorded experiences (need at least {} with failures to derive one)",
                    metrics::MIN_SAMPLE
                )
            } else {
                format!(
                    "no improvement opportunities: none of the {} recorded experience(s) failed or errored in the last {}h",
                    totals.recent.total,
                    WINDOW_MS / 3_600_000
                )
            }
        } else {
            format!(
                "{} improvement opportunit(y/ies) derived from {} failing experience(s) and {} failed reflection job(s) in the last {}h",
                opportunities.len(),
                totals.recent.problems,
                reflection.failed,
                WINDOW_MS / 3_600_000
            )
        });

        self.opportunities = opportunities.clone();
        Ok(opportunities)
    }

    /// Prioritize improvements based on value, evidence, cost, and risk.
    pub fn prioritize_improvements(
        &self,
        opportunities: &[ImprovementOpportunity],
    ) -> Vec<ImprovementOpportunity> {
        let mut prioritized = opportunities.to_vec();
        prioritized.sort_by(|a, b| {
            let score_a = a.expected_value / (a.estimated_cost + 0.1)
                - match a.risk_level {
                    RiskLevel::Low => 0.0,
                    RiskLevel::Medium => 0.2,
                    RiskLevel::High => 0.5,
                };
            let score_b = b.expected_value / (b.estimated_cost + 0.1)
                - match b.risk_level {
                    RiskLevel::Low => 0.0,
                    RiskLevel::Medium => 0.2,
                    RiskLevel::High => 0.5,
                };
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        prioritized
    }

    /// Execute an improvement.
    ///
    /// [`OpportunityAction::AnalysisReport`] opportunities are executed for
    /// real: the engine re-reads the store and each recorded step carries
    /// the output that work actually produced. [`OpportunityAction::RequiresChange`]
    /// opportunities are never auto-executed — the returned execution stays
    /// [`ExecutionStatus::Pending`] with a step saying why.
    pub fn execute_improvement(
        &mut self,
        opportunity: &ImprovementOpportunity,
    ) -> Result<ImprovementExecution> {
        let execution = match &opportunity.action {
            OpportunityAction::AnalysisReport { category } => {
                self.run_analysis_report(opportunity, category)?
            }
            OpportunityAction::RequiresChange => ImprovementExecution {
                id: gen_id("exec"),
                opportunity_id: opportunity.id.clone(),
                steps: vec![ImprovementStep {
                    description: format!(
                        "deferred: '{}' requires an actual change and is not auto-executed",
                        opportunity.description
                    ),
                    status: StepStatus::Pending,
                    output: None,
                }],
                status: ExecutionStatus::Pending,
                started_ms: now_ms(),
                completed_ms: None,
                outcome: Some(
                    "requires-change: recorded as pending; no automated change applied".to_owned(),
                ),
            },
        };

        self.executions.push(execution.clone());
        if let Err(e) = self.persist_execution(&execution) {
            self.executions.pop();
            return Err(e);
        }
        Ok(execution)
    }

    /// Perform the read-only analysis an [`OpportunityAction::AnalysisReport`]
    /// opportunity asks for and record each step's real output.
    fn run_analysis_report(
        &self,
        opportunity: &ImprovementOpportunity,
        category: &str,
    ) -> Result<ImprovementExecution> {
        let started = now_ms();
        let since = started.saturating_sub(WINDOW_MS);
        let mut steps = Vec::new();

        let stats = match self.store.experience_stats_for(Some(category), WINDOW_MS) {
            Ok(stats) => {
                steps.push(ImprovementStep {
                    description: format!("Collect failure evidence for '{category}'"),
                    status: StepStatus::Completed,
                    output: Some(format!(
                        "{} of {} recorded '{category}' experience(s) in the last {}h failed or errored \
                         ({} failed outright, {} with errors, {} with corrections); all-time {}/{}",
                        stats.recent.problems,
                        stats.recent.total,
                        WINDOW_MS / 3_600_000,
                        stats.recent.failure,
                        stats.recent.with_errors,
                        stats.recent.with_corrections,
                        stats.all_time.problems,
                        stats.all_time.total,
                    )),
                });
                stats
            }
            Err(e) => {
                steps.push(ImprovementStep {
                    description: format!("Collect failure evidence for '{category}'"),
                    status: StepStatus::Failed,
                    output: Some(format!("store read failed: {e}")),
                });
                return Ok(Self::analysis_execution(
                    opportunity,
                    steps,
                    ExecutionStatus::Failed,
                    started,
                    Some(format!("evidence collection failed: {e}")),
                ));
            }
        };

        let failures = match self.store.list_failed_experiences_since(since, 200) {
            Ok(experiences) => experiences
                .into_iter()
                .filter(|e| e.task_category == category)
                .collect::<Vec<_>>(),
            Err(e) => {
                steps.push(ImprovementStep {
                    description: "Summarize recorded failure modes".to_string(),
                    status: StepStatus::Failed,
                    output: Some(format!("store read failed: {e}")),
                });
                return Ok(Self::analysis_execution(
                    opportunity,
                    steps,
                    ExecutionStatus::Failed,
                    started,
                    Some(format!("failure enumeration failed: {e}")),
                ));
            }
        };

        let mut error_counts: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        let mut tool_counts: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        let mut outright_failures = 0usize;
        for exp in &failures {
            if exp.outcome == ExperienceOutcome::Failure {
                outright_failures += 1;
            }
            for err in &exp.errors {
                *error_counts.entry(err.as_str()).or_default() += 1;
            }
            for tool in &exp.tools_used {
                *tool_counts.entry(tool.as_str()).or_default() += 1;
            }
        }
        let top = |counts: &std::collections::BTreeMap<&str, usize>| {
            counts
                .iter()
                .take(3)
                .map(|(k, n)| format!("{n}x {k}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        steps.push(ImprovementStep {
            description: "Summarize recorded failure modes".to_string(),
            status: StepStatus::Completed,
            output: Some(if failures.is_empty() {
                format!("no failing '{category}' experience(s) remain in the last {}h window", WINDOW_MS / 3_600_000)
            } else {
                format!(
                    "inspected {} failing '{category}' experience(s) ({} failed outright); tools: [{}]; top errors: [{}]",
                    failures.len(),
                    outright_failures,
                    top(&tool_counts),
                    top(&error_counts),
                )
            }),
        });

        let problem_rate = if stats.recent.total == 0 {
            0.0
        } else {
            stats.recent.problems as f32 / stats.recent.total as f32 * 100.0
        };
        let report = format!(
            "analysis report for '{}': {}/{} recent experience(s) failed or errored \
             (problem rate {:.0}%), {} failing record(s) inspected; the underlying \
             improvement still requires a manual change — nothing was modified",
            category,
            stats.recent.problems,
            stats.recent.total,
            problem_rate,
            failures.len(),
        );
        steps.push(ImprovementStep {
            description: opportunity.description.clone(),
            status: StepStatus::Completed,
            output: Some(report),
        });

        Ok(Self::analysis_execution(
            opportunity,
            steps,
            ExecutionStatus::Completed,
            started,
            Some(format!(
                "analysis report produced for '{category}'; no automated change applied"
            )),
        ))
    }

    fn analysis_execution(
        opportunity: &ImprovementOpportunity,
        steps: Vec<ImprovementStep>,
        status: ExecutionStatus,
        started_ms: u64,
        outcome: Option<String>,
    ) -> ImprovementExecution {
        let finished = status != ExecutionStatus::Pending;
        ImprovementExecution {
            id: gen_id("exec"),
            opportunity_id: opportunity.id.clone(),
            steps,
            status,
            started_ms,
            completed_ms: finished.then(now_ms),
            outcome,
        }
    }

    /// Track improvement history.
    pub fn track_improvement(&mut self, execution: ImprovementExecution) -> Result<()> {
        self.executions.push(execution.clone());
        if let Err(e) = self.persist_execution(&execution) {
            self.executions.pop();
            return Err(e);
        }
        Ok(())
    }

    /// Write one execution record to the store; called at every mutation
    /// boundary so the durable history never lags the in-memory list.
    fn persist_execution(&self, execution: &ImprovementExecution) -> Result<()> {
        self.store.save_engine_record(
            EXECUTION_NAMESPACE,
            &execution.id,
            &to_json_value(execution)?,
        )
    }

    /// Verify that an improvement actually worked.
    pub fn verify_improvement(&self, execution_id: &str) -> Result<bool> {
        if let Some(exec) = self.executions.iter().find(|e| e.id == execution_id) {
            Ok(exec.status == ExecutionStatus::Completed)
        } else {
            Ok(false)
        }
    }

    /// Get improvement history.
    pub fn get_improvement_history(&self) -> &[ImprovementExecution] {
        &self.executions
    }

    /// Get pending opportunities.
    pub fn get_pending_opportunities(&self) -> Vec<ImprovementOpportunity> {
        self.opportunities
            .iter()
            .filter(|op| {
                !self
                    .executions
                    .iter()
                    .any(|exec| exec.opportunity_id == op.id)
            })
            .cloned()
            .collect()
    }
}

/// Map a recorded `task_category` onto an [`ImprovementArea`].
///
/// Categories are producer-defined free-form strings (the runtime records the
/// first tool used, or `conversation`), so this is a documented keyword
/// heuristic; anything unrecognised lands in [`ImprovementArea::ErrorHandling`]
/// rather than being silently dropped.
fn area_for_category(category: &str) -> ImprovementArea {
    let category = category.to_ascii_lowercase();
    let matches = |needles: &[&str]| needles.iter().any(|n| category.contains(n));
    if matches(&["plan"]) {
        ImprovementArea::PlanningQuality
    } else if matches(&["decompos"]) {
        ImprovementArea::TaskDecomposition
    } else if matches(&["cod", "edit", "write", "patch"]) {
        ImprovementArea::CodingPerformance
    } else if matches(&["memory", "recall", "retriev"]) {
        ImprovementArea::MemoryRetrieval
    } else if matches(&["context"]) {
        ImprovementArea::ContextManagement
    } else if matches(&["tool", "shell", "bash", "read", "grep", "glob"]) {
        ImprovementArea::ToolSelection
    } else if matches(&["agent", "hive", "team", "delegat"]) {
        ImprovementArea::AgentCoordination
    } else if matches(&["recover", "retry", "rollback"]) {
        ImprovementArea::Recovery
    } else if matches(&["verif", "test", "check"]) {
        ImprovementArea::Verification
    } else if matches(&["research", "search", "web"]) {
        ImprovementArea::ResearchQuality
    } else if matches(&["conversation", "chat"]) {
        ImprovementArea::ExecutionEfficiency
    } else {
        ImprovementArea::ErrorHandling
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opportunity(id: &str) -> ImprovementOpportunity {
        ImprovementOpportunity {
            id: id.to_string(),
            area: ImprovementArea::ErrorHandling,
            description: "test".to_string(),
            evidence: vec![],
            expected_value: 0.5,
            estimated_cost: 0.3,
            risk_level: RiskLevel::Low,
            priority: 5,
            action: OpportunityAction::RequiresChange,
        }
    }

    fn failing_experience(i: usize, category: &str) -> crate::types::Experience {
        crate::types::Experience {
            id: String::new(),
            timestamp_ms: crate::types::now_ms(),
            agent_id: "main".to_string(),
            project_id: None,
            task_category: category.to_string(),
            user_request: format!("failing request {i}"),
            outcome: ExperienceOutcome::Failure,
            tools_used: vec!["shell".to_string()],
            errors: vec!["boom".to_string()],
            corrections: Vec::new(),
            feedback: None,
            reusable: false,
            sensitivity: crate::types::Sensitivity::Private,
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn executions_survive_a_store_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evolution.db");
        let execution_id;

        {
            let store = EvolutionStore::open(&path).unwrap();
            let mut engine = SelfImprovementEngine::new(store).unwrap();
            let execution = engine.execute_improvement(&opportunity("imp-1")).unwrap();
            execution_id = execution.id.clone();
        }

        let store = EvolutionStore::open(&path).unwrap();
        let engine = SelfImprovementEngine::new(store).unwrap();
        assert_eq!(engine.get_improvement_history().len(), 1);
        assert_eq!(engine.get_improvement_history()[0].id, execution_id);
        assert!(!engine.verify_improvement(&execution_id).unwrap());
    }

    #[test]
    fn a_fresh_store_starts_with_no_executions() {
        let engine = SelfImprovementEngine::new(EvolutionStore::open_in_memory().unwrap()).unwrap();
        assert!(engine.get_improvement_history().is_empty());
    }

    #[test]
    fn analysis_report_executes_with_real_step_outputs() {
        let store = EvolutionStore::open_in_memory().unwrap();
        for i in 0..metrics::MIN_SAMPLE {
            store
                .save_experience(&failing_experience(i, "build"))
                .unwrap();
        }
        let mut engine = SelfImprovementEngine::new(store).unwrap();
        let opportunities = engine.identify_improvement_opportunities().unwrap();
        let top = engine
            .prioritize_improvements(&opportunities)
            .into_iter()
            .next()
            .expect("failing 'build' experiences must produce an opportunity");
        assert_eq!(
            top.action,
            OpportunityAction::AnalysisReport {
                category: "build".to_string()
            }
        );

        let execution = engine.execute_improvement(&top).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Completed);
        assert!(execution.completed_ms.is_some());
        assert_eq!(execution.steps.len(), 3);
        for step in &execution.steps {
            assert_eq!(step.status, StepStatus::Completed);
            let output = step.output.as_deref().unwrap_or_default();
            assert!(
                !output.is_empty(),
                "step '{}' has no real output",
                step.description
            );
        }
        assert!(
            execution.steps[0]
                .output
                .as_deref()
                .unwrap()
                .contains("build"),
            "evidence step must quote the category it analysed"
        );
        assert!(
            execution
                .outcome
                .as_deref()
                .unwrap()
                .contains("no automated change applied")
        );
        assert!(engine.verify_improvement(&execution.id).unwrap());
    }

    #[test]
    fn requires_change_never_auto_executes() {
        let mut engine =
            SelfImprovementEngine::new(EvolutionStore::open_in_memory().unwrap()).unwrap();
        let execution = engine.execute_improvement(&opportunity("imp-rc")).unwrap();
        assert_eq!(execution.status, ExecutionStatus::Pending);
        assert!(execution.completed_ms.is_none());
        assert_eq!(execution.steps.len(), 1);
        assert_eq!(execution.steps[0].status, StepStatus::Pending);
        assert!(
            execution
                .outcome
                .as_deref()
                .unwrap()
                .contains("no automated change applied")
        );
        assert!(!engine.verify_improvement(&execution.id).unwrap());
    }
}
