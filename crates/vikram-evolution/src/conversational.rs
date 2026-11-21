//! Conversational Control: Natural language interface for evolution control.
//!
//! PARTIALLY LIVE: `vikram evolution ask` parses user text through
//! [`ConversationalControl::parse_command`] and answers through
//! [`ConversationalControl::handle_command`]. Query-style commands
//! (status, self-analysis, weaknesses, improvement history, provenance,
//! action explanation) answer from the shared [`EvolutionStore`] — counts,
//! proposal/skill-version rows, learning events — and from read-only
//! `MapekLoop`/`SelfImprovementEngine` probes. Commands that would mutate
//! state the store cannot express (memory consolidation, model routing,
//! workflow learning, undo) return `success: false` with the honest reason
//! and the real next step instead of a fabricated outcome.
//!
//! `autonomous_enabled` and `command_history` remain process-local: the
//! flag is only readable via [`ConversationalControl::is_autonomous_enabled`]
//! — no scheduler consults it — and history is a session log, not durable
//! state.

use crate::error::{EvolutionError, Result};
use crate::mapek::{HealthStatus, MapekLoop};
use crate::self_improvement::SelfImprovementEngine;
use crate::store::EvolutionStore;
use crate::types::{CandidateStatus, now_ms};
use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};

/// Conversational command types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationalCommand {
    AnalyzeYourself,
    WhatCanYouImprove,
    FindYourWeaknesses,
    ImproveMemorySystem,
    OptimizeModelSelection,
    ReviewAgentArchitecture,
    LearnCodingWorkflow,
    WhatHaveYouImprovedRecently,
    WhyDidYouChangeSetting { setting: String },
    UndoLastChange,
    StopAutonomousImprovements,
    ResumeAutonomousManagement,
    ExplainAction { action_id: String },
    ReportImprovements,
}

/// Command response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandResponse {
    pub command: ConversationalCommand,
    pub success: bool,
    pub message: String,
    pub data: Option<serde_json::Value>,
    pub timestamp_ms: u64,
}

pub struct ConversationalControl {
    store: EvolutionStore,
    autonomous_enabled: bool,
    command_history: Vec<CommandResponse>,
}

