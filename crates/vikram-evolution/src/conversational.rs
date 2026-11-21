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
use crate::metrics::PerformanceDimension;
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
    Ask { question: String },
    WhoIs { name: String },
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
            ConversationalCommand::Ask { question } => self.ask_freeform(question),
            ConversationalCommand::WhoIs { name } => self.who_is(name),
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
        } else if let Some(name) = who_is_name(input) {
            Some(ConversationalCommand::WhoIs { name })
        } else {
            let question = input.trim();
            if question.is_empty() {
                None
            } else {
                Some(ConversationalCommand::Ask {
                    question: question.to_string(),
                })
            }
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

    /// Free-form fallback: keyword retrieval over the store. This answers
    /// "what was recorded about X", never "why X happened" — no inference.
    fn ask_freeform(&self, question: &str) -> Result<CommandResponse> {
        let trimmed = question.trim();
        let terms = question_keywords(trimmed);
        let search_terms: Vec<String> = if terms.is_empty() && !trimmed.is_empty() {
            vec![trimmed.to_lowercase()]
        } else {
            terms
        };

        let mut events: Vec<serde_json::Value> = Vec::new();
        let mut memories: Vec<serde_json::Value> = Vec::new();
        let mut seen_events = std::collections::HashSet::new();
        let mut seen_memories = std::collections::HashSet::new();
        for term in search_terms.iter().take(6) {
            for e in self.store.search_events(term, 3)? {
                if seen_events.insert(e.id.clone()) && events.len() < 5 {
                    events.push(serde_json::json!({
                        "id": e.id,
                        "kind": e.kind,
                        "summary": truncate(&e.summary, 120),
                    }));
                }
            }
            for m in self.store.search_memory(None, term, 3)? {
                if seen_memories.insert(m.id.clone()) && memories.len() < 5 {
                    memories.push(serde_json::json!({
                        "id": m.id,
                        "key": m.key,
                        "value": truncate(&m.value, 120),
                    }));
                }
            }
        }

        let mut proposals: Vec<serde_json::Value> = Vec::new();
        let mut seen_proposals = std::collections::HashSet::new();
        for p in self.store.list_proposals(None)? {
            if proposals.len() >= 5 {
                break;
            }
            let haystack = format!(
                "{} {}",
                p.summary.to_lowercase(),
                p.details.to_string().to_lowercase()
            );
            if search_terms.iter().any(|t| haystack.contains(t))
                && seen_proposals.insert(p.id.clone())
            {
                proposals.push(serde_json::json!({
                    "id": p.id,
                    "summary": truncate(&p.summary, 120),
                    "status": format!("{:?}", p.status),
                }));
            }
        }

        let mut experiences: Vec<serde_json::Value> = Vec::new();
        let mut seen_experiences = std::collections::HashSet::new();
        for e in self.store.list_experiences(None, 100)? {
            if experiences.len() >= 5 {
                break;
            }
            let haystack = format!(
                "{} {} {}",
                e.user_request.to_lowercase(),
                e.task_category.to_lowercase(),
                e.errors.join(" ").to_lowercase()
            );
            if search_terms.iter().any(|t| haystack.contains(t))
                && seen_experiences.insert(e.id.clone())
            {
                experiences.push(serde_json::json!({
                    "id": e.id,
                    "task_category": e.task_category,
                    "outcome": format!("{:?}", e.outcome),
                    "user_request": truncate(&e.user_request, 120),
                }));
            }
        }

        let performance = if wants_performance(trimmed) {
            Some(self.performance_payload(&search_terms)?)
        } else {
            None
        };

        let total = events.len() + memories.len() + proposals.len() + experiences.len();
        let message = if let Some(perf) = &performance {
            perf_message(perf, trimmed)
        } else if trimmed.is_empty() {
            "Empty question — nothing to search the store for.".to_string()
        } else if total == 0 {
            format!(
                "No recorded activity matching '{}' — the store holds nothing about those terms. This is retrieval, not inference: ask about recorded events, memories, proposals, or experiences by name.",
                trimmed
            )
        } else {
            format!(
                "Found {} related record(s) in the store for '{}': {} learning event(s), {} memor(ies), {} proposal(s), {} experience(s). These are recorded rows, not a diagnosis — see `data`.",
                total,
                trimmed,
                events.len(),
                memories.len(),
                proposals.len(),
                experiences.len()
            )
        };
        Ok(CommandResponse {
            command: ConversationalCommand::Ask {
                question: trimmed.to_string(),
            },
            success: true,
            message,
            data: Some(serde_json::json!({
                "terms": search_terms,
                "learning_events": events,
                "memories": memories,
                "proposals": proposals,
                "experiences": experiences,
                "performance": performance,
            })),
            timestamp_ms: now_ms(),
        })
    }

    /// "Who is X" answers only from recorded evolution-store rows: live
    /// memories whose key/value names X, experiences whose request,
    /// category or project names X, and learning events matching X. The
    /// runtime relation table (vikram.db, managed by vikram-store) is a
    /// different database this engine cannot open, so the reply states that
    /// scope plainly instead of implying relations were checked.
    fn who_is(&self, name: &str) -> Result<CommandResponse> {
        let trimmed = name.trim();
        let command = ConversationalCommand::WhoIs {
            name: trimmed.to_string(),
        };
        if trimmed.is_empty() {
            return Ok(CommandResponse {
                command,
                success: true,
                message:
                    "No name given — ask e.g. \"who is Mom\" or \"what do you know about Vikram CLI\"."
                        .to_string(),
                data: None,
                timestamp_ms: now_ms(),
            });
        }
        let needle = trimmed.to_lowercase();

        let memories: Vec<serde_json::Value> = self
            .store
            .search_memory(None, trimmed, 5)?
            .into_iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "layer": format!("{:?}", m.layer),
                    "key": m.key,
                    "value": truncate(&m.value, 160),
                    "status": format!("{:?}", m.status),
                    "confidence": m.confidence,
                    "updated_ms": m.updated_ms,
                })
            })
            .collect();

        let mut experiences: Vec<serde_json::Value> = Vec::new();
        for e in self.store.list_experiences(None, 200)? {
            if experiences.len() >= 5 {
                break;
            }
            let haystack = format!(
                "{} {} {}",
                e.user_request.to_lowercase(),
                e.task_category.to_lowercase(),
                e.project_id.as_deref().unwrap_or("").to_lowercase()
            );
            if haystack.contains(&needle) {
                experiences.push(serde_json::json!({
                    "id": e.id,
                    "task_category": e.task_category,
                    "outcome": format!("{:?}", e.outcome),
                    "project_id": e.project_id,
                    "user_request": truncate(&e.user_request, 120),
                    "timestamp_ms": e.timestamp_ms,
                }));
            }
        }

        let events: Vec<serde_json::Value> = self
            .store
            .search_events(trimmed, 5)?
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "kind": e.kind,
                    "summary": truncate(&e.summary, 120),
                    "timestamp_ms": e.timestamp_ms,
                })
            })
            .collect();

        let total = memories.len() + experiences.len() + events.len();
        let message = if total == 0 {
            format!(
                "Nothing recorded about '{trimmed}' — no live memory, experience or learning event in the evolution store mentions them. Runtime relations are kept in a separate store (vikram.db) that `vikram evolution` does not read."
            )
        } else {
            format!(
                "Recorded mentions of '{trimmed}': {} memor(ies), {} experience(s), {} learning event(s) — retrieval over the evolution store only; runtime relations live in vikram.db, a separate store.",
                memories.len(),
                experiences.len(),
                events.len()
            )
        };
        Ok(CommandResponse {
            command,
            success: true,
            message,
            data: Some(serde_json::json!({
                "name": trimmed,
                "memories": memories,
                "experiences": experiences,
                "learning_events": events,
            })),
            timestamp_ms: now_ms(),
        })
    }

    /// Store-backed per-agent/per-category performance for Ask questions
    /// like "how is X performing". `agents`/`categories` carry the top
    /// groups by volume; `matched_*` narrows them to keys a query term
    /// names. All numbers are SQL aggregates over recorded experiences.
    fn performance_payload(&self, terms: &[String]) -> Result<serde_json::Value> {
        let agents = self
            .store
            .performance_stats(PerformanceDimension::Agent, None, 10)?;
        let categories = self
            .store
            .performance_stats(PerformanceDimension::Category, None, 10)?;
        let matches = |key: &str| {
            let k = key.to_lowercase();
            !k.is_empty()
                && terms
                    .iter()
                    .any(|t| k == *t || k.contains(t.as_str()) || t.contains(k.as_str()))
        };
        let encode = |v: &Vec<crate::metrics::PerformanceStats>| -> Result<serde_json::Value> {
            serde_json::to_value(v)
                .map_err(|e| EvolutionError::Serde(format!("performance stats: {e}")))
        };
        let matched_agents: Vec<_> = agents.iter().filter(|s| matches(&s.key)).cloned().collect();
        let matched_categories: Vec<_> = categories
            .iter()
            .filter(|s| matches(&s.key))
            .cloned()
            .collect();
        Ok(serde_json::json!({
            "agents": encode(&agents)?,
            "categories": encode(&categories)?,
            "matched_agents": encode(&matched_agents)?,
            "matched_categories": encode(&matched_categories)?,
        }))
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

const ASK_STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "your", "yours", "was", "were", "will",
    "would", "could", "should", "have", "has", "had", "what", "when", "where", "which", "who",
    "whom", "whose", "why", "how", "did", "does", "doing", "done", "can", "may", "might", "must",
    "shall", "this", "that", "these", "those", "there", "here", "with", "from", "about", "into",
    "over", "under", "than", "then", "them", "they", "their", "theirs", "our", "ours", "out",
    "off", "any", "all", "some", "such", "own", "same", "too", "very", "just", "only", "also",
    "been", "being", "because", "before", "after", "while", "during", "between", "through",
    "please", "tell", "show", "give", "want", "need", "know", "think", "say", "said", "let", "get",
    "got", "make", "made", "use", "used", "using", "see", "seen", "look", "looking", "much",
    "many", "more", "most", "less", "least", "each", "every", "both", "few", "now", "still",
    "again", "once", "ever", "never", "always", "often", "really", "actually",
];

