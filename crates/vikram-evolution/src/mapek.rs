//! MAPE-K Loop: Monitor-Analyze-Plan-Execute over shared Knowledge.
//!
//! This engine orchestrates the complete autonomous management loop following
//! the MAPE-K autonomic computing architecture.
//!
//! # Monitoring is measured, not simulated
//!
//! Every value [`MapekLoop::monitor`] returns is derived from the shared
//! [`EvolutionStore`] (via [`crate::metrics`]) or from a real host probe:
//!
//! - runtime health is folded from recorded experience outcomes, reflection
//!   job states, proposal states and live memory counts, and *is allowed to be*
//!   `Degraded`/`Unhealthy`/`Unknown` — there is no hardcoded `Healthy`;
//! - resource metrics mix real host readings (`/proc` load and memory) with the
//!   store's own on-disk footprint, and say in `notes` which value came from
//!   where (and which is unavailable on this platform);
//! - provider status is the failure rate and mean duration of recorded
//!   experiences (`sample_count` says how many), and reports
//!   `available: false` when nothing has been recorded to judge by;
//! - execution outcomes and failures are the actual stored experiences and
//!   failed reflection jobs inside the recency window.
//!
//! When the store holds no evidence the loop reports `Unknown`/unavailable with
//! an explanation instead of inventing numbers, and `Healthy` is only claimed
//! once at least [`metrics::MIN_SAMPLE`] experiences back it.
//!
//! # Execution is bounded by what the loop can actually do
//!
//! [`MapekLoop`] owns one handle: the shared [`EvolutionStore`]. It has no
//! broker, provider client, scheduler, agent registry or runtime-configuration
//! handle, so it cannot restart a provider, switch a model, edit a skill or
//! change a runtime setting. [`MapekLoop::execute_action`] therefore reports
//! three distinct outcomes ([`ActionOutcome`]) instead of one optimistic one:
//!
//! | outcome | what happened | `success` |
//! |---|---|---|
//! | [`ActionOutcome::Remediated`] | a change was written through the store and read back (re-queueing failed reflection jobs) | `true` |
//! | [`ActionOutcome::RecordedForReview`] | a pending `record_issue` proposal was persisted for an operator; nothing was remediated | `false` |
//! | [`ActionOutcome::NotExecuted`] | nothing was done and nothing recorded; `error` says why | `false` |
//!
//! [`ExecutionResult::overall_success`] is `true` only for a non-empty plan in
//! which every action was remediated, so a cycle never reports "executed" for
//! work that was only written down. See [`ExecutionResult::failure_reason`].

use crate::error::Result;
use crate::metrics;
use crate::store::EvolutionStore;
use crate::types::{
    ExperienceOutcome, LearningEvent, LearningProposal, ProposalKind, ProposalStatus,
    ReflectionJobStatus, gen_id, now_ms,
};
use serde::{Deserialize, Serialize};

/// Recency window used for every "recent" judgement in this module.
const WINDOW_MS: u64 = metrics::DEFAULT_WINDOW_MS;
/// Failure share at or above which the experience pipeline is `Unhealthy`.
const UNHEALTHY_FAILURE_RATE: f32 = 0.5;
/// Failure share at or above which the experience pipeline is `Degraded`.
const DEGRADED_FAILURE_RATE: f32 = 0.25;
/// Error share at or above which the experience pipeline is `Degraded`.
const DEGRADED_ERROR_RATE: f32 = 0.5;
/// Pending `record_issue` proposals at or above which issue tracking is
/// `Unhealthy` (below it, any such proposal is `Degraded`).
const UNRESOLVED_ISSUES_UNHEALTHY: usize = 3;
/// Proposal backlog (pending) at or above which learning is `Degraded`.
const PROPOSAL_BACKLOG_DEGRADED: usize = 20;
/// Reflection backlog (pending + running) at or above which it is `Degraded`.
const REFLECTION_BACKLOG_DEGRADED: usize = 20;
/// Rejection share of decided proposals at or above which learning is
/// `Degraded` (only judged with at least [`metrics::MIN_SAMPLE`] proposals).
const REJECTION_RATE_DEGRADED: f32 = 0.5;
/// Most execution outcomes / failures reported per monitoring pass.
const MAX_OUTCOMES: usize = 100;
const MAX_FAILURES: usize = 50;
/// Disk budget the store footprint is expressed against.
const STORE_DISK_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
/// Action target naming the reflection subsystem — the one component the loop
/// has a real actuator for (see [`MapekLoop::execute_action`]).
const REFLECTION_TARGET: &str = "reflection";
/// How many times a reflection job may fail before the loop stops re-queuing
/// it and hands it to an operator instead.
const MAX_REFLECTION_ATTEMPTS: u32 = 3;
/// `agent_id` the loop stamps on the records it persists.
const MAPEK_AGENT: &str = "mapek";

