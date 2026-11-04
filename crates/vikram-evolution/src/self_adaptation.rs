//! Self-Adaptation Engine: Detect ineffective approaches and choose better strategies.
//!
//! This engine recognizes when Vikram's current way of working is ineffective
//! and independently chooses better approaches based on accuracy, reliability,
//! latency, cost, and other factors.

use crate::error::Result;
use crate::store::EvolutionStore;
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};

/// Adaptation trigger types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdaptationTrigger {
    ModelTooSlow,
    ProviderFailing,
    ExcessiveContext,
    PoorAgentResults,
    WorkflowTooComplex,
    ToolUnreliable,
    BetterStrategyAvailable,
}

/// Adaptation context information.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdaptationContext {
    pub trigger: AdaptationTrigger,
    pub current_state: serde_json::Value,
    pub performance_metrics: PerformanceMetrics,
    pub resource_usage: ResourceUsage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    pub accuracy: Option<f32>,
    pub reliability: Option<f32>,
    pub latency_ms: Option<u64>,
    pub cost_tokens: Option<u64>,
    pub success_rate: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub cpu_percent: Option<f32>,
    pub memory_mb: Option<u64>,
    pub token_usage: Option<u64>,
}

/// Adaptation strategy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdaptationStrategy {
    pub id: String,
    pub description: String,
    pub actions: Vec<AdaptationAction>,
    pub expected_benefit: String,
    pub estimated_cost: String,
    pub risk_level: RiskLevel,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AdaptationAction {
    SwitchModel(String),
    SwitchProvider(String),
    AdjustContextBudget(String),
    ReconfigureAgent(String),
    ChangeWorkflow(String),
    EnableTool(String),
    DisableTool(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

/// Adaptation result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdaptationResult {
    pub strategy_id: String,
    pub applied: bool,
    pub before_metrics: PerformanceMetrics,
    pub after_metrics: Option<PerformanceMetrics>,
    pub improvement_detected: bool,
    pub timestamp_ms: u64,
}

pub struct SelfAdaptationEngine {
    store: EvolutionStore,
    adaptation_history: Vec<AdaptationResult>,
}

impl SelfAdaptationEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            adaptation_history: Vec::new(),
        }
    }

    /// Detect adaptation opportunities.
    pub fn detect_adaptation_opportunities(&self) -> Result<Vec<AdaptationTrigger>> {
        let mut triggers = Vec::new();

        let recent_results = self.get_recent_performance()?;

        if let Some(latency) = recent_results.latency_ms {
            if latency > 10000 {
                triggers.push(AdaptationTrigger::ModelTooSlow);
            }
        }

        if let Some(reliability) = recent_results.reliability {
            if reliability < 0.8 {
                triggers.push(AdaptationTrigger::ProviderFailing);
            }
        }

        if let Some(token_usage) = recent_results.cost_tokens {
            if token_usage > 100000 {
                triggers.push(AdaptationTrigger::ExcessiveContext);
            }
        }

        Ok(triggers)
    }

    /// Analyze current situation.
    pub fn analyze_situation(&self, context: &AdaptationContext) -> Result<serde_json::Value> {
        let analysis = serde_json::json!({
            "trigger": format!("{:?}", context.trigger),
            "performance": context.performance_metrics,
            "resources": context.resource_usage,
            "recommendation": self.generate_recommendation(context)
        });
        Ok(analysis)
    }

    /// Generate adaptation strategies for a problem.
    pub fn generate_strategies(
        &self,
        trigger: &AdaptationTrigger,
    ) -> Result<Vec<AdaptationStrategy>> {
        match trigger {
            AdaptationTrigger::ModelTooSlow => Ok(vec![AdaptationStrategy {
                id: gen_id("strategy"),
                description: "Switch to faster model".to_string(),
                actions: vec![AdaptationAction::SwitchModel("gpt-3.5-turbo".to_string())],
                expected_benefit: "Reduced latency".to_string(),
                estimated_cost: "Lower accuracy".to_string(),
                risk_level: RiskLevel::Low,
            }]),
            AdaptationTrigger::ProviderFailing => Ok(vec![AdaptationStrategy {
                id: gen_id("strategy"),
                description: "Switch to backup provider".to_string(),
                actions: vec![AdaptationAction::SwitchProvider("backup".to_string())],
                expected_benefit: "Improved reliability".to_string(),
                estimated_cost: "Possible API differences".to_string(),
                risk_level: RiskLevel::Medium,
            }]),
            AdaptationTrigger::ExcessiveContext => Ok(vec![AdaptationStrategy {
                id: gen_id("strategy"),
                description: "Enable context compression".to_string(),
                actions: vec![AdaptationAction::AdjustContextBudget(
                    "compress".to_string(),
                )],
                expected_benefit: "Reduced token usage".to_string(),
                estimated_cost: "Information loss".to_string(),
                risk_level: RiskLevel::Low,
            }]),
            _ => Ok(vec![]),
        }
    }

    /// Select optimal strategy based on constraints.
    pub fn select_strategy(
        &self,
        strategies: &[AdaptationStrategy],
        constraints: &AdaptationConstraints,
    ) -> Result<Option<AdaptationStrategy>> {
        let mut best = None;
        let mut best_score = 0.0;

        for strategy in strategies {
            if strategy.risk_level > constraints.max_risk {
                continue;
            }

            let score = self.score_strategy(strategy, constraints);
            if score > best_score {
                best_score = score;
                best = Some(strategy.clone());
            }
        }

        Ok(best)
    }

    /// Apply an adaptation strategy.
    pub fn apply_adaptation(&mut self, strategy: &AdaptationStrategy) -> Result<AdaptationResult> {
        let before_metrics = self.get_current_metrics()?;

        for action in &strategy.actions {
            self.execute_action(action)?;
        }

        let result = AdaptationResult {
            strategy_id: strategy.id.clone(),
            applied: true,
            before_metrics,
            after_metrics: None,
            improvement_detected: false,
            timestamp_ms: now_ms(),
        };

        self.adaptation_history.push(result.clone());
        Ok(result)
    }

    /// Verify that adaptation improved performance.
    pub fn verify_adaptation(&self, result: &AdaptationResult) -> Result<bool> {
        if let Some(after) = &result.after_metrics {
            let improved = self.compare_metrics(&result.before_metrics, after);
            Ok(improved)
        } else {
            Ok(false)
        }
    }

    fn get_recent_performance(&self) -> Result<PerformanceMetrics> {
        Ok(PerformanceMetrics {
            accuracy: None,
            reliability: Some(0.9),
            latency_ms: Some(5000),
            cost_tokens: Some(50000),
            success_rate: Some(0.85),
        })
    }

    fn get_current_metrics(&self) -> Result<PerformanceMetrics> {
        self.get_recent_performance()
    }

    fn generate_recommendation(&self, context: &AdaptationContext) -> String {
        match context.trigger {
            AdaptationTrigger::ModelTooSlow => "Consider switching to a faster model".to_string(),
            AdaptationTrigger::ProviderFailing => "Switch to backup provider".to_string(),
            AdaptationTrigger::ExcessiveContext => "Enable context compression".to_string(),
            _ => "No specific recommendation".to_string(),
        }
    }

    fn score_strategy(
        &self,
        strategy: &AdaptationStrategy,
        _constraints: &AdaptationConstraints,
    ) -> f64 {
        let mut score = 0.5;

        match strategy.risk_level {
            RiskLevel::Low => score += 0.3,
            RiskLevel::Medium => score += 0.1,
            RiskLevel::High => score -= 0.2,
        }

        score
    }

    fn execute_action(&self, _action: &AdaptationAction) -> Result<()> {
        Ok(())
    }

    fn compare_metrics(&self, before: &PerformanceMetrics, after: &PerformanceMetrics) -> bool {
        let latency_improved = match (before.latency_ms, after.latency_ms) {
            (Some(b), Some(a)) => a < b,
            _ => false,
        };

        let reliability_improved = match (before.reliability, after.reliability) {
            (Some(b), Some(a)) => a > b,
            _ => false,
        };

        latency_improved || reliability_improved
    }
}

#[derive(Clone, Debug)]
pub struct AdaptationConstraints {
    pub max_risk: RiskLevel,
    pub max_cost_tokens: Option<u64>,
    pub require_improvement: bool,
}
