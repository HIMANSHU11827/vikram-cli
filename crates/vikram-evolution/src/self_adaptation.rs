//! Self-Adaptation Engine: Detect ineffective approaches and choose better strategies.
//!
//! This engine recognizes when Vikram's current way of working is ineffective
//! and independently chooses better approaches based on accuracy, reliability,
//! latency, cost, and other factors.
//!
//! Its performance signal is [`SelfAdaptationEngine::recent_performance`],
//! which is aggregated from the recorded experiences and reflection jobs in
//! the [`EvolutionStore`] (see [`crate::metrics`]) — not from constants. When
//! the store holds no evidence a field is `None` and the corresponding
//! adaptation trigger simply cannot fire, which is the honest outcome: there
//! is nothing measured to adapt to.
//!
//! # Applying an adaptation is bounded by what the engine owns
//!
//! [`SelfAdaptationEngine`] owns one handle — the [`EvolutionStore`]. It has
//! no model catalogue, provider registry, agent configuration or tool
//! registry handle, so it cannot switch a model, switch a provider,
//! reconfigure an agent or enable/disable a tool — those
//! [`AdaptationAction`] variants return `Err` with a reason and become
//! persisted, operator-visible `record_issue` proposals. Exactly one action
//! is real: [`AdaptationAction::ConsolidateMemory`] runs
//! [`crate::memory::LayeredMemory::consolidate`] against the store,
//! superseding duplicate live memory entries — an observable mutation this
//! engine can truthfully perform, so it is the only variant
//! [`SelfAdaptationEngine::execute_action`] answers `Ok(())` for.
//! [`AdaptationResult::applied`] is `true` only when an action was actually
//! carried out, so a strategy that was merely recorded never reads as
//! applied.
//!
//! DORMANT / store-scoped: no runtime or CLI path calls this engine today.
//! Detection reads the durable store and `adaptation_history` is persisted
//! to `engine_records` under `self_adaptation.result`, rehydrated on the
//! first [`SelfAdaptationEngine::apply_adaptation`].

use crate::error::{EvolutionError, Result};
use crate::memory::LayeredMemory;
use crate::metrics;
use crate::store::{EvolutionStore, from_json_value, to_json_value};
use crate::types::{LearningEvent, LearningProposal, ProposalKind, ProposalStatus, gen_id, now_ms};
use serde::{Deserialize, Serialize};

/// `engine_records` namespace holding every [`AdaptationResult`].
const RESULT_NAMESPACE: &str = "self_adaptation.result";

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
    /// Run memory consolidation against the store: supersede duplicate
    /// live memory entries. The only action this engine can actually
    /// carry out — it mutates store state it owns.
    ConsolidateMemory,
}

impl AdaptationAction {
    /// Stable `snake_case` name of the action, used in persisted records.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SwitchModel(_) => "switch_model",
            Self::SwitchProvider(_) => "switch_provider",
            Self::AdjustContextBudget(_) => "adjust_context_budget",
            Self::ReconfigureAgent(_) => "reconfigure_agent",
            Self::ChangeWorkflow(_) => "change_workflow",
            Self::EnableTool(_) => "enable_tool",
            Self::DisableTool(_) => "disable_tool",
            Self::ConsolidateMemory => "consolidate_memory",
        }
    }

    /// The value the action wants applied (model id, provider id, …), if it
    /// carries one. [`AdaptationAction::ConsolidateMemory`] carries none.
    pub fn value(&self) -> &str {
        match self {
            Self::SwitchModel(v)
            | Self::SwitchProvider(v)
            | Self::AdjustContextBudget(v)
            | Self::ReconfigureAgent(v)
            | Self::ChangeWorkflow(v)
            | Self::EnableTool(v)
            | Self::DisableTool(v) => v,
            Self::ConsolidateMemory => "",
        }
    }

    /// One-line description, e.g. `"switch_model -> gpt-3.5-turbo"`.
    pub fn describe(&self) -> String {
        let value = self.value();
        if value.is_empty() {
            self.kind().to_string()
        } else {
            format!("{} -> {}", self.kind(), value)
        }
    }
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
    /// `true` only when every action of the strategy was actually carried out.
    /// A strategy whose actions could only be recorded for an operator is
    /// `false`; see [`AdaptationResult::not_applied`].
    pub applied: bool,
    pub before_metrics: PerformanceMetrics,
    pub after_metrics: Option<PerformanceMetrics>,
    pub improvement_detected: bool,
    pub timestamp_ms: u64,
    /// Why each action that was not applied was not applied, one entry per
    /// action, in strategy order. Empty when every action was applied.
    #[serde(default)]
    pub not_applied: Vec<String>,
    /// Ids of the pending `record_issue` proposals persisted so the operator
    /// can pick the unapplied actions up. One per unapplied action.
    #[serde(default)]
    pub review_proposal_ids: Vec<String>,
}