/// Monitoring data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MonitoringData {
    pub id: String,
    pub timestamp_ms: u64,
    pub runtime_health: RuntimeHealth,
    pub resource_metrics: ResourceMetrics,
    pub provider_status: ProviderStatus,
    pub execution_outcomes: Vec<ExecutionOutcome>,
    pub failures: Vec<FailureRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeHealth {
    pub overall_status: HealthStatus,
    pub subsystems: Vec<SubsystemHealth>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthStatus {
    /// Not enough recorded activity to judge. Distinct from `Healthy`: an
    /// empty store is *unverified*, not well.
    Unknown,
    Healthy,
    Degraded,
    Unhealthy,
    Critical,
}

impl HealthStatus {
    /// Rank used to fold subsystem statuses into one overall status.
    /// `Unknown` ranks below `Healthy` so it only wins when *every* subsystem
    /// is unverified.
    pub fn severity_rank(&self) -> u8 {
        match self {
            HealthStatus::Unknown => 0,
            HealthStatus::Healthy => 1,
            HealthStatus::Degraded => 2,
            HealthStatus::Unhealthy => 3,
            HealthStatus::Critical => 4,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubsystemHealth {
    pub name: String,
    pub status: HealthStatus,
    pub details: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceMetrics {
    /// Host CPU pressure: 1-minute load average divided by online CPUs,
    /// clamped to `0.0..=1.0`. `0.0` when the platform cannot be read (see
    /// `notes`).
    pub cpu_usage: f32,
    /// Fraction of host memory in use (`MemTotal - MemAvailable`). `0.0` when
    /// the platform cannot be read (see `notes`).
    pub memory_usage: f32,
    /// Evolution store footprint as a fraction of [`STORE_DISK_BUDGET_BYTES`].
    pub disk_usage: f32,
    /// Whether network activity was observed. Never true: the evolution store
    /// holds no network telemetry, so this is reported as `false` with a note
    /// rather than claimed.
    pub network_active: bool,
    /// Provenance for the numbers above: which probe produced each value and
    /// which values are unavailable.
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderStatus {
    /// `false` when there is no sample to judge availability by, or when the
    /// recorded failure rate is at/above [`UNHEALTHY_FAILURE_RATE`].
    pub available: bool,
    /// Mean recorded experience duration (else mean reflection-job latency);
    /// `0` when no duration was ever recorded — see `notes`.
    pub latency_ms: u64,
    /// Share of the sampled experiences that failed.
    pub error_rate: f32,
    /// How many experiences the rate was computed from.
    #[serde(default)]
    pub sample_count: usize,
    /// Provenance for the numbers above.
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionOutcome {
    pub task_id: String,
    pub success: bool,
    pub duration_ms: u64,
    pub tokens_used: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailureRecord {
    pub id: String,
    pub component: String,
    pub error_type: String,
    pub message: String,
    pub timestamp_ms: u64,
}

/// Analysis result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub id: String,
    pub issues: Vec<Issue>,
    pub opportunities: Vec<Opportunity>,
    pub anomalies: Vec<Anomaly>,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Issue {
    pub severity: IssueSeverity,
    pub component: String,
    pub description: String,
    pub impact: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueSeverity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Opportunity {
    pub area: String,
    pub description: String,
    pub expected_benefit: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Anomaly {
    pub metric: String,
    pub expected: serde_json::Value,
    pub actual: serde_json::Value,
    pub deviation: f32,
}

/// Action plan.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionPlan {
    pub id: String,
    pub actions: Vec<PlannedAction>,
    pub estimated_benefit: String,
    pub estimated_cost: String,
    pub risk_level: RiskLevel,
    pub success_criteria: Vec<String>,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedAction {
    pub id: String,
    pub action_type: ActionType,
    pub target: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ActionType {
    ConfigurationChange,
    Adaptation,
    Improvement,
    Development,
    Recovery,
}

impl ActionType {
    /// Stable `snake_case` name, used in persisted records and in the
    /// execution report.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ConfigurationChange => "configuration_change",
            Self::Adaptation => "adaptation",
            Self::Improvement => "improvement",
            Self::Development => "development",
            Self::Recovery => "recovery",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

/// Execution result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub plan_id: String,
    pub action_results: Vec<ActionResult>,
    /// `true` only when the plan held at least one action and *every* action
    /// ended as [`ActionOutcome::Remediated`]. An action that was merely
    /// recorded for operator review is not a remediation, so it keeps this
    /// `false`.
    pub overall_success: bool,
    pub timestamp_ms: u64,
    /// How many actions the plan held.
    #[serde(default)]
    pub planned_actions: usize,
    /// How many actions a real change was applied for.
    #[serde(default)]
    pub remediated_actions: usize,
    /// How many actions could only be recorded for operator review.
    #[serde(default)]
    pub recorded_for_review: usize,
}

impl ExecutionResult {
    /// One-line account of why this execution is not a success, or `None`
    /// when every planned action was remediated.
    ///
    /// A plan that could not be remediated never reports a silent failure:
    /// this string names how many actions were not remediated, why the first
    /// one failed, and how many were handed to an operator.
    pub fn failure_reason(&self) -> Option<String> {
        if self.planned_actions == 0 {
            return Some("plan held no actions: nothing was executed".to_string());
        }
        if self.overall_success {
            return None;
        }
        let not_remediated = self.planned_actions.saturating_sub(self.remediated_actions);
        let first_error = self
            .action_results
            .iter()
            .find(|r| !r.success)
            .map(|r| {
                r.error.clone().unwrap_or_else(|| {
                    format!("action {} failed without a recorded reason", r.action_id)
                })
            })
            .unwrap_or_else(|| "no action result was recorded".to_string());
        Some(format!(
            "{not_remediated} of {} action(s) were not remediated ({} recorded for operator review): {first_error}",
            self.planned_actions, self.recorded_for_review
        ))
    }
}

/// What an [`ActionResult`] actually achieved.
///
/// The distinction exists so a plan report cannot conflate "I fixed it" with
/// "I wrote down that it needs fixing": only [`ActionOutcome::Remediated`]
/// ever carries `success: true`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    /// A real change was written through the engine's store and verified by
    /// reading it back. The only outcome that reports `success: true`.
    Remediated,
    /// The loop could not perform the action; it persisted a pending
    /// `record_issue` proposal so an operator sees it. Nothing about the
    /// system's behaviour changed, so `success` is `false`.
    RecordedForReview,
    /// Nothing was done and nothing was recorded. `success` is `false` and
    /// `error` says why.
    #[default]
    NotExecuted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionResult {
    /// Id of the [`PlannedAction`] this result belongs to.
    pub action_id: String,
    /// `true` only for [`ActionOutcome::Remediated`].
    pub success: bool,
    /// What was actually done, in words an operator can check.
    pub output: String,
    /// Why the action was not remediated, when it was not.
    pub error: Option<String>,
    /// What the action actually achieved; see [`ActionOutcome`].
    ///
    /// Defaults to [`ActionOutcome::NotExecuted`] for records written before
    /// this field existed, so old data claims nothing.
    #[serde(default)]
    pub outcome: ActionOutcome,
    /// The pending `record_issue` proposal persisted for operator review, when
    /// `outcome == ActionOutcome::RecordedForReview`.
    #[serde(default)]
    pub review_proposal_id: Option<String>,
}

/// Knowledge base.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnowledgeBase {
    pub decisions: Vec<DecisionRecord>,
    pub outcomes: Vec<OutcomeRecord>,
    pub self_model: serde_json::Value,
    pub learning_history: Vec<LearningRecord>,
    pub adaptation_strategies: Vec<AdaptationStrategy>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: String,
    pub context: String,
    pub decision: String,
    pub rationale: String,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutcomeRecord {
    pub id: String,
    pub action_id: String,
    pub outcome: String,
    pub effectiveness: f32,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningRecord {
    pub id: String,
    pub lesson: String,
    pub context: String,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdaptationStrategy {
    pub name: String,
    pub description: String,
    pub effectiveness: f32,
}

pub struct MapekLoop {
    store: EvolutionStore,
    knowledge: KnowledgeBase,
    cycle_count: u32,
}

impl MapekLoop {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            knowledge: KnowledgeBase {
                decisions: Vec::new(),
                outcomes: Vec::new(),
                self_model: serde_json::json!({}),
                learning_history: Vec::new(),
                adaptation_strategies: Vec::new(),
            },
            cycle_count: 0,
        }
    }

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Monitor: Collect data about runtime health and status.
    pub fn monitor(&mut self) -> Result<MonitoringData> {
        let data = MonitoringData {
            id: gen_id("monitor"),
            timestamp_ms: now_ms(),
            runtime_health: self.collect_runtime_health()?,
            resource_metrics: self.collect_resource_metrics()?,
            provider_status: self.collect_provider_status()?,
            execution_outcomes: self.collect_execution_outcomes()?,
            failures: self.collect_failures()?,
        };

        Ok(data)
    }

    /// Analyze: Understand what is happening and identify issues.
    pub fn analyze(&mut self, data: &MonitoringData) -> Result<AnalysisResult> {
        let mut issues = Vec::new();
        let opportunities = Vec::new();
        let anomalies = Vec::new();

        match data.runtime_health.overall_status {
            HealthStatus::Healthy => {}
            HealthStatus::Unknown => issues.push(Issue {
                severity: IssueSeverity::Low,
                component: "runtime".to_string(),
                description: "Runtime health unverified: too little recorded activity".to_string(),
                impact: "No conclusion can be drawn from the store yet".to_string(),
            }),
            HealthStatus::Degraded => issues.push(Issue {
                severity: IssueSeverity::Medium,
                component: "runtime".to_string(),
                description: "Runtime health degraded".to_string(),
                impact: "May affect all operations".to_string(),
            }),
            HealthStatus::Unhealthy | HealthStatus::Critical => issues.push(Issue {
                severity: IssueSeverity::High,
                component: "runtime".to_string(),
                description: "Runtime health degraded".to_string(),
                impact: "May affect all operations".to_string(),
            }),
        }

        if data.provider_status.error_rate > 0.1 && data.provider_status.sample_count > 0 {
            issues.push(Issue {
                severity: IssueSeverity::Medium,
                component: "provider".to_string(),
                description: format!(
                    "Provider error rate elevated: {:.0}% of {} sampled experience(s) failed",
                    data.provider_status.error_rate * 100.0,
                    data.provider_status.sample_count
                ),
                impact: "Degraded reliability".to_string(),
            });
        }

        // Failed reflection jobs are reported as their own issue because they
        // are the one failure class the loop can actually act on (see
        // `execute_action`): the job row can be re-queued in the store.
        let reflection_failures = data
            .failures
            .iter()
            .filter(|f| f.component == REFLECTION_TARGET)
            .count();
        if reflection_failures > 0 {
            issues.push(Issue {
                severity: if reflection_failures >= UNRESOLVED_ISSUES_UNHEALTHY {
                    IssueSeverity::High
                } else {
                    IssueSeverity::Medium
                },
                component: REFLECTION_TARGET.to_string(),
                description: format!(
                    "{reflection_failures} reflection job(s) failed and can be retried"
                ),
                impact: "The experiences behind those jobs were never reflected on".to_string(),
            });
        }

        let pipeline_failures = data.failures.len().saturating_sub(reflection_failures);
        if pipeline_failures >= metrics::MIN_SAMPLE {
            issues.push(Issue {
                severity: IssueSeverity::Medium,
                component: "experience_pipeline".to_string(),
                description: format!(
                    "{pipeline_failures} failing experience(s) recorded in the last {}h",
                    WINDOW_MS / 3_600_000
                ),
                impact: "Repeated failures in recorded work".to_string(),
            });
        }

        let result = AnalysisResult {
            id: gen_id("analysis"),
            issues,
            opportunities,
            anomalies,
            timestamp_ms: now_ms(),
        };

        Ok(result)
    }

    /// Plan: Generate action plans to address issues.
    ///
    /// The action *type* is chosen from what the loop can actually do, not
    /// from severity alone: an issue on [`REFLECTION_TARGET`] becomes a
    /// [`ActionType::Recovery`] (the loop can re-queue the failed job in the
    /// store), every other component keeps the severity mapping. Types the
    /// loop has no actuator for are still planned — they are the ones
    /// [`MapekLoop::execute_action`] escalates to an operator instead of
    /// pretending to perform them.
    pub fn plan(&mut self, analysis: &AnalysisResult) -> Result<ActionPlan> {
        let mut actions = Vec::new();

        for issue in &analysis.issues {
            let (action_type, description) = if issue.component == REFLECTION_TARGET
                && matches!(
                    issue.severity,
                    IssueSeverity::Critical | IssueSeverity::High | IssueSeverity::Medium
                ) {
                (
                    ActionType::Recovery,
                    format!("Re-queue failed reflection job(s): {}", issue.description),
                )
            } else {
                match issue.severity {
                    IssueSeverity::Critical | IssueSeverity::High => (
                        ActionType::Recovery,
                        format!("Address critical issue: {}", issue.description),
                    ),
                    IssueSeverity::Medium => (
                        ActionType::Adaptation,
                        format!("Adapt to issue: {}", issue.description),
                    ),
                    IssueSeverity::Low => (
                        ActionType::Improvement,
                        format!("Improve: {}", issue.description),
                    ),
                }
            };
            actions.push(PlannedAction {
                id: gen_id("action"),
                action_type,
                target: issue.component.clone(),
                description,
                parameters: serde_json::json!({}),
            });
        }

        let plan = ActionPlan {
            id: gen_id("plan"),
            actions,
            estimated_benefit: "Improved system health".to_string(),
            estimated_cost: "Moderate".to_string(),
            risk_level: RiskLevel::Medium,
            success_criteria: vec!["All issues resolved".to_string()],
            timestamp_ms: now_ms(),
        };

        Ok(plan)
    }

    /// Execute: carry out an approved action plan.
    ///
    /// Every action goes through [`Self::execute_action`], which reports what
    /// it actually achieved. [`ExecutionResult::overall_success`] is `true`
    /// only for a non-empty plan in which *every* action ended as
    /// [`ActionOutcome::Remediated`]; an action that could only be recorded
    /// for operator review keeps it `false`, and
    /// [`ExecutionResult::failure_reason`] says so in words.
    pub fn execute(&mut self, plan: &ActionPlan) -> Result<ExecutionResult> {
        let mut action_results = Vec::with_capacity(plan.actions.len());

        for action in &plan.actions {
            action_results.push(self.execute_action(action)?);
        }

        let remediated_actions = action_results
            .iter()
            .filter(|r| r.outcome == ActionOutcome::Remediated)
            .count();
        let recorded_for_review = action_results
            .iter()
            .filter(|r| r.outcome == ActionOutcome::RecordedForReview)
            .count();
        let overall_success =
            !action_results.is_empty() && remediated_actions == action_results.len();

        Ok(ExecutionResult {
            plan_id: plan.id.clone(),
            planned_actions: action_results.len(),
            remediated_actions,
            recorded_for_review,
            action_results,
            overall_success,
            timestamp_ms: now_ms(),
        })
    }

    /// Update knowledge: Store decisions and the *remediation* outcome.
    ///
    /// `effectiveness` is `1.0` only when the plan was actually remediated
    /// ([`ExecutionResult::overall_success`]); a plan that was merely recorded
    /// for an operator is stored as a failure with `0.0`, so the knowledge
    /// base cannot later "remember" work that never happened.
    pub fn update_knowledge(&mut self, plan: &ActionPlan, execution: &ExecutionResult) {
        for action in &plan.actions {
            self.knowledge.decisions.push(DecisionRecord {
                id: gen_id("decision"),
                context: format!("{:?}", action.action_type),
                decision: action.description.clone(),
                rationale: plan.estimated_benefit.clone(),
                timestamp_ms: now_ms(),
            });
        }

        self.knowledge.outcomes.push(OutcomeRecord {
            id: gen_id("outcome"),
            action_id: plan.id.clone(),
            outcome: if execution.overall_success {
                "success".to_string()
            } else {
                "failure".to_string()
            },
            effectiveness: if execution.overall_success { 1.0 } else { 0.0 },
            timestamp_ms: now_ms(),
        });
    }

    /// Run a complete MAPE-K cycle.
    ///
    /// The returned [`CycleResult`] carries the honest execution verdict:
    /// `execution_success` is `true` only when the cycle's plan was actually
    /// remediated, and `failure_reason` explains what was not (see
    /// [`ExecutionResult::failure_reason`]). An unremediated cycle also leaves
    /// a `mapek_execution_incomplete` event in the store, so the failure is
    /// still findable after the report has been dropped.
    pub fn run_cycle(&mut self) -> Result<CycleResult> {
        self.cycle_count += 1;

        let monitoring_data = self.monitor()?;
        let analysis = self.analyze(&monitoring_data)?;
        let plan = self.plan(&analysis)?;
        let execution = self.execute(&plan)?;
        self.update_knowledge(&plan, &execution);

        let failure_reason = execution.failure_reason();
        if let Some(reason) = &failure_reason {
            self.store.save_event(&LearningEvent {
                id: gen_id("evt"),
                timestamp_ms: now_ms(),
                agent_id: MAPEK_AGENT.to_string(),
                kind: "mapek_execution_incomplete".to_string(),
                summary: format!(
                    "MAPE-K cycle {} did not remediate its plan: {reason}",
                    self.cycle_count
                ),
                payload: serde_json::json!({
                    "cycle_number": self.cycle_count,
                    "plan_id": plan.id,
                    "planned_actions": execution.planned_actions,
                    "remediated_actions": execution.remediated_actions,
                    "recorded_for_review": execution.recorded_for_review,
                    "failure_reason": reason,
                }),
            })?;
        }

        Ok(CycleResult {
            cycle_number: self.cycle_count,
            monitoring_data_id: monitoring_data.id,
            analysis_id: analysis.id,
            plan_id: plan.id,
            execution_success: execution.overall_success,
            planned_actions: execution.planned_actions,
            remediated_actions: execution.remediated_actions,
            recorded_for_review: execution.recorded_for_review,
            failure_reason,
            timestamp_ms: now_ms(),
        })
    }

    /// Monitor: runtime health, folded from recorded outcomes, reflection job
    /// states, proposal states and live memory counts.
    ///
    /// A store with no (or too little) recorded activity yields
    /// [`HealthStatus::Unknown`], never a fabricated `Healthy`.
    fn collect_runtime_health(&self) -> Result<RuntimeHealth> {
        let experiences = self.store.experience_stats(WINDOW_MS)?;
        let proposals = self.store.proposal_stats()?;
        let reflection = self.store.reflection_stats()?;
        let live_memory = self.store.count_memory()?;

        let mut subsystems = Vec::with_capacity(4);

        // 1. Experience pipeline: recorded outcomes.
        let (status, details) = match experiences.authoritative_sample() {
            None => (
                HealthStatus::Unknown,
                format!(
                    "{} experience(s) recorded ({} in the last {}h): no sample of {} to rate",
                    experiences.all_time.total,
                    experiences.recent.total,
                    WINDOW_MS / 3_600_000,
                    metrics::MIN_SAMPLE
                ),
            ),
            Some(sample) => {
                let failure = sample.failure_rate().unwrap_or(0.0);
                let errors = sample.error_rate().unwrap_or(0.0);
                let status = if failure >= UNHEALTHY_FAILURE_RATE {
                    HealthStatus::Unhealthy
                } else if failure >= DEGRADED_FAILURE_RATE || errors >= DEGRADED_ERROR_RATE {
                    HealthStatus::Degraded
                } else {
                    HealthStatus::Healthy
                };
                (
                    status,
                    format!(
                        "{} experience(s) sampled: failure {:.0}%, errors {:.0}%, {} succeeded",
                        sample.total,
                        failure * 100.0,
                        errors * 100.0,
                        sample.success
                    ),
                )
            }
        };
        subsystems.push(SubsystemHealth {
            name: "experience_pipeline".to_string(),
            status,
            details,
        });

        // 2. Reflection: job outcomes and backlog.
        let (status, details) = if reflection.total == 0 {
            (
                HealthStatus::Unknown,
                "no reflection jobs recorded".to_string(),
            )
        } else if reflection.failed > 0
            && (reflection.failure_rate().unwrap_or(0.0) >= 0.5 || reflection.failed >= 3)
        {
            (
                HealthStatus::Unhealthy,
                format!(
                    "{} of {} finished reflection job(s) failed",
                    reflection.failed,
                    reflection.done + reflection.failed
                ),
            )
        } else if reflection.failed > 0 {
            (
                HealthStatus::Degraded,
                format!(
                    "{} reflection job(s) failed ({} done, {} queued)",
                    reflection.failed,
                    reflection.done,
                    reflection.backlog()
                ),
            )
        } else if reflection.backlog() >= REFLECTION_BACKLOG_DEGRADED {
            (
                HealthStatus::Degraded,
                format!(
                    "{} reflection job(s) queued for {} done",
                    reflection.backlog(),
                    reflection.done
                ),
            )
        } else {
            (
                HealthStatus::Healthy,
                format!(
                    "{} reflection job(s): {} done, {} queued",
                    reflection.total,
                    reflection.done,
                    reflection.backlog()
                ),
            )
        };
        subsystems.push(SubsystemHealth {
            name: "reflection".to_string(),
            status,
            details,
        });

        // 3. Learning proposals: unresolved recorded issues, operator backlog,
        //    and wholesale rejection.
        let (status, details) = if proposals.pending_record_issue > 0 {
            let status = if proposals.pending_record_issue >= UNRESOLVED_ISSUES_UNHEALTHY {
                HealthStatus::Unhealthy
            } else {
                HealthStatus::Degraded
            };
            (
                status,
                format!(
                    "{} pending record_issue proposal(s): recorded failures are unresolved",
                    proposals.pending_record_issue
                ),
            )
        } else if proposals.total == 0 {
            if experiences.all_time.total == 0 {
                (
                    HealthStatus::Unknown,
                    "no learning proposals recorded yet".to_string(),
                )
            } else if experiences.all_time.problems > 0 {
                (
                    HealthStatus::Degraded,
                    format!(
                        "{} failing experience(s) recorded but no issue proposal exists: reflection has not run",
                        experiences.all_time.problems
                    ),
                )
            } else if experiences.authoritative_sample().is_some() {
                (
                    HealthStatus::Healthy,
                    "no proposals recorded: no failures or corrections to learn from".to_string(),
                )
            } else {
                // Too few experiences to claim the learning loop is fine.
                (
                    HealthStatus::Unknown,
                    format!(
                        "no learning proposals recorded yet (only {} experience(s), {} needed)",
                        experiences.all_time.total,
                        metrics::MIN_SAMPLE
                    ),
                )
            }
        } else if proposals.pending >= PROPOSAL_BACKLOG_DEGRADED {
            (
                HealthStatus::Degraded,
                format!(
                    "{} pending proposal(s) awaiting a decision",
                    proposals.pending
                ),
            )
        } else if proposals.total >= metrics::MIN_SAMPLE
            && proposals
                .rejection_rate()
                .is_some_and(|rate| rate >= REJECTION_RATE_DEGRADED)
        {
            (
                HealthStatus::Degraded,
                format!(
                    "{} of {} decided proposal(s) rejected",
                    proposals.rejected,
                    proposals.approved + proposals.applied + proposals.rejected
                ),
            )
        } else {
            (
                HealthStatus::Healthy,
                format!(
                    "{} proposal(s): {} pending, {} applied, {} rejected",
                    proposals.total, proposals.pending, proposals.applied, proposals.rejected
                ),
            )
        };
        subsystems.push(SubsystemHealth {
            name: "learning_proposals".to_string(),
            status,
            details,
        });

        // 4. Memory: corrections recorded but nothing persisted is a real
        //    learning failure; an empty store is simply unverified.
        let (status, details) = if live_memory > 0 {
            (
                HealthStatus::Healthy,
                format!("{live_memory} live memory entr(y/ies)"),
            )
        } else if experiences.all_time.with_corrections > 0 {
            (
                HealthStatus::Degraded,
                format!(
                    "{} experience(s) carried user corrections but no live memory entry exists",
                    experiences.all_time.with_corrections
                ),
            )
        } else {
            (
                HealthStatus::Unknown,
                "no live memory entries yet (no corrections recorded to learn from)".to_string(),
            )
        };
        subsystems.push(SubsystemHealth {
            name: "memory".to_string(),
            status,
            details,
        });

        let worst = subsystems
            .iter()
            .filter(|s| s.status != HealthStatus::Unknown)
            .map(|s| s.status)
            .max_by_key(|s| s.severity_rank());

        let overall_status = match worst {
            // Nothing in the store is backed by evidence.
            None => HealthStatus::Unknown,
            // `Healthy` is only claimed once the experience sample is large
            // enough to support it; a handful of rows is still unverified.
            Some(HealthStatus::Healthy) if experiences.authoritative_sample().is_none() => {
                HealthStatus::Unknown
            }
            Some(status) => status,
        };

        Ok(RuntimeHealth {
            overall_status,
            subsystems,
        })
    }

    /// Monitor: real host and store measurements.
    ///
    /// `cpu_usage`/`memory_usage` are host probes (`/proc`), `disk_usage` is
    /// the store's own footprint against a documented budget, and `notes`
    /// records the provenance (or unavailability) of each.
    fn collect_resource_metrics(&self) -> Result<ResourceMetrics> {
        let footprint = self.store.store_footprint_bytes()?;
        let mut notes = Vec::new();

        let cpu_usage = host_cpu_pressure();
        match cpu_usage {
            Some(value) => notes.push(format!(
                "cpu_usage: 1-minute host load average / online CPUs = {value:.2}"
            )),
            None => {
                notes.push("cpu_usage: host load is not readable on this platform; 0.0".to_string())
            }
        }

        let memory_usage = host_memory_usage();
        match memory_usage {
            Some(value) => notes.push(format!(
                "memory_usage: host (MemTotal - MemAvailable) / MemTotal = {value:.2}"
            )),
            None => notes.push(
                "memory_usage: host memory is not readable on this platform; 0.0".to_string(),
            ),
        }

        let disk_usage = (footprint as f32 / STORE_DISK_BUDGET_BYTES as f32).clamp(0.0, 1.0);
        notes.push(format!(
            "disk_usage: evolution store occupies {footprint} bytes of a {STORE_DISK_BUDGET_BYTES} byte budget"
        ));
        notes.push(
            "network_active: no network telemetry exists in the evolution store; reported false"
                .to_string(),
        );

        Ok(ResourceMetrics {
            cpu_usage: cpu_usage.unwrap_or(0.0),
            memory_usage: memory_usage.unwrap_or(0.0),
            disk_usage,
            network_active: false,
            notes,
        })
    }

    /// Monitor: provider health as measured from recorded experiences.
    ///
    /// The store does not attribute outcomes to a provider or model, so the
    /// rate is the failure share of the sampled experiences and `notes` says
    /// so. With no sample the status is explicitly unavailable instead of
    /// optimistic.
    fn collect_provider_status(&self) -> Result<ProviderStatus> {
        let experiences = self.store.experience_stats(WINDOW_MS)?;
        let reflection = self.store.reflection_stats()?;
        let mut notes = Vec::new();

        let (error_rate, available, sample_count) = match experiences.authoritative_sample() {
            Some(sample) => {
                let rate = sample.failure_rate().unwrap_or(0.0);
                notes.push(format!(
                    "error_rate: {:.0}% of {} sampled experience(s) failed (outcomes are not attributed to a provider)",
                    rate * 100.0,
                    sample.total
                ));
                (rate, rate < UNHEALTHY_FAILURE_RATE, sample.total)
            }
            None => {
                notes.push(format!(
                    "available: unverified, only {} experience(s) recorded (need {})",
                    experiences.all_time.total,
                    metrics::MIN_SAMPLE
                ));
                (0.0, false, experiences.all_time.total)
            }
        };

        let latency_ms = match experiences.avg_duration_ms() {
            Some(ms) => {
                notes.push(format!(
                    "latency_ms: mean recorded experience duration ({ms}ms)"
                ));
                ms
            }
            None => match reflection.avg_latency_ms {
                Some(ms) => {
                    notes.push(format!(
                        "latency_ms: no experience recorded a duration; using mean reflection-job latency ({ms}ms)"
                    ));
                    ms
                }
                None => {
                    notes.push(
                        "latency_ms: no duration recorded in experiences or reflection jobs; 0"
                            .to_string(),
                    );
                    0
                }
            },
        };

        Ok(ProviderStatus {
            available,
            latency_ms,
            error_rate,
            sample_count,
            notes,
        })
    }

    /// Monitor: the actual recorded experiences inside the recency window.
    ///
    /// `duration_ms`/`tokens_used` are `0` when the producer did not record
    /// them; see [`metrics::recorded_duration_ms`].
    fn collect_execution_outcomes(&self) -> Result<Vec<ExecutionOutcome>> {
        let since = now_ms().saturating_sub(WINDOW_MS);
        let experiences = self.store.list_experiences_since(since, MAX_OUTCOMES)?;
        Ok(experiences
            .iter()
            .map(|e| ExecutionOutcome {
                task_id: e.id.clone(),
                success: e.outcome == ExperienceOutcome::Success,
                duration_ms: metrics::recorded_duration_ms(e).unwrap_or(0),
                tokens_used: metrics::recorded_tokens_used(e).unwrap_or(0),
            })
            .collect())
    }

    /// Monitor: failing experiences and failed reflection jobs, newest first.
    fn collect_failures(&self) -> Result<Vec<FailureRecord>> {
        let since = now_ms().saturating_sub(WINDOW_MS);
        let mut failures: Vec<FailureRecord> = self
            .store
            .list_failed_experiences_since(since, MAX_FAILURES)?
            .into_iter()
            .map(|e| FailureRecord {
                id: e.id.clone(),
                component: e.task_category.clone(),
                error_type: e.outcome.as_str().to_string(),
                message: if e.errors.is_empty() {
                    format!(
                        "{} task reported outcome `{}` with no error detail",
                        e.task_category,
                        e.outcome.as_str()
                    )
                } else {
                    e.errors.join("; ")
                },
                timestamp_ms: e.timestamp_ms,
            })
            .collect();

        for job in self.store.list_failed_jobs(MAX_FAILURES)? {
            failures.push(FailureRecord {
                id: job.id.clone(),
                component: "reflection".to_string(),
                error_type: "reflection_job_failed".to_string(),
                message: job.error.unwrap_or_else(|| {
                    "reflection job failed without an error message".to_string()
                }),
                timestamp_ms: job.updated_ms,
            });
        }

        failures.sort_by_key(|f| std::cmp::Reverse(f.timestamp_ms));
        Ok(failures)
    }

    /// Execute one planned action.
    ///
    /// # What this loop can and cannot do
    ///
    /// A [`MapekLoop`] owns exactly one handle — the shared [`EvolutionStore`].
    /// It has no broker, provider client, scheduler, agent registry or
    /// runtime-configuration handle, so it cannot itself restart a provider,
    /// switch a model, edit a skill or change a runtime setting. Execution is
    /// therefore split in two, and neither half claims more than it did:
    ///
    /// - **`ActionType::Recovery` targeting [`REFLECTION_TARGET`]** — a real
    ///   remediation the store can perform: every failed reflection job below
    ///   [`MAX_REFLECTION_ATTEMPTS`] is put back to `pending` so that
    ///   [`crate::reflection::ReflectionEngine::process_pending`] retries it,
    ///   and the new state is read back to verify the write landed. Only this
    ///   path reports `success: true` ([`ActionOutcome::Remediated`]).
    /// - **Everything else** — not performed. The action is persisted as a
    ///   pending [`ProposalKind::RecordIssue`] proposal so it reaches an
    ///   operator instead of being dropped, and the result reports
    ///   `success: false`, [`ActionOutcome::RecordedForReview`], an `error`
    ///   naming the missing actuator, and the output `"recorded action <id>
    ///   for operator review; no automatic remediation for <type>"`.
    ///
    /// A recovery action with no failed job to re-queue does nothing at all
    /// and says so ([`ActionOutcome::NotExecuted`]); failed jobs that already
    /// exhausted their retries are escalated for review rather than retried
    /// forever. Store failures are surfaced in the action's `error`/
    /// `outcome` — never swallowed — and keep
    /// [`ExecutionResult::overall_success`] `false`.
    fn execute_action(&self, action: &PlannedAction) -> Result<ActionResult> {
        match &action.action_type {
            ActionType::Recovery if action.target == REFLECTION_TARGET => {
                self.recover_failed_reflection_jobs(action)
            }
            ActionType::Recovery => self.record_for_review(
                action,
                &format!(
                    "the loop's only recovery actuator re-queues failed reflection jobs; it cannot recover `{}`",
                    action.target
                ),
            ),
            other => {
                let reason = format!(
                    "this loop has no {} actuator: it can only re-queue failed reflection jobs through the store",
                    other.as_str()
                );
                self.record_for_review(action, &reason)
            }
        }
    }

    /// Re-queue failed reflection jobs so the reflection engine retries them.
    ///
    /// This is the loop's only real actuator, and it is store-backed and
    /// verifiable: each job row is rewritten to `pending` with a fresh
    /// `updated_ms` (attempts and last error are preserved so retry accounting
    /// stays honest) and the result is read back with
    /// [`EvolutionStore::list_failed_jobs`] before success is claimed.
    ///
    /// - No failed job at all: nothing is written, outcome
    ///   [`ActionOutcome::NotExecuted`].
    /// - Every failed job already at [`MAX_REFLECTION_ATTEMPTS`]: nothing is
    ///   rewritten, and the situation is escalated for operator review.
    /// - Otherwise: the jobs are re-queued; the returned output names them.
    fn recover_failed_reflection_jobs(&self, action: &PlannedAction) -> Result<ActionResult> {
        let failed = match self.store.list_failed_jobs(MAX_FAILURES) {
            Ok(jobs) => jobs,
            Err(e) => {
                return Ok(not_executed(
                    action,
                    format!("could not read failed reflection jobs: {e}"),
                ));
            }
        };
        if failed.is_empty() {
            return Ok(not_executed(
                action,
                "no failed reflection job exists to re-queue; nothing was changed".to_string(),
            ));
        }

        let mut requeued: Vec<String> = Vec::new();
        let mut exhausted: Vec<String> = Vec::new();
        let total_failed = failed.len();

        for mut job in failed {
            if job.attempts >= MAX_REFLECTION_ATTEMPTS {
                exhausted.push(job.id);
                continue;
            }
            job.status = ReflectionJobStatus::Pending;
            job.updated_ms = now_ms();
            if let Err(e) = self.store.save_job(&job) {
                return Ok(not_executed(
                    action,
                    format!(
                        "could not re-queue reflection job {}: {e} ({} of {total_failed} job(s) re-queued before the failure)",
                        job.id,
                        requeued.len()
                    ),
                ));
            }
            requeued.push(job.id);
        }

        if requeued.is_empty() {
            return self.record_for_review(
                action,
                &format!(
                    "all {total_failed} failed reflection job(s) reached the {MAX_REFLECTION_ATTEMPTS}-attempt retry cap"
                ),
            );
        }

        // Verify the side effect instead of trusting the write: the jobs must
        // no longer be listed as failed.
        let still_failed: Vec<String> = match self.store.list_failed_jobs(MAX_FAILURES) {
            Ok(jobs) => jobs
                .into_iter()
                .map(|j| j.id)
                .filter(|id| requeued.contains(id))
                .collect(),
            Err(e) => {
                return Ok(not_executed(
                    action,
                    format!("could not verify the re-queued reflection jobs: {e}"),
                ));
            }
        };
        if !still_failed.is_empty() {
            return Ok(not_executed(
                action,
                format!(
                    "re-queue did not persist for reflection job(s) {}: the store still lists them as failed",
                    still_failed.join(", ")
                ),
            ));
        }

        let mut output = format!(
            "re-queued {} failed reflection job(s) as pending for retry: {}",
            requeued.len(),
            requeued.join(", ")
        );
        if !exhausted.is_empty() {
            output.push_str(&format!(
                "; {} job(s) left failed at the {MAX_REFLECTION_ATTEMPTS}-attempt cap: {}",
                exhausted.len(),
                exhausted.join(", ")
            ));
        }

        Ok(ActionResult {
            action_id: action.id.clone(),
            success: true,
            output,
            error: None,
            outcome: ActionOutcome::Remediated,
            review_proposal_id: None,
        })
    }

    /// Persist an action the loop cannot perform as a pending
    /// [`ProposalKind::RecordIssue`] proposal for operator review.
    ///
    /// This records; it does not remediate, and the returned [`ActionResult`]
    /// says exactly that: `success: false`,
    /// [`ActionOutcome::RecordedForReview`], the persisted proposal id, an
    /// `error` naming the missing actuator, and an output that spells out
    /// "recorded … for operator review; no automatic remediation for …". The
    /// proposal is read back through the store by the caller's own queries, so
    /// the effect is verifiable.
    fn record_for_review(&self, action: &PlannedAction, reason: &str) -> Result<ActionResult> {
        let proposal = LearningProposal {
            id: gen_id("prop"),
            experience_id: None,
            agent_id: MAPEK_AGENT.to_string(),
            kind: ProposalKind::RecordIssue,
            summary: format!(
                "MAPE-K action {} ({}) on `{}` needs an operator",
                action.id,
                action.action_type.as_str(),
                action.target
            ),
            details: serde_json::json!({
                "source": MAPEK_AGENT,
                "action_id": action.id,
                "action_type": action.action_type.as_str(),
                "target": action.target,
                "description": action.description,
                "parameters": action.parameters,
                "no_automatic_remediation": reason,
            }),
            confidence: 1.0,
            requires_approval: true,
            created_ms: now_ms(),
            status: ProposalStatus::Pending,
        };

        if let Err(e) = self.store.save_proposal(&proposal) {
            return Ok(not_executed(
                action,
                format!(
                    "could not persist the operator-review record for action {}: {e}",
                    action.id
                ),
            ));
        }

        Ok(ActionResult {
            action_id: action.id.clone(),
            success: false,
            output: format!(
                "recorded action {} for operator review; no automatic remediation for {}",
                action.id,
                action.action_type.as_str()
            ),
            error: Some(format!(
                "no automatic remediation for {} on `{}`: {reason}",
                action.action_type.as_str(),
                action.target
            )),
            outcome: ActionOutcome::RecordedForReview,
            review_proposal_id: Some(proposal.id),
        })
    }
}

/// An action result for work that was neither remediated nor recorded.
fn not_executed(action: &PlannedAction, reason: String) -> ActionResult {
    ActionResult {
        action_id: action.id.clone(),
        success: false,
        output: format!(
            "no effect for action {} ({}) on `{}`: {reason}",
            action.id,
            action.action_type.as_str(),
            action.target
        ),
        error: Some(reason),
        outcome: ActionOutcome::NotExecuted,
        review_proposal_id: None,
    }
}

/// Outcome of one complete MAPE-K cycle.
///
/// `execution_success` is `true` only when the plan produced by this cycle was
/// really remediated; `failure_reason` explains any shortfall instead of
/// hiding it.
#[derive(Clone, Debug)]
pub struct CycleResult {
    pub cycle_number: u32,
    pub monitoring_data_id: String,
    pub analysis_id: String,
    pub plan_id: String,
    /// `true` only when every planned action was
    /// [`ActionOutcome::Remediated`] and the plan was not empty.
    pub execution_success: bool,
    pub timestamp_ms: u64,
    /// How many actions this cycle's plan held.
    pub planned_actions: usize,
    /// How many of them a real change was applied for.
    pub remediated_actions: usize,
    /// How many of them were only recorded for operator review.
    pub recorded_for_review: usize,
    /// Why the cycle did not remediate its plan, when it did not.
    pub failure_reason: Option<String>,
}

// ---------- host probes ----------
//
// These read real numbers from the host. They are deliberately best-effort:
// a platform without `/proc` returns `None` (and the collector says so in
// `notes`) rather than a made-up value.

/// The `procfs` value for `key` in kB, e.g. `("MemTotal:", 16_000_000)`.
fn proc_meminfo_kb(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    })
}

/// Fraction of host memory currently in use, from `/proc/meminfo`.
fn host_memory_usage() -> Option<f32> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let total = proc_meminfo_kb(&text, "MemTotal:")?;
    let available = proc_meminfo_kb(&text, "MemAvailable:")?;
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(available);
    Some((used as f32 / total as f32).clamp(0.0, 1.0))
}

/// Number of online CPUs, from `/proc/cpuinfo`; `1` when unreadable.
fn host_cpu_count() -> usize {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|text| {
            text.lines()
                .filter(|line| line.starts_with("processor"))
                .count()
        })
        .ok()
        .filter(|count| *count > 0)
        .unwrap_or(1)
}

/// 1-minute host load average divided by the online CPU count, clamped to
/// `0.0..=1.0` — a load-pressure reading for CPU utilisation that needs no
/// two-sample delta.
fn host_cpu_pressure() -> Option<f32> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let one_minute: f32 = text.split_whitespace().next()?.parse().ok()?;
    Some((one_minute / host_cpu_count() as f32).clamp(0.0, 1.0))
}