/// Lowercase alphanumeric tokens of length >= 3 that are not stopwords,
/// deduplicated, preserving question order.
fn question_keywords(question: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for token in question
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3 && !ASK_STOPWORDS.contains(t))
    {
        if seen.insert(token.to_string()) {
            out.push(token.to_string());
        }
    }
    out
}

/// Extract the name from a "who is X" / "who's X" / "what do you know
/// about X" question, preserving the original casing. Returns `Some("")`
/// when the prefix matched but nothing followed, so the handler can answer
/// honestly instead of falling through to generic Ask.
fn who_is_name(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let lower = trimmed.to_lowercase();
    for prefix in ["who is ", "who's ", "what do you know about "] {
        if lower.starts_with(prefix) {
            // The prefixes are pure ASCII, so the byte offset is a valid
            // char boundary in the original string too.
            let name = trimmed[prefix.len()..]
                .trim()
                .trim_end_matches(['?', '.', '!'])
                .trim()
                .to_string();
            return Some(name);
        }
    }
    None
}

/// True when a free-form question asks about agent/category performance
/// rather than plain record retrieval.
fn wants_performance(question: &str) -> bool {
    let q = question.to_lowercase();
    [
        "perform",
        "success rate",
        "failure rate",
        "track record",
        "how is ",
        "how are ",
    ]
    .iter()
    .any(|p| q.contains(p))
}