pub struct SelfAdaptationEngine {
    store: EvolutionStore,
    adaptation_history: Vec<AdaptationResult>,
    /// Whether `adaptation_history` has been rehydrated from `engine_records`.
    hydrated: bool,
}

impl SelfAdaptationEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            adaptation_history: Vec::new(),
            hydrated: false,
        }
    }

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Recorded adaptation results so far (rehydrated from the store on
    /// the first [`Self::apply_adaptation`]).
    pub fn get_adaptation_history(&self) -> &[AdaptationResult] {
        &self.adaptation_history
    }

    /// Load persisted results into memory once. Mutating entry points call
    /// this first so post-restart callers see the durable history; a
    /// corrupt payload is an error, not a silently-empty list.
    fn hydrate(&mut self) -> Result<()> {
        if self.hydrated {
            return Ok(());
        }
        let mut results = Vec::new();
        for (_id, payload) in self.store.list_engine_records(RESULT_NAMESPACE)? {
            results.push(from_json_value::<AdaptationResult>(payload)?);
        }
        results.sort_by_key(|r| r.timestamp_ms);
        self.adaptation_history = results;
        self.hydrated = true;
        Ok(())
    }

    /// Detect adaptation opportunities.
    pub fn detect_adaptation_opportunities(&self) -> Result<Vec<AdaptationTrigger>> {
        let mut triggers = Vec::new();

        let recent_results = self.get_recent_performance()?;

        if let Some(latency) = recent_results.latency_ms
            && latency > 10000
        {
            triggers.push(AdaptationTrigger::ModelTooSlow);
        }

        if let Some(reliability) = recent_results.reliability
            && reliability < 0.8
        {
            triggers.push(AdaptationTrigger::ProviderFailing);
        }

        if let Some(token_usage) = recent_results.cost_tokens
            && token_usage > 100000
        {
            triggers.push(AdaptationTrigger::ExcessiveContext);
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
            AdaptationTrigger::ExcessiveContext => Ok(vec![
                AdaptationStrategy {
                    id: gen_id("strategy"),
                    description: "Enable context compression".to_string(),
                    actions: vec![AdaptationAction::AdjustContextBudget(
                        "compress".to_string(),
                    )],
                    expected_benefit: "Reduced token usage".to_string(),
                    estimated_cost: "Information loss".to_string(),
                    risk_level: RiskLevel::Low,
                },
                AdaptationStrategy {
                    id: gen_id("strategy"),
                    description: "Consolidate duplicate memory entries".to_string(),
                    actions: vec![AdaptationAction::ConsolidateMemory],
                    expected_benefit: "Fewer live memory entries for context retrieval to load"
                        .to_string(),
                    estimated_cost: "None; superseded entries remain for audit".to_string(),
                    risk_level: RiskLevel::Low,
                },
            ]),
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
    ///
    /// Each action goes through [`Self::execute_action`], which can only
    /// succeed when the engine actually changed something it owns. Since this
    /// engine owns the store and nothing else, every adaptation action comes
    /// back as an `Err`; instead of dropping those errors (which is what made
    /// this method claim `applied: true` for work that never happened) they
    /// are:
    ///
    /// 1. persisted as a pending [`ProposalKind::RecordIssue`] proposal whose
    ///    details carry the strategy id, the action and the "no automatic
    ///    remediation" reason — so an operator sees the request, and
    /// 2. surfaced in [`AdaptationResult::not_applied`] and
    ///    [`AdaptationResult::review_proposal_ids`].
    ///
    /// [`AdaptationResult::applied`] is therefore `false` whenever no action
    /// was carried out. The only error this method returns is a store failure
    /// while persisting that record: an adaptation the engine cannot apply and
    /// cannot even record is a genuine failure, never a silent success.
    ///
    /// Each result is also persisted to `engine_records` so the history
    /// survives a restart.
    pub fn apply_adaptation(&mut self, strategy: &AdaptationStrategy) -> Result<AdaptationResult> {
        self.hydrate()?;
        let before_metrics = self.get_current_metrics()?;

        let mut not_applied = Vec::new();
        let mut review_proposal_ids = Vec::new();
        let mut applied_actions = 0usize;

        for action in &strategy.actions {
            match self.execute_action(action) {
                Ok(()) => applied_actions += 1,
                Err(e) => {
                    let reason = e.to_string();
                    review_proposal_ids.push(self.record_unapplied(strategy, action, &reason)?);
                    not_applied.push(format!("{}: {reason}", action.describe()));
                }
            }
        }

        let applied = !strategy.actions.is_empty() && applied_actions == strategy.actions.len();
        let result = AdaptationResult {
            strategy_id: strategy.id.clone(),
            applied,
            before_metrics,
            after_metrics: None,
            improvement_detected: false,
            timestamp_ms: now_ms(),
            not_applied,
            review_proposal_ids,
        };

        self.adaptation_history.push(result.clone());
        self.store.save_engine_record(
            RESULT_NAMESPACE,
            &gen_id("adapt"),
            &to_json_value(&result)?,
        )?;
        Ok(result)
    }

    /// Verify that adaptation improved performance.
    ///
    /// Returns `false` when the result carries no post-adaptation measurement —
    /// including for a strategy that was never applied, whose
    /// [`AdaptationResult::after_metrics`] is always `None`. "Could not verify
    /// an improvement" is never reported as an improvement.
    pub fn verify_adaptation(&self, result: &AdaptationResult) -> Result<bool> {
        if let Some(after) = &result.after_metrics {
            let improved = self.compare_metrics(&result.before_metrics, after);
            Ok(improved)
        } else {
            Ok(false)
        }
    }

    fn get_recent_performance(&self) -> Result<PerformanceMetrics> {
        let stats = self.store.experience_stats(metrics::DEFAULT_WINDOW_MS)?;
        let reflection = self.store.reflection_stats()?;

        // Only a sample of at least `metrics::MIN_SAMPLE` experiences is
        // treated as evidence; below that the rate stays `None` and no
        // adaptation is triggered on it.
        let sample = stats.authoritative_sample();
        let success_rate = sample.and_then(|s| s.success_rate());

        Ok(PerformanceMetrics {
            // No ground-truth correctness signal is stored with an
            // experience (only its outcome), so accuracy is not claimed.
            accuracy: None,
            // Reliability is the recorded success share: the only reliability
            // signal the store actually holds.
            reliability: success_rate,
            // Mean recorded duration, falling back to real reflection-job
            // latency when no producer recorded a duration.
            latency_ms: stats.avg_duration_ms().or(reflection.avg_latency_ms),
            cost_tokens: stats.avg_tokens(),
            success_rate,
        })
    }

    /// Public read of the store-derived snapshot that
    /// [`Self::detect_adaptation_opportunities`] decides on.
    ///
    /// Fields are `None` when the store holds no evidence for them; nothing
    /// is defaulted to a plausible-looking number.
    pub fn recent_performance(&self) -> Result<PerformanceMetrics> {
        self.get_recent_performance()
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

    /// Carry out one adaptation action, or explain why it cannot be carried
    /// out.
    ///
    /// # Contract
    ///
    /// `Ok(())` means an observable change was made — it is returned only when
    /// the engine actually applied the action. `Err` carries the reason it was
    /// not applied.
    ///
    /// `SelfAdaptationEngine` holds the [`EvolutionStore`] and nothing else: it
    /// has no model catalogue, provider registry, context-budget control,
    /// agent configuration or tool registry to write to, and the store schema
    /// has no table that a model/provider/tool switch could be applied
    /// through. [`AdaptationAction::ConsolidateMemory`] is the single
    /// exception: it runs [`LayeredMemory::consolidate`], a real mutation of
    /// store state this engine owns, and records the outcome as a learning
    /// event. Every other variant returns `Err` naming the missing handle;
    /// writing a proposal into the store would only *record* the request, so
    /// it is not claimed here — the caller ([`Self::apply_adaptation`])
    /// persists that record and surfaces it, which keeps "recorded" and
    /// "applied" distinguishable.
    fn execute_action(&self, action: &AdaptationAction) -> Result<()> {
        match action {
            AdaptationAction::ConsolidateMemory => {
                let removed = LayeredMemory::new(self.store.clone()).consolidate()?;
                self.store.save_event(&LearningEvent {
                    id: gen_id("evt"),
                    timestamp_ms: now_ms(),
                    agent_id: "self_adaptation".to_string(),
                    kind: "adaptation".to_string(),
                    summary: format!(
                        "memory consolidation superseded {removed} duplicate live entries"
                    ),
                    payload: serde_json::json!({
                        "action": "consolidate_memory",
                        "removed": removed,
                    }),
                })?;
                Ok(())
            }
            _ => Err(EvolutionError::Validation(format!(
                "no automatic remediation for {}: this engine holds only the evolution store, not a {} control handle",
                action.describe(),
                action.kind()
            ))),
        }
    }

    /// Persist an adaptation action the engine cannot apply as a pending
    /// `record_issue` proposal for operator review.
    ///
    /// The record names the strategy, the action and the reason, so the
    /// request survives the call even though nothing was applied. Returns the
    /// proposal id; a store failure is propagated rather than swallowed.
    fn record_unapplied(
        &self,
        strategy: &AdaptationStrategy,
        action: &AdaptationAction,
        reason: &str,
    ) -> Result<String> {
        let proposal = LearningProposal {
            id: gen_id("prop"),
            experience_id: None,
            agent_id: "self_adaptation".to_string(),
            kind: ProposalKind::RecordIssue,
            summary: format!(
                "Adaptation strategy {} ({}): {} needs an operator",
                strategy.id,
                strategy.description,
                action.describe()
            ),
            details: serde_json::json!({
                "source": "self_adaptation",
                "strategy_id": strategy.id,
                "strategy_description": strategy.description,
                "action": action.kind(),
                "value": action.value(),
                "risk_level": format!("{:?}", strategy.risk_level),
                "expected_benefit": strategy.expected_benefit,
                "no_automatic_remediation": reason,
            }),
            confidence: 1.0,
            requires_approval: true,
            created_ms: now_ms(),
            status: ProposalStatus::Pending,
        };
        self.store.save_proposal(&proposal)?;
        Ok(proposal.id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{KnowledgeStatus, MemoryEntry, MemoryLayer};

    fn engine(store: EvolutionStore) -> SelfAdaptationEngine {
        SelfAdaptationEngine::new(store)
    }

    fn memory(id: &str, confidence: f32) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            layer: MemoryLayer::Semantic,
            scope: "global".to_string(),
            key: "dup".to_string(),
            value: format!("v-{id}"),
            status: KnowledgeStatus::Observed,
            confidence,
            source_experience: None,
            provenance: "test".to_string(),
            created_ms: now_ms(),
            updated_ms: now_ms(),
            expires_ms: None,
            superseded_by: None,
        }
    }

    fn strategy(actions: Vec<AdaptationAction>) -> AdaptationStrategy {
        AdaptationStrategy {
            id: gen_id("strategy"),
            description: "test strategy".to_string(),
            actions,
            expected_benefit: "test".to_string(),
            estimated_cost: "test".to_string(),
            risk_level: RiskLevel::Low,
        }
    }

    #[test]
    fn consolidate_memory_is_a_real_applied_action() {
        let store = EvolutionStore::open_in_memory().unwrap();
        store.save_memory(&memory("mem-a", 0.4)).unwrap();
        store.save_memory(&memory("mem-b", 0.9)).unwrap();
        assert_eq!(store.count_memory().unwrap(), 2);

        let mut engine = engine(store.clone());
        let result = engine
            .apply_adaptation(&strategy(vec![AdaptationAction::ConsolidateMemory]))
            .unwrap();

        assert!(result.applied);
        assert!(result.not_applied.is_empty());
        assert!(result.review_proposal_ids.is_empty());
        // The action really mutated the store: one duplicate superseded.
        assert_eq!(store.count_memory().unwrap(), 1);
        let events = store.list_events(None, 10).unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.kind == "adaptation" && e.summary.contains("consolidation"))
        );
    }

    #[test]
    fn non_owned_actions_still_error_and_become_proposals() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let mut engine = engine(store.clone());
        let result = engine
            .apply_adaptation(&strategy(vec![AdaptationAction::SwitchModel(
                "gpt-4".to_string(),
            )]))
            .unwrap();

        assert!(!result.applied);
        assert_eq!(result.not_applied.len(), 1);
        assert_eq!(result.review_proposal_ids.len(), 1);
        let proposal = store
            .get_proposal(&result.review_proposal_ids[0])
            .unwrap()
            .unwrap();
        assert_eq!(proposal.status, ProposalStatus::Pending);
    }

    #[test]
    fn adaptation_history_is_durable() {
        let store = EvolutionStore::open_in_memory().unwrap();
        {
            let mut engine = engine(store.clone());
            engine
                .apply_adaptation(&strategy(vec![AdaptationAction::ConsolidateMemory]))
                .unwrap();
        }
        let mut engine = engine(store);
        engine
            .apply_adaptation(&strategy(vec![AdaptationAction::ConsolidateMemory]))
            .unwrap();
        // Hydration on the second apply must have loaded the first result.
        assert_eq!(engine.get_adaptation_history().len(), 2);
    }
}