impl ConversationalControl {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            autonomous_enabled: true,
            command_history: Vec::new(),
        }
    }

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Handle a natural language command.
    pub fn handle_command(&mut self, command: ConversationalCommand) -> Result<CommandResponse> {
        let response = match &command {
            ConversationalCommand::AnalyzeYourself => self.analyze_yourself(),
            ConversationalCommand::WhatCanYouImprove => self.what_can_you_improve(),
            ConversationalCommand::FindYourWeaknesses => self.find_your_weaknesses(),
            ConversationalCommand::ImproveMemorySystem => self.improve_memory_system(),
            ConversationalCommand::OptimizeModelSelection => self.optimize_model_selection(),
            ConversationalCommand::ReviewAgentArchitecture => self.review_agent_architecture(),
            ConversationalCommand::LearnCodingWorkflow => self.learn_coding_workflow(),
            ConversationalCommand::WhatHaveYouImprovedRecently => {
                self.what_have_you_improved_recently()
            }
            ConversationalCommand::WhyDidYouChangeSetting { setting } => {
                self.why_did_you_change_setting(setting)
            }
            ConversationalCommand::UndoLastChange => self.undo_last_change(),
            ConversationalCommand::StopAutonomousImprovements => {
                self.stop_autonomous_improvements()
            }
            ConversationalCommand::ResumeAutonomousManagement => {
                self.resume_autonomous_management()
            }
            ConversationalCommand::ExplainAction { action_id } => self.explain_action(action_id),
            ConversationalCommand::ReportImprovements => self.report_improvements(),
        }?;

        self.command_history.push(response.clone());
        Ok(response)
    }

    /// Parse a natural language string into a command.
    pub fn parse_command(&self, input: &str) -> Option<ConversationalCommand> {
        let input_lower = input.to_lowercase();

        if input_lower.contains("analyze yourself") || input_lower.contains("self-analysis") {
            Some(ConversationalCommand::AnalyzeYourself)
        } else if input_lower.contains("what can you improve") {
            Some(ConversationalCommand::WhatCanYouImprove)
        } else if input_lower.contains("find your weaknesses") {
            Some(ConversationalCommand::FindYourWeaknesses)
        } else if input_lower.contains("improve your memory") {
            Some(ConversationalCommand::ImproveMemorySystem)
        } else if input_lower.contains("optimize your model") {
            Some(ConversationalCommand::OptimizeModelSelection)
        } else if input_lower.contains("review your agent") {
            Some(ConversationalCommand::ReviewAgentArchitecture)
        } else if input_lower.contains("learn coding workflow") {
            Some(ConversationalCommand::LearnCodingWorkflow)
        } else if input_lower.contains("what have you improved") {
            Some(ConversationalCommand::WhatHaveYouImprovedRecently)
        } else if input_lower.contains("why did you change") {
            let setting = input
                .split("change")
                .nth(1)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            Some(ConversationalCommand::WhyDidYouChangeSetting { setting })
        } else if input_lower.contains("undo last change") {
            Some(ConversationalCommand::UndoLastChange)
        } else if input_lower.contains("stop autonomous") {
            Some(ConversationalCommand::StopAutonomousImprovements)
        } else if input_lower.contains("resume autonomous") {
            Some(ConversationalCommand::ResumeAutonomousManagement)
        } else if input_lower.contains("explain action") {
            let action_id = input
                .split("action")
                .nth(1)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            Some(ConversationalCommand::ExplainAction { action_id })
        } else if input_lower.contains("report improvements") {
            Some(ConversationalCommand::ReportImprovements)
        } else {
            None
        }
    }

    /// Store-derived tallies shared by the status/report-style answers.
    /// Every number comes from a row that exists; nothing is assumed.
    fn store_snapshot(&self) -> Result<serde_json::Value> {
        let proposals = self.store.proposal_stats()?;
        let reflection = self.store.reflection_stats()?;
        Ok(serde_json::json!({
            "experiences": self.store.count_experiences()?,
            "live_memories": self.store.count_memory()?,
            "learning_events": self.store.count_events()?,
            "self_check_events": self.store.count_events_of_kind("self_check")?,
            "proposals": {
                "total": proposals.total,
                "pending": proposals.pending,
                "approved": proposals.approved,
                "rejected": proposals.rejected,
                "applied": proposals.applied,
                "pending_record_issue": proposals.pending_record_issue,
            },
            "reflection_jobs": {
                "total": reflection.total,
                "pending": reflection.pending,
                "running": reflection.running,
                "done": reflection.done,
                "failed": reflection.failed,
            },
            "skills_with_versions": self.store.latest_skill_versions()?.len(),
        }))
    }

    /// Run the read-only MAPE-K monitor/analyze pair over the store.
    /// `monitor`/`analyze` persist nothing.
    fn mapek_monitor_analyze(
        &self,
    ) -> Result<(crate::mapek::MonitoringData, crate::mapek::AnalysisResult)> {
        let mut mapek = MapekLoop::new(self.store.clone())?;
        let data = mapek.monitor()?;
        let analysis = mapek.analyze(&data)?;
        Ok((data, analysis))
    }

    fn analyze_yourself(&self) -> Result<CommandResponse> {
        let (monitoring, analysis) = self.mapek_monitor_analyze()?;
        let health = monitoring.runtime_health.overall_status;
        let health_json = serde_json::to_value(health)
            .map_err(|e| EvolutionError::Serde(format!("health status: {e}")))?;
        let unverified = if health == HealthStatus::Unknown {
            " — unverified: too little recorded activity"
        } else {
            ""
        };
        Ok(CommandResponse {
            command: ConversationalCommand::AnalyzeYourself,
            success: true,
            message: format!(
                "Self-analysis complete: runtime health is {health:?}{unverified}; {} issue(s) identified from recorded store data.",
                analysis.issues.len()
            ),
            data: Some(serde_json::json!({
                "runtime_health": health_json,
                "issues": analysis.issues,
                "store": self.store_snapshot()?,
            })),
            timestamp_ms: now_ms(),
        })
    }

    fn what_can_you_improve(&self) -> Result<CommandResponse> {
        let mut engine = SelfImprovementEngine::new(self.store.clone())?;
        let opportunities = engine.identify_improvement_opportunities()?;
        let prioritized = engine.prioritize_improvements(&opportunities);
        let note = engine
            .last_opportunity_note()
            .unwrap_or_default()
            .to_string();
        let message = if prioritized.is_empty() {
            format!("No improvement opportunities right now. {note}")
        } else {
            format!(
                "{} improvement opportunit(y/ies) derived from recorded failure evidence.",
                prioritized.len()
            )
        };
        Ok(CommandResponse {
            command: ConversationalCommand::WhatCanYouImprove,
            success: true,
            message,
            data: Some(serde_json::json!({
                "opportunities": prioritized,
                "note": note,
            })),
            timestamp_ms: now_ms(),
        })
    }

    fn find_your_weaknesses(&self) -> Result<CommandResponse> {
        let (_, analysis) = self.mapek_monitor_analyze()?;
        let mut engine = SelfImprovementEngine::new(self.store.clone())?;
        let opportunities = engine.identify_improvement_opportunities()?;
        let note = engine
            .last_opportunity_note()
            .unwrap_or_default()
            .to_string();

        let mut weaknesses: Vec<serde_json::Value> = analysis
            .issues
            .iter()
            .map(|issue| serde_json::json!(issue))
            .collect();
        for opportunity in opportunities.iter().take(5) {
            weaknesses.push(serde_json::json!({
                "severity": format!("{:?}", opportunity.risk_level),
                "component": format!("{:?}", opportunity.area),
                "description": opportunity.description,
                "impact": opportunity.evidence.join("; "),
            }));
        }

        let message = if weaknesses.is_empty() {
            format!("No weaknesses detected from recorded data. {note}")
        } else {
            format!(
                "{} weakness(es)/opportunity area(s) derived from recorded evidence.",
                weaknesses.len()
            )
        };
        Ok(CommandResponse {
            command: ConversationalCommand::FindYourWeaknesses,
            success: true,
            message,
            data: Some(serde_json::json!({
                "weaknesses": weaknesses,
                "note": note,
            })),
            timestamp_ms: now_ms(),
        })
    }

    fn improve_memory_system(&self) -> Result<CommandResponse> {
        let memories = self.store.count_memory()?;
        Ok(CommandResponse {
            command: ConversationalCommand::ImproveMemorySystem,
            success: false,
            message: format!(
                "Not actionable from here: memory consolidation and decay run inside LayeredMemory as experiences are recorded and reflected on — there is no queued memory job this command can start. The store currently holds {memories} live memory entrie(s)."
            ),
            data: Some(serde_json::json!({ "live_memories": memories })),
            timestamp_ms: now_ms(),
        })
    }

    fn optimize_model_selection(&self) -> Result<CommandResponse> {
        let pending = self.store.proposal_stats()?.pending;
        Ok(CommandResponse {
            command: ConversationalCommand::OptimizeModelSelection,
            success: false,
            message: format!(
                "Not actionable: model selection and routing are runtime/provider concerns outside the evolution store, and Vikram has no actuator that rewrites model weights or routing tables. Routing changes reach an operator as learning proposals — {pending} proposal(s) currently pending review."
            ),
            data: Some(serde_json::json!({ "pending_proposals": pending })),
            timestamp_ms: now_ms(),
        })
    }

    fn review_agent_architecture(&self) -> Result<CommandResponse> {
        // The architecture itself is runtime configuration, not store data;
        // what the store can answer honestly is which agents and memory
        // scopes the recorded history actually involves.
        let activity = self.store.with_conn(|conn| {
            let agents = collect_rows(
                conn,
                "SELECT DISTINCT agent_id FROM experiences ORDER BY agent_id",
                [],
                |r| r.get::<_, String>(0),
            )?;
            let scopes = collect_rows(
                conn,
                "SELECT DISTINCT scope FROM memory_entries ORDER BY scope",
                [],
                |r| r.get::<_, String>(0),
            )?;
            Ok(serde_json::json!({
                "agents_in_experience": agents,
                "memory_scopes": scopes,
            }))
        })?;
        let agents = activity["agents_in_experience"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        Ok(CommandResponse {
            command: ConversationalCommand::ReviewAgentArchitecture,
            success: true,
            message: format!(
                "Agent topology is runtime configuration, not store state. Recorded history involves {agents} agent id(s); see data for the ids and memory scopes actually in use."
            ),
            data: Some(activity),
            timestamp_ms: now_ms(),
        })
    }

    fn learn_coding_workflow(&self) -> Result<CommandResponse> {
        let proposals = self.store.proposal_stats()?;
        let jobs = self.store.reflection_stats()?;
        Ok(CommandResponse {
            command: ConversationalCommand::LearnCodingWorkflow,
            success: false,
            message: format!(
                "Not directly actionable: workflow/skill learning runs through the reflection pipeline — recorded experiences become reflection jobs, and only operator-approved proposals become skill versions. Currently {} pending proposal(s) and {} pending/running reflection job(s). To teach a workflow, record experiences or review proposals via `vikram evolution`.",
                proposals.pending,
                jobs.backlog()
            ),
            data: Some(serde_json::json!({
                "pending_proposals": proposals.pending,
                "reflection_backlog": jobs.backlog(),
            })),
            timestamp_ms: now_ms(),
        })
    }

    fn what_have_you_improved_recently(&self) -> Result<CommandResponse> {
        let applied = self.store.list_proposals(Some("applied"))?;
        let skill_versions = self.store.list_recent_skill_versions(10)?;
        let promoted: Vec<_> = self
            .store
            .list_candidates(None)?
            .into_iter()
            .filter(|c| c.status == CandidateStatus::Promoted)
            .collect();
        Ok(CommandResponse {
            command: ConversationalCommand::WhatHaveYouImprovedRecently,
            success: true,
            message: format!(
                "Recorded improvements: {} applied proposal(s), {} skill version row(s), {} promoted optimization candidate(s).",
                applied.len(),
                skill_versions.len(),
                promoted.len()
            ),
            data: Some(serde_json::json!({
                "applied_proposals": applied,
                "recent_skill_versions": skill_versions,
                "promoted_candidates": promoted,
            })),
            timestamp_ms: now_ms(),
        })
    }

    fn why_did_you_change_setting(&self, setting: &str) -> Result<CommandResponse> {
        let needle = setting.to_lowercase();
        let applied = self.store.list_proposals(Some("applied"))?;
        let proposal_matches: Vec<_> = applied
            .iter()
            .filter(|p| {
                p.summary.to_lowercase().contains(&needle)
                    || p.details.to_string().to_lowercase().contains(&needle)
            })
            .collect();
        let skill_matches: Vec<_> = self
            .store
            .list_recent_skill_versions(100)?
            .into_iter()
            .filter(|v| {
                v.change_description.to_lowercase().contains(&needle)
                    || v.skill_name.to_lowercase().contains(&needle)
            })
            .collect();

        let found = !proposal_matches.is_empty() || !skill_matches.is_empty();
        let message = if setting.trim().is_empty() {
            "No setting was named — ask e.g. \"why did you change memory consolidation\"."
                .to_string()
        } else if found {
            format!(
                "Found {} applied proposal(s) and {} skill version(s) mentioning '{setting}'.",
                proposal_matches.len(),
                skill_matches.len()
            )
        } else {
            format!(
                "No applied proposal or skill version recorded in the store mentions '{setting}'. Changes that never reached `applied`/versioned state leave no provenance here."
            )
        };
        Ok(CommandResponse {
            command: ConversationalCommand::WhyDidYouChangeSetting {
                setting: setting.to_string(),
            },
            success: true,
            message,
            data: Some(serde_json::json!({
                "setting": setting,
                "matching_proposals": proposal_matches,
                "matching_skill_versions": skill_matches,
            })),
            timestamp_ms: now_ms(),
        })
    }

    fn undo_last_change(&self) -> Result<CommandResponse> {
        // Revert paths exist per artifact (SkillEvolution::rollback,
        // OptimizationEngine::rollback) but nothing identifies "the last
        // change" unambiguously, so this interface refuses to guess.
        let applied = self.store.list_proposals(Some("applied"))?;
        Ok(CommandResponse {
            command: ConversationalCommand::UndoLastChange,
            success: false,
            message:
                "No generic undo: applied proposals and skill versions are the change record, and reverting the wrong one is worse than none. Skill versions restore via SkillEvolution::rollback(name, version) and candidates via OptimizationEngine::rollback(id) — name the artifact to revert it."
                    .to_string(),
            data: Some(serde_json::json!({
                "most_recent_applied_proposal": applied.first(),
            })),
            timestamp_ms: now_ms(),
        })
    }

    fn stop_autonomous_improvements(&mut self) -> Result<CommandResponse> {
        self.autonomous_enabled = false;
        Ok(CommandResponse {
            command: ConversationalCommand::StopAutonomousImprovements,
            success: true,
            message: "Autonomous-improvement flag paused for this process. Note: no scheduler currently consults this flag — approval gating lives in AutonomyManager and is unaffected.".to_string(),
            data: None,
            timestamp_ms: now_ms(),
        })
    }

    fn resume_autonomous_management(&mut self) -> Result<CommandResponse> {
        self.autonomous_enabled = true;
        Ok(CommandResponse {
            command: ConversationalCommand::ResumeAutonomousManagement,
            success: true,
            message: "Autonomous-improvement flag resumed for this process.".to_string(),
            data: None,
            timestamp_ms: now_ms(),
        })
    }

    fn explain_action(&self, action_id: &str) -> Result<CommandResponse> {
        let events = if action_id.is_empty() {
            Vec::new()
        } else {
            self.store.search_events(action_id, 10)?
        };
        let proposal = if action_id.is_empty() {
            None
        } else {
            self.store.get_proposal(action_id)?
        };
        let candidate = if action_id.is_empty() {
            None
        } else {
            self.store.get_candidate(action_id)?
        };

        let found = !events.is_empty() || proposal.is_some() || candidate.is_some();
        let message = if action_id.trim().is_empty() {
            "No action id given — ask e.g. \"explain action <id>\".".to_string()
        } else if found {
            format!("Found recorded trace(s) for action '{action_id}'.")
        } else {
            format!(
                "No learning event, proposal or optimization candidate in the store matches '{action_id}'."
            )
        };
        Ok(CommandResponse {
            command: ConversationalCommand::ExplainAction {
                action_id: action_id.to_string(),
            },
            success: true,
            message,
            data: Some(serde_json::json!({
                "action_id": action_id,
                "learning_events": events,
                "proposal": proposal,
                "candidate": candidate,
            })),
            timestamp_ms: now_ms(),
        })
    }

    fn report_improvements(&self) -> Result<CommandResponse> {
        let store = self.store_snapshot()?;
        let improvement = SelfImprovementEngine::new(self.store.clone())?;
        let executions = improvement.get_improvement_history();
        let completed = executions
            .iter()
            .filter(|e| e.status == crate::self_improvement::ExecutionStatus::Completed)
            .count();
        let failed = executions
            .iter()
            .filter(|e| e.status == crate::self_improvement::ExecutionStatus::Failed)
            .count();
        Ok(CommandResponse {
            command: ConversationalCommand::ReportImprovements,
            success: true,
            message: format!(
                "Improvement report from recorded state: {} applied proposal(s), {} pending proposal(s), {} skill(s) with version history, {} improvement execution(s) ({} completed, {} failed).",
                store["proposals"]["applied"],
                store["proposals"]["pending"],
                store["skills_with_versions"],
                executions.len(),
                completed,
                failed
            ),
            data: Some(serde_json::json!({
                "store": store,
                "improvement_executions": {
                    "total": executions.len(),
                    "completed": completed,
                    "failed": failed,
                },
            })),
            timestamp_ms: now_ms(),
        })
    }

    /// Check if autonomous improvements are enabled.
    pub fn is_autonomous_enabled(&self) -> bool {
        self.autonomous_enabled
    }

    /// Get command history.
    pub fn get_command_history(&self) -> &[CommandResponse] {
        &self.command_history
    }
}