/// Human-readable summary for a `performance_payload` result; counts only,
/// the rates live in `data`.
fn perf_message(payload: &serde_json::Value, question: &str) -> String {
    let len = |k: &str| payload[k].as_array().map(|a| a.len()).unwrap_or(0);
    let (agents, categories) = (len("agents"), len("categories"));
    let matched = len("matched_agents") + len("matched_categories");
    if agents == 0 && categories == 0 {
        return "No experiences recorded yet — there is no performance data to report.".to_string();
    }
    if matched > 0 {
        let mut parts = Vec::new();
        for section in ["matched_agents", "matched_categories"] {
            if let Some(groups) = payload[section].as_array() {
                for g in groups.iter().take(3) {
                    let key = g["key"].as_str().unwrap_or("?");
                    let total = g["total"].as_u64().unwrap_or(0);
                    let pct = |field: &str| {
                        let n = g[field].as_u64().unwrap_or(0);
                        if total == 0 {
                            "n/a".to_string()
                        } else {
                            format!("{:.0}%", n as f64 * 100.0 / total as f64)
                        }
                    };
                    parts.push(format!(
                        "{key}: {total} experience(s), {} success, {} failure",
                        pct("success"),
                        pct("failure")
                    ));
                }
            }
        }
        format!(
            "Performance from recorded experiences matching '{question}': {}. Mean recorded duration, top errors and tool usage are in `data.performance` — computed only from stored outcomes, not estimated.",
            parts.join("; ")
        )
    } else {
        format!(
            "No recorded agent or task category matches '{question}'. Performance data covers {agents} agent(s) and {categories} categor(ies) — see `data.performance` for the top groups by volume."
        )
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
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

    #[test]
    fn parse_command_routes_freeform_text_to_ask() {
        let (control, _) = control();
        match control.parse_command("why did the build fail?") {
            Some(ConversationalCommand::Ask { question }) => {
                assert_eq!(question, "why did the build fail?");
            }
            other => panic!("expected Ask, got {other:?}"),
        }
        assert!(control.parse_command("").is_none());
        assert!(control.parse_command("   \n\t ").is_none());
        assert_eq!(
            control.parse_command("analyze yourself"),
            Some(ConversationalCommand::AnalyzeYourself)
        );
        assert_eq!(
            control.parse_command("what have you improved recently"),
            Some(ConversationalCommand::WhatHaveYouImprovedRecently)
        );
    }

    #[test]
    fn ask_freeform_returns_store_derived_records() {
        let (mut control, store) = control();
        store
            .save_event(&LearningEvent {
                id: "evt-build".to_string(),
                timestamp_ms: now_ms(),
                agent_id: "agent-1".to_string(),
                kind: "action_executed".to_string(),
                summary: "build failed on tests".to_string(),
                payload: serde_json::json!({}),
            })
            .unwrap();
        seed_proposal(
            &store,
            "prop-test",
            ProposalStatus::Pending,
            "fix flaky tests in build",
        );
        seed_experience(&store, "exp-1", ExperienceOutcome::Failure);

        let command = control.parse_command("why did the build fail?").unwrap();
        let response = control.handle_command(command).unwrap();
        assert!(response.success);
        assert!(response.message.contains("related record"));
        let data = response.data.unwrap();
        let events = data["learning_events"].as_array().unwrap();
        assert!(events.iter().any(|e| e["id"] == "evt-build"));
        let proposals = data["proposals"].as_array().unwrap();
        assert!(proposals.iter().any(|p| p["id"] == "prop-test"));
    }

    #[test]
    fn ask_freeform_reports_no_match_honestly() {
        let (mut control, store) = control();
        seed_proposal(
            &store,
            "prop-1",
            ProposalStatus::Applied,
            "unrelated change",
        );
        let command = control
            .parse_command("what did we work on yesterday?")
            .unwrap();
        let response = control.handle_command(command).unwrap();
        assert!(response.success);
        assert!(response.message.contains("No recorded activity matching"));
        let data = response.data.unwrap();
        assert_eq!(data["learning_events"].as_array().unwrap().len(), 0);
        assert_eq!(data["proposals"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn ask_freeform_empty_question_is_honest() {
        let (mut control, _) = control();
        let response = control
            .handle_command(ConversationalCommand::Ask {
                question: "   ".to_string(),
            })
            .unwrap();
        assert!(response.success);
        assert!(response.message.contains("Empty question"));
    }

    #[test]
    fn ask_performance_question_reports_store_aggregates() {
        let (mut control, store) = control();
        seed_experience(&store, "exp-1", ExperienceOutcome::Success);
        seed_experience(&store, "exp-2", ExperienceOutcome::Failure);

        let command = control.parse_command("how is agent-1 performing?").unwrap();
        let response = control.handle_command(command).unwrap();
        assert!(response.success);
        assert!(response.message.contains("Performance"));
        let data = response.data.unwrap();
        let perf = &data["performance"];
        let agents = perf["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["key"], "agent-1");
        assert_eq!(agents[0]["total"], 2);
        assert_eq!(agents[0]["success"], 1);
        assert_eq!(agents[0]["failure"], 1);
        assert!(response.message.contains("agent-1"));
        assert!(response.message.contains("50% success"));
        assert!(response.message.contains("50% failure"));
        let matched = perf["matched_agents"].as_array().unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0]["key"], "agent-1");
        assert_eq!(perf["categories"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn ask_performance_question_on_empty_store_is_honest() {
        let (mut control, _) = control();
        let response = control
            .handle_command(ConversationalCommand::Ask {
                question: "how is agent-9 performing?".to_string(),
            })
            .unwrap();
        assert!(response.success);
        assert!(response.message.contains("No experiences recorded"));
        let data = response.data.unwrap();
        assert_eq!(data["performance"]["agents"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn ask_performance_question_with_unknown_key_still_reports_groups() {
        let (mut control, store) = control();
        seed_experience(&store, "exp-1", ExperienceOutcome::Success);
        let response = control
            .handle_command(ConversationalCommand::Ask {
                question: "what is the success rate for nobody?".to_string(),
            })
            .unwrap();
        assert!(response.success);
        assert!(
            response
                .message
                .contains("No recorded agent or task category")
        );
        let data = response.data.unwrap();
        assert_eq!(data["performance"]["agents"].as_array().unwrap().len(), 1);
        assert_eq!(
            data["performance"]["matched_agents"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    fn seed_experience_request(store: &EvolutionStore, id: &str, request: &str) {
        store
            .save_experience(&Experience {
                id: id.to_string(),
                timestamp_ms: now_ms(),
                agent_id: "agent-1".to_string(),
                project_id: None,
                task_category: "coding".to_string(),
                user_request: request.to_string(),
                outcome: ExperienceOutcome::Success,
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

    #[test]
    fn parse_command_routes_who_is_questions() {
        let (control, _) = control();
        match control.parse_command("who is Mom?") {
            Some(ConversationalCommand::WhoIs { name }) => assert_eq!(name, "Mom"),
            other => panic!("expected WhoIs, got {other:?}"),
        }
        match control.parse_command("what do you know about Vikram CLI") {
            Some(ConversationalCommand::WhoIs { name }) => {
                assert_eq!(name, "Vikram CLI")
            }
            other => panic!("expected WhoIs, got {other:?}"),
        }
        match control.parse_command("who's dad") {
            Some(ConversationalCommand::WhoIs { name }) => assert_eq!(name, "dad"),
            other => panic!("expected WhoIs, got {other:?}"),
        }
    }

    #[test]
    fn who_is_reports_recorded_mentions_across_store_tables() {
        let (mut control, store) = control();
        store
            .save_memory(&MemoryEntry {
                id: "mem-mom".to_string(),
                layer: MemoryLayer::Semantic,
                scope: "user:u1".to_string(),
                key: "mom-preferences".to_string(),
                value: "Mom prefers window seats".to_string(),
                status: KnowledgeStatus::UserStated,
                confidence: 0.9,
                source_experience: None,
                provenance: "test".to_string(),
                created_ms: now_ms(),
                updated_ms: now_ms(),
                expires_ms: None,
                superseded_by: None,
            })
            .unwrap();
        seed_experience_request(&store, "exp-mom", "book dinner with Mom");
        seed_experience_request(&store, "exp-other", "unrelated task");
        store
            .save_event(&LearningEvent {
                id: "evt-mom".to_string(),
                timestamp_ms: now_ms(),
                agent_id: "agent-1".to_string(),
                kind: "note".to_string(),
                summary: "Mom's birthday is in June".to_string(),
                payload: serde_json::json!({}),
            })
            .unwrap();

        let response = control
            .handle_command(ConversationalCommand::WhoIs {
                name: "Mom".to_string(),
            })
            .unwrap();
        assert!(response.success);
        assert!(response.message.contains("Recorded mentions of 'Mom'"));
        let data = response.data.unwrap();
        assert_eq!(data["memories"].as_array().unwrap().len(), 1);
        assert_eq!(data["memories"][0]["id"], "mem-mom");
        let experiences = data["experiences"].as_array().unwrap();
        assert_eq!(experiences.len(), 1);
        assert_eq!(experiences[0]["id"], "exp-mom");
        assert_eq!(data["learning_events"].as_array().unwrap().len(), 1);
        assert_eq!(data["learning_events"][0]["id"], "evt-mom");
    }

    #[test]
    fn who_is_on_empty_store_is_honest() {
        let (mut control, _) = control();
        let response = control
            .handle_command(ConversationalCommand::WhoIs {
                name: "nobody".to_string(),
            })
            .unwrap();
        assert!(response.success);
        assert!(response.message.contains("Nothing recorded about 'nobody'"));
        assert!(response.message.contains("separate store"));
        let data = response.data.unwrap();
        assert_eq!(data["memories"].as_array().unwrap().len(), 0);
        assert_eq!(data["experiences"].as_array().unwrap().len(), 0);
        assert_eq!(data["learning_events"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn who_is_with_no_name_is_honest() {
        let (mut control, _) = control();
        let response = control
            .handle_command(ConversationalCommand::WhoIs {
                name: "   ".to_string(),
            })
            .unwrap();
        assert!(response.success);
        assert!(response.message.contains("No name given"));
        assert!(response.data.is_none());
    }
}
