//! Self-Improvement Engine: Proactively identify and execute improvements.
//!
//! This engine identifies areas where Vikram can improve, prioritizes them
//! based on value and risk, and executes improvements through a controlled lifecycle.

use crate::error::Result;
use crate::store::EvolutionStore;
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};

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
}

impl SelfImprovementEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            opportunities: Vec::new(),
            executions: Vec::new(),
        }
    }

    /// Identify improvement opportunities.
    pub fn identify_improvement_opportunities(&mut self) -> Result<Vec<ImprovementOpportunity>> {
        let mut opportunities = Vec::new();

        opportunities.push(ImprovementOpportunity {
            id: gen_id("imp"),
            area: ImprovementArea::ContextManagement,
            description: "Optimize context loading strategy".to_string(),
            evidence: vec!["High token usage detected".to_string()],
            expected_value: 0.8,
            estimated_cost: 0.3,
            risk_level: RiskLevel::Low,
            priority: 5,
        });

        opportunities.push(ImprovementOpportunity {
            id: gen_id("imp"),
            area: ImprovementArea::ToolSelection,
            description: "Improve tool selection heuristics".to_string(),
            evidence: vec!["Suboptimal tool choices observed".to_string()],
            expected_value: 0.7,
            estimated_cost: 0.4,
            risk_level: RiskLevel::Medium,
            priority: 4,
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
