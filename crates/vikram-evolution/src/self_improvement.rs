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

use crate::error::Result;
use crate::metrics;
use crate::store::EvolutionStore;
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            opportunities: Vec::new(),
            executions: Vec::new(),
            last_scan_note: None,
        }
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
    pub fn execute_improvement(
        &mut self,
        opportunity: &ImprovementOpportunity,
    ) -> Result<ImprovementExecution> {
        let execution = ImprovementExecution {
            id: gen_id("exec"),
            opportunity_id: opportunity.id.clone(),
            steps: vec![
                ImprovementStep {
                    description: "Analyze current implementation".to_string(),
                    status: StepStatus::Pending,
                    output: None,
                },
                ImprovementStep {
                    description: "Design improvement".to_string(),
                    status: StepStatus::Pending,
                    output: None,
                },
                ImprovementStep {
                    description: "Implement improvement".to_string(),
                    status: StepStatus::Pending,
                    output: None,
                },
                ImprovementStep {
                    description: "Test improvement".to_string(),
                    status: StepStatus::Pending,
                    output: None,
                },
                ImprovementStep {
                    description: "Verify improvement".to_string(),
                    status: StepStatus::Pending,
                    output: None,
                },
            ],
            status: ExecutionStatus::Pending,
            started_ms: now_ms(),
            completed_ms: None,
            outcome: None,
        };

        self.executions.push(execution.clone());
        Ok(execution)
    }

    /// Track improvement history.
    pub fn track_improvement(&mut self, execution: ImprovementExecution) {
        self.executions.push(execution);
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
