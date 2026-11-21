//! Read-only aggregate view over the [`EvolutionStore`].
//!
//! LIVE via CLI: [`Dashboard::snapshot`] is a pure store read reachable
//! through `vikram evolution status`. It mutates nothing.

use crate::error::Result;
use crate::metrics::{AgentTrend, PerformanceDimension, PerformanceStats, TREND_WINDOW_MS};
use crate::store::EvolutionStore;
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct LearningDashboard {
    pub experiences: usize,
    pub memories: usize,
    pub proposals_pending: usize,
    pub proposals_total: usize,
    pub candidates: usize,
    pub hive_lessons: usize,
    pub learning_events: usize,
    pub reflection_jobs_pending: usize,
    /// Top agents by recorded experience volume, with outcome rates, mean
    /// recorded duration, top error strings and tool usage. Empty when no
    /// experiences exist.
    pub agent_performance: Vec<PerformanceStats>,
    /// Same aggregates grouped by `task_category`.
    pub category_performance: Vec<PerformanceStats>,
    /// Per-agent success-rate trend: the last [`TREND_WINDOW_MS`] versus the
    /// equally long window before it. `InsufficientData` unless both windows
    /// hold at least `MIN_SAMPLE` experiences.
    pub agent_trends: Vec<AgentTrend>,
}

pub struct Dashboard {
    store: EvolutionStore,
}

impl Dashboard {
    pub fn new(store: EvolutionStore) -> Self {
        Self { store }
    }
    /// Accessor for the underlying store (shares the same `Arc`).
    pub fn store(&self) -> EvolutionStore {
        self.store.clone()
    }
    pub fn snapshot(&self) -> Result<LearningDashboard> {
        use crate::types::ProposalStatus;
        let proposals = self.store.list_proposals(None)?;
        let pending = proposals
            .iter()
            .filter(|p| p.status == ProposalStatus::Pending)
            .count();
        let jobs = self.store.list_pending_jobs()?;
        Ok(LearningDashboard {
            experiences: self.store.count_experiences()?,
            memories: self.store.count_memory()?,
            proposals_pending: pending,
            proposals_total: proposals.len(),
            candidates: self.store.list_candidates(None)?.len(),
            hive_lessons: self.store.list_hive_lessons()?.len(),
            learning_events: self.store.count_events()?,
            reflection_jobs_pending: jobs.len(),
            agent_performance: self.store.performance_stats(
                PerformanceDimension::Agent,
                None,
                10,
            )?,
            category_performance: self.store.performance_stats(
                PerformanceDimension::Category,
                None,
                10,
            )?,
            agent_trends: self.store.agent_trends(TREND_WINDOW_MS)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Experience, ExperienceOutcome, Sensitivity, now_ms};

    fn seed(store: &EvolutionStore, id: &str, agent: &str, category: &str) {
        store
            .save_experience(&Experience {
                id: id.to_string(),
                timestamp_ms: now_ms(),
                agent_id: agent.to_string(),
                project_id: None,
                task_category: category.to_string(),
                user_request: "req".to_string(),
                outcome: ExperienceOutcome::Success,
                tools_used: vec!["read".to_string()],
                errors: vec![],
                corrections: vec![],
                feedback: None,
                reusable: false,
                sensitivity: Sensitivity::Public,
                metadata: serde_json::json!({}),
            })
            .unwrap();
    }

    #[test]
    fn snapshot_reports_empty_performance_on_empty_store() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let dash = Dashboard::new(store).snapshot().unwrap();
        assert!(dash.agent_performance.is_empty());
        assert!(dash.category_performance.is_empty());
    }

    #[test]
    fn snapshot_includes_agent_and_category_performance() {
        let store = EvolutionStore::open_in_memory().unwrap();
        seed(&store, "e1", "agent-1", "coding");
        seed(&store, "e2", "agent-1", "coding");
        seed(&store, "e3", "agent-2", "shell");
        let dash = Dashboard::new(store).snapshot().unwrap();
        assert_eq!(dash.agent_performance.len(), 2);
        assert_eq!(dash.agent_performance[0].key, "agent-1");
        assert_eq!(dash.agent_performance[0].total, 2);
        assert_eq!(dash.agent_performance[0].success_rate(), Some(1.0));
        assert_eq!(dash.agent_performance[0].tool_usage[0].name, "read");
        assert_eq!(dash.category_performance.len(), 2);
        assert_eq!(dash.category_performance[0].key, "coding");
        assert_eq!(dash.category_performance[0].total, 2);
        // All seeds are "now": both trend windows are too thin for a verdict.
        assert_eq!(dash.agent_trends.len(), 2);
        assert!(
            dash.agent_trends
                .iter()
                .all(|t| t.trend == crate::metrics::TrendDirection::InsufficientData)
        );
    }

    #[test]
    fn snapshot_reports_improving_trend_from_seeded_windows() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let now = now_ms();
        let w = crate::metrics::TREND_WINDOW_MS;
        let prior = now - w - 1_000;
        for i in 0..5 {
            let mut e = Experience {
                id: format!("r{i}"),
                timestamp_ms: now,
                agent_id: "agent-1".to_string(),
                project_id: None,
                task_category: "coding".to_string(),
                user_request: "req".to_string(),
                outcome: ExperienceOutcome::Success,
                tools_used: vec![],
                errors: vec![],
                corrections: vec![],
                feedback: None,
                reusable: false,
                sensitivity: Sensitivity::Public,
                metadata: serde_json::json!({}),
            };
            store.save_experience(&e).unwrap();
            e.id = format!("p{i}");
            e.timestamp_ms = prior;
            e.outcome = ExperienceOutcome::Failure;
            store.save_experience(&e).unwrap();
        }
        let dash = Dashboard::new(store).snapshot().unwrap();
        assert_eq!(dash.agent_trends.len(), 1);
        let trend = &dash.agent_trends[0];
        assert_eq!(trend.agent_id, "agent-1");
        assert_eq!(trend.recent_success_rate, Some(1.0));
        assert_eq!(trend.prior_success_rate, Some(0.0));
        assert_eq!(trend.trend, crate::metrics::TrendDirection::Improving);
    }
}