/// Collect `query_map` rows on an already-locked connection, mapping store
/// errors the same way [`EvolutionStore`] does.
fn collect_rows<T>(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    let rows = stmt
        .query_map(p, map)
        .map_err(|e| EvolutionError::Store(e.to_string()))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| EvolutionError::Store(e.to_string()))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn control() -> (ConversationalControl, EvolutionStore) {
        let store = EvolutionStore::open_in_memory().unwrap();
        let control = ConversationalControl::new(store.clone());
        (control, store)
    }

    fn seed_experience(store: &EvolutionStore, id: &str, outcome: ExperienceOutcome) {
        store
            .save_experience(&Experience {
                id: id.to_string(),
                timestamp_ms: now_ms(),
                agent_id: "agent-1".to_string(),
                project_id: None,
                task_category: "coding".to_string(),
                user_request: "do a thing".to_string(),
                outcome,
                tools_used: vec![],
                errors: vec![],
                corrections: vec![],
                feedback: None,
                reusable: false,
                sensitivity: Sensitivity::Public,
                metadata: serde_json::json!({}),
            })
            .unwrap();
    }

    fn seed_memory(store: &EvolutionStore, id: &str) {
        store
            .save_memory(&MemoryEntry {
                id: id.to_string(),
                layer: MemoryLayer::Semantic,
                scope: "global".to_string(),
                key: format!("key-{id}"),
                value: "value".to_string(),
                status: KnowledgeStatus::Observed,
                confidence: 0.9,
                source_experience: None,
                provenance: "test".to_string(),
                created_ms: now_ms(),
                updated_ms: now_ms(),
                expires_ms: None,
                superseded_by: None,
            })
            .unwrap();
    }

    fn seed_proposal(store: &EvolutionStore, id: &str, status: ProposalStatus, summary: &str) {
        store
            .save_proposal(&LearningProposal {
                id: id.to_string(),
                experience_id: None,
                agent_id: "agent-1".to_string(),
                kind: ProposalKind::ImproveSkill,
                summary: summary.to_string(),
                details: serde_json::json!({}),
                confidence: 0.8,
                requires_approval: true,
                created_ms: now_ms(),
                status,
            })
            .unwrap();
    }

    fn seed_skill_version(store: &EvolutionStore, name: &str, version: u32, desc: &str) {
        store
            .save_skill_version(&SkillVersion {
                skill_name: name.to_string(),
                version,
                content: "# skill".to_string(),
                created_ms: now_ms(),
                parent_version: (version > 1).then_some(version - 1),
                change_description: desc.to_string(),
            })
            .unwrap();
    }

    #[test]
    fn report_improvements_reports_real_store_counts() {
        let (mut control, store) = control();
        seed_experience(&store, "exp-1", ExperienceOutcome::Success);
        seed_experience(&store, "exp-2", ExperienceOutcome::Failure);
        seed_memory(&store, "mem-1");
        seed_proposal(
            &store,
            "prop-1",
            ProposalStatus::Applied,
            "improved skill x",
        );
        seed_proposal(&store, "prop-2", ProposalStatus::Pending, "pending idea");
        seed_skill_version(&store, "demo", 1, "initial version");
        store
            .save_event(&LearningEvent {
                id: "evt-1".to_string(),
                timestamp_ms: now_ms(),
                agent_id: "self-check".to_string(),
                kind: "self_check".to_string(),
                summary: "self check".to_string(),
                payload: serde_json::json!({}),
            })
            .unwrap();

        let response = control
            .handle_command(ConversationalCommand::ReportImprovements)
            .unwrap();
        assert!(response.success);
        let data = response.data.unwrap();
        assert_eq!(data["store"]["experiences"], 2);
        assert_eq!(data["store"]["live_memories"], 1);
        assert_eq!(data["store"]["proposals"]["applied"], 1);
        assert_eq!(data["store"]["proposals"]["pending"], 1);
        assert_eq!(data["store"]["self_check_events"], 1);
        assert_eq!(data["store"]["skills_with_versions"], 1);
        assert!(response.message.contains("1 applied proposal"));
    }

    #[test]
    fn improved_recently_lists_applied_proposals_and_skill_versions() {
        let (mut control, store) = control();
        seed_proposal(&store, "prop-1", ProposalStatus::Applied, "applied change");
        seed_proposal(&store, "prop-2", ProposalStatus::Pending, "not applied");
        seed_skill_version(&store, "demo", 2, "tweak retrieval");

        let response = control
            .handle_command(ConversationalCommand::WhatHaveYouImprovedRecently)
            .unwrap();
        let data = response.data.unwrap();
        let proposals = data["applied_proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0]["id"], "prop-1");
        let versions = data["recent_skill_versions"].as_array().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0]["skill_name"], "demo");
        assert_eq!(versions[0]["version"], 2);
    }

    #[test]
    fn what_can_you_improve_is_empty_with_an_empty_store() {
        let (mut control, _) = control();
        let response = control
            .handle_command(ConversationalCommand::WhatCanYouImprove)
            .unwrap();
        assert!(response.success);
        let data = response.data.unwrap();
        assert_eq!(data["opportunities"].as_array().unwrap().len(), 0);
        assert!(
            data["note"]
                .as_str()
                .unwrap()
                .contains("no improvement opportunities")
        );
    }

    #[test]
    fn what_can_you_improve_derives_opportunities_from_failures() {
        let (mut control, store) = control();
        for i in 0..6 {
            seed_experience(&store, &format!("exp-{i}"), ExperienceOutcome::Failure);
        }
        let response = control
            .handle_command(ConversationalCommand::WhatCanYouImprove)
            .unwrap();
        let data = response.data.unwrap();
        let opportunities = data["opportunities"].as_array().unwrap();
        assert!(!opportunities.is_empty());
        assert!(response.message.contains("improvement opportunit"));
    }

    #[test]
    fn unsupported_commands_fail_honestly() {
        let (mut control, _) = control();
        for command in [
            ConversationalCommand::ImproveMemorySystem,
            ConversationalCommand::OptimizeModelSelection,
            ConversationalCommand::LearnCodingWorkflow,
            ConversationalCommand::UndoLastChange,
        ] {
            let response = control.handle_command(command.clone()).unwrap();
            assert!(!response.success, "{command:?} must not claim success");
            assert!(
                response.message.contains("actionable")
                    || response.message.contains("No generic undo"),
                "{command:?}: {}",
                response.message
            );
        }
    }

    #[test]
    fn explain_action_finds_recorded_events() {
        let (mut control, store) = control();
        store
            .save_event(&LearningEvent {
                id: "evt-1".to_string(),
                timestamp_ms: now_ms(),
                agent_id: "agent-1".to_string(),
                kind: "action_executed".to_string(),
                summary: "executed action act-42".to_string(),
                payload: serde_json::json!({"action_id": "act-42"}),
            })
            .unwrap();

        let response = control
            .handle_command(ConversationalCommand::ExplainAction {
                action_id: "act-42".to_string(),
            })
            .unwrap();
        assert!(response.success);
        let data = response.data.unwrap();
        assert_eq!(
            data["learning_events"].as_array().unwrap().len(),
            1,
            "the seeded event must be found: {}",
            response.message
        );

        let missing = control
            .handle_command(ConversationalCommand::ExplainAction {
                action_id: "nope-1".to_string(),
            })
            .unwrap();
        assert!(missing.message.contains("No learning event"));
    }

    #[test]
    fn why_did_you_change_finds_provenance() {
        let (mut control, store) = control();
        seed_proposal(
            &store,
            "prop-1",
            ProposalStatus::Applied,
            "tuned memory consolidation window",
        );
        seed_proposal(
            &store,
            "prop-2",
            ProposalStatus::Applied,
            "unrelated change",
        );
        seed_skill_version(&store, "demo", 1, "first cut");
        seed_skill_version(&store, "demo", 2, "memory consolidation tuning");

        let response = control
            .handle_command(ConversationalCommand::WhyDidYouChangeSetting {
                setting: "memory consolidation".to_string(),
            })
            .unwrap();
        let data = response.data.unwrap();
        assert_eq!(data["matching_proposals"].as_array().unwrap().len(), 1);
        assert_eq!(data["matching_skill_versions"].as_array().unwrap().len(), 1);
        assert!(response.message.contains("Found"));

        let none = control
            .handle_command(ConversationalCommand::WhyDidYouChangeSetting {
                setting: "nonexistent knob".to_string(),
            })
            .unwrap();
        assert!(none.message.contains("No applied proposal"));
    }

    #[test]
    fn analyze_yourself_reports_unverified_health_on_empty_store() {
        let (mut control, _) = control();
        let response = control
            .handle_command(ConversationalCommand::AnalyzeYourself)
            .unwrap();
        assert!(response.success);
        assert!(response.message.contains("unverified"));
        let data = response.data.unwrap();
        assert_eq!(data["runtime_health"], "Unknown");
        assert_eq!(data["store"]["experiences"], 0);
    }

    #[test]
    fn review_agent_architecture_lists_real_agent_ids() {
        let (mut control, store) = control();
        seed_experience(&store, "exp-1", ExperienceOutcome::Success);
        let response = control
            .handle_command(ConversationalCommand::ReviewAgentArchitecture)
            .unwrap();
        let data = response.data.unwrap();
        assert_eq!(data["agents_in_experience"], serde_json::json!(["agent-1"]));
    }
}
