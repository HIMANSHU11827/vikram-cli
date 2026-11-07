//! Comprehensive tests for autonomous self-evolution capabilities.

use vikram_evolution::{
    AutonomyLevel, AutonomyManager, ConversationalCommand, ConversationalControl, EvolutionEngine,
};

#[test]
fn test_self_awareness_inspection() {
    let mut engine =
        EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();
    let model = engine.self_awareness.inspect_runtime().unwrap();

    assert!(!model.architecture.crates.is_empty());
    assert!(model.capabilities.planning);
    assert!(model.capabilities.evolution);
    assert!(!model.tools.builtin.is_empty());
}

#[test]
fn test_self_awareness_capability_verification() {
    let mut engine =
        EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();
    engine.self_awareness.inspect_runtime().unwrap();

    assert!(engine.self_awareness.verify_capability("planning").unwrap());
    assert!(
        engine
            .self_awareness
            .verify_capability("evolution")
            .unwrap()
    );
    assert!(
        !engine
            .self_awareness
            .verify_capability("nonexistent")
            .unwrap()
    );
}

#[test]
fn test_self_configuration_model_selection() {
    let mut engine =
        EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();

    let criteria = vikram_evolution::ModelSelectionCriteria {
        task_type: "coding".to_string(),
        complexity: "high".to_string(),
        latency_requirement: None,
        cost_constraint: None,
        token_budget: None,
    };

    let models = vec!["gpt-4".to_string(), "gpt-3.5-turbo".to_string()];
    let selected = engine
        .self_config
        .select_model_for_task(&criteria, &models)
        .unwrap();

    assert_eq!(selected, "gpt-4");
}

#[test]
fn test_self_configuration_latency_optimization() {
    let mut engine =
        EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();

    let criteria = vikram_evolution::ModelSelectionCriteria {
        task_type: "simple".to_string(),
        complexity: "low".to_string(),
        latency_requirement: Some("low".to_string()),
        cost_constraint: None,
        token_budget: None,
    };

    let models = vec!["gpt-4".to_string(), "gpt-3.5-turbo".to_string()];
    let selected = engine
        .self_config
        .select_model_for_task(&criteria, &models)
        .unwrap();

    assert_eq!(selected, "gpt-3.5-turbo");
}

// Self-adaptation tests require additional type exports
/*
#[test]
fn test_self_adaptation_trigger_detection() {
    let engine = EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();
    let triggers = engine.self_adaptation.detect_adaptation_opportunities().unwrap();

    assert!(!triggers.is_empty());
}

#[test]
fn test_self_adaptation_strategy_generation() {
    let engine = EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();
    let trigger = vikram_evolution::AdaptationTrigger::ModelTooSlow;
    let strategies = engine.self_adaptation.generate_strategies(&trigger).unwrap();

    assert!(!strategies.is_empty());
}

#[test]
fn test_self_adaptation_strategy_selection() {
    let engine = EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();
    let trigger = vikram_evolution::AdaptationTrigger::ModelTooSlow;
    let strategies = engine.self_adaptation.generate_strategies(&trigger).unwrap();

    let constraints = vikram_evolution::AdaptationConstraints {
        max_risk: vikram_evolution::RiskLevel::Medium,
        max_cost_tokens: None,
        require_improvement: true,
    };

    let selected = engine.self_adaptation.select_strategy(&strategies, &constraints).unwrap();
    assert!(selected.is_some());
}
*/

// Additional tests require more type exports - testing core functionality for now
/*
#[test]
fn test_self_improvement_identification() {
    let mut engine = EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();
    let opportunities = engine.self_improvement.identify_improvement_opportunities().unwrap();
    assert!(!opportunities.is_empty());
}

#[test]
fn test_self_development_skill_creation() {
    let mut engine = EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();
    let project = engine.self_development.create_skill("test_skill", "# Test Skill", "test").unwrap();
    assert_eq!(project.status, vikram_evolution::ProjectStatus::InProgress);
}

#[test]
fn test_self_evaluation_benchmark_creation() {
    let mut engine = EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();
    let benchmark = engine.self_evaluation.create_benchmark(
        "Test Benchmark", "A test benchmark", "coding",
        vikram_evolution::PerformanceMetrics {
            task_completion_rate: 0.9, task_correctness: 0.85, tool_success_rate: 0.95,
            model_performance: 0.8, agent_performance: 0.85, execution_time_ms: 5000,
            token_consumption: 10000, resource_consumption: 0.7, failure_rate: 0.1,
            recovery_rate: 0.9, regression_rate: 0.05, user_corrections: 2, skill_effectiveness: 0.8,
        },
    );
    assert_eq!(benchmark.name, "Test Benchmark");
}

#[test]
fn test_self_recovery_snapshot_creation() {
    let mut engine = EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();
    let snapshot = engine.self_recovery.create_snapshot().unwrap();
    assert!(!snapshot.id.is_empty());
}

// The MAPE-K monitor/cycle assertions that used to live here are now real,
// tested behaviour below: the old commented block claimed
// `assert!(result.execution_success)` for an empty store, which is exactly the
// fake success this suite now forbids.
*/

#[test]
fn test_autonomy_level_classification() {
    let manager = AutonomyManager::new(
        EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills"))
            .unwrap()
            .store,
    );

    assert_eq!(
        manager.determine_autonomy_level(&vikram_evolution::ActionType::AdjustModelRouting),
        AutonomyLevel::Automatic
    );
    assert_eq!(
        manager.determine_autonomy_level(&vikram_evolution::ActionType::ModifySourceCode),
        AutonomyLevel::ControlledDevelopment
    );
    assert_eq!(
        manager.determine_autonomy_level(&vikram_evolution::ActionType::ChangeSecurityPolicy),
        AutonomyLevel::ApprovalRequired
    );
}

#[test]
fn test_autonomy_automatic_execution() {
    let mut manager = AutonomyManager::new(
        EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills"))
            .unwrap()
            .store,
    );

    let record = manager
        .execute_automatic(vikram_evolution::ActionType::AdjustModelRouting, "test")
        .unwrap();

    assert!(record.approved);
    assert!(record.executed);
}

#[test]
fn test_conversational_command_parsing() {
    let control = ConversationalControl::new(
        EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills"))
            .unwrap()
            .store,
    );

    assert!(control.parse_command("analyze yourself").is_some());
    assert!(control.parse_command("what can you improve").is_some());
    assert!(control.parse_command("find your weaknesses").is_some());
    assert!(control.parse_command("improve your memory").is_some());
    assert!(control.parse_command("optimize your model").is_some());
    assert!(control.parse_command("undo last change").is_some());
    assert!(control.parse_command("stop autonomous").is_some());
    assert!(control.parse_command("resume autonomous").is_some());
}

#[test]
fn test_conversational_command_execution() {
    let mut control = ConversationalControl::new(
        EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills"))
            .unwrap()
            .store,
    );

    let response = control
        .handle_command(ConversationalCommand::AnalyzeYourself)
        .unwrap();

    assert!(response.success);
    assert!(!response.message.is_empty());
}

#[test]
fn test_conversational_autonomous_control() {
    let mut control = ConversationalControl::new(
        EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills"))
            .unwrap()
            .store,
    );

    assert!(control.is_autonomous_enabled());

    control
        .handle_command(ConversationalCommand::StopAutonomousImprovements)
        .unwrap();
    assert!(!control.is_autonomous_enabled());

    control
        .handle_command(ConversationalCommand::ResumeAutonomousManagement)
        .unwrap();
    assert!(control.is_autonomous_enabled());
}

#[test]
fn test_evolution_engine_integration() {
    let mut engine =
        EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap();

    assert!(engine.self_awareness.inspect_runtime().is_ok());
    assert!(engine.mapek.run_cycle().is_ok());
    assert!(
        engine
            .self_adaptation
            .detect_adaptation_opportunities()
            .is_ok()
    );
    assert!(
        engine
            .self_improvement
            .identify_improvement_opportunities()
            .is_ok()
    );
}

#[test]
fn test_persistence_across_restarts() {
    let db_path = std::path::PathBuf::from("/tmp/test-evolution.db");
    let skills_root = std::path::PathBuf::from("/tmp/skills");

    let _ = std::fs::remove_file(&db_path);

    {
        let mut engine = EvolutionEngine::open(db_path.clone(), skills_root.clone()).unwrap();
        engine.self_awareness.inspect_runtime().unwrap();
        // Record an experience to test persistence
        let exp = vikram_evolution::types::Experience {
            id: String::new(),
            timestamp_ms: 0,
            agent_id: "test".to_string(),
            project_id: None,
            task_category: "test".to_string(),
            user_request: "test".to_string(),
            outcome: vikram_evolution::ExperienceOutcome::Success,
            tools_used: vec![],
            errors: vec![],
            corrections: vec![],
            feedback: None,
            reusable: false,
            sensitivity: vikram_evolution::Sensitivity::Public,
            metadata: serde_json::json!({}),
        };
        engine.experience.record(exp).unwrap();
    }

    {
        let engine = EvolutionEngine::open(db_path, skills_root).unwrap();
        // Experience should persist even if self-model doesn't
        let count = engine.experience.count().unwrap();
        assert!(count > 0);
    }
}

// ---------------------------------------------------------------------------
// MAPE-K / self-adaptation execution honesty
//
// `execute_action` must report `success: true` only for a verifiable side
// effect, and `Ok(())` from an adaptation must mean something observably
// happened. These tests pin both halves of that contract: a real store-backed
// remediation reports success, everything else reports failure and says why.
// ---------------------------------------------------------------------------

use vikram_evolution::mapek::{
    ActionOutcome, ActionPlan, ActionResult, ActionType as PlanActionType, PlannedAction,
    RiskLevel as PlanRiskLevel,
};
use vikram_evolution::self_adaptation::{
    AdaptationAction, AdaptationStrategy, RiskLevel as AdaptationRiskLevel,
};
use vikram_evolution::{
    AdaptationTrigger, ProposalKind, ProposalStatus, ReflectionJob, ReflectionJobStatus,
};

fn engine() -> EvolutionEngine {
    EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap()
}

fn planned(id: &str, action_type: PlanActionType, target: &str) -> PlannedAction {
    PlannedAction {
        id: id.to_string(),
        action_type,
        target: target.to_string(),
        description: format!("test action {id}"),
        parameters: serde_json::json!({}),
    }
}

fn plan_of(actions: Vec<PlannedAction>) -> ActionPlan {
    ActionPlan {
        id: "plan-test".to_string(),
        actions,
        estimated_benefit: "test".to_string(),
        estimated_cost: "test".to_string(),
        risk_level: PlanRiskLevel::Low,
        success_criteria: vec!["test".to_string()],
        timestamp_ms: 0,
    }
}

fn save_failed_job(engine: &EvolutionEngine, id: &str, attempts: u32) {
    engine
        .store
        .save_job(&ReflectionJob {
            id: id.to_string(),
            experience_id: format!("exp-{id}"),
            agent_id: "test".to_string(),
            status: ReflectionJobStatus::Failed,
            attempts,
            created_ms: 1_000,
            updated_ms: 2_000,
            error: Some("boom".to_string()),
        })
        .unwrap();
}

#[test]
fn mapek_cycle_does_not_claim_success_when_it_remediated_nothing() {
    let mut engine = engine();
    let result = engine.mapek.run_cycle().unwrap();

    // The empty store yields one "runtime unverified" issue and the loop has
    // no actuator for it, so the cycle must report failure, not success.
    assert!(!result.execution_success);
    assert_eq!(result.cycle_number, 1);
    assert_eq!(result.planned_actions, 1);
    assert_eq!(result.remediated_actions, 0);
    assert_eq!(result.recorded_for_review, 1);

    let reason = result
        .failure_reason
        .clone()
        .expect("a cycle that remediated nothing must explain itself");
    assert!(
        reason.contains("1 of 1 action(s) were not remediated"),
        "{reason}"
    );
    assert!(reason.contains("no automatic remediation"), "{reason}");

    // The escalation is real: one pending issue proposal, for an operator.
    let pending = engine.store.list_proposals(Some("pending")).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, ProposalKind::RecordIssue);
    assert_eq!(pending[0].status, ProposalStatus::Pending);
    assert!(pending[0].requires_approval);
    assert_eq!(pending[0].agent_id, "mapek");
    assert_eq!(pending[0].details["source"], "mapek");
}

#[test]
fn mapek_records_for_review_instead_of_claiming_the_action_ran() {
    let mut engine = engine();
    let execution = engine
        .mapek
        .execute(&plan_of(vec![planned(
            "action-1",
            PlanActionType::Improvement,
            "experience_pipeline",
        )]))
        .unwrap();

    assert!(!execution.overall_success);
    assert_eq!(execution.planned_actions, 1);
    assert_eq!(execution.remediated_actions, 0);
    assert_eq!(execution.recorded_for_review, 1);

    let result = &execution.action_results[0];
    assert_eq!(result.action_id, "action-1");
    assert!(!result.success);
    assert_eq!(result.outcome, ActionOutcome::RecordedForReview);
    // The output must name what happened (recording) and what did not
    // (remediation) — never the old "Action executed successfully".
    assert_eq!(
        result.output,
        "recorded action action-1 for operator review; no automatic remediation for improvement"
    );
    assert!(!result.output.contains("executed successfully"));
    let error = result.error.clone().expect("a non-success must name why");
    assert!(error.contains("no improvement actuator"), "{error}");

    // The record is verifiable through the store, and it is a record — not a
    // remediation: no experience, memory or job row was touched.
    let proposal_id = result
        .review_proposal_id
        .clone()
        .expect("the escalation must be traceable");
    let stored = engine
        .store
        .get_proposal(&proposal_id)
        .unwrap()
        .expect("the operator-review proposal must be persisted");
    assert_eq!(stored.kind, ProposalKind::RecordIssue);
    assert_eq!(stored.status, ProposalStatus::Pending);
    assert_eq!(stored.details["action_id"], "action-1");
    assert_eq!(stored.details["action_type"], "improvement");

    let reason = execution.failure_reason().unwrap();
    assert!(reason.contains("recorded for operator review"), "{reason}");
}

#[test]
fn mapek_action_with_no_possible_effect_reports_failure_and_changes_nothing() {
    let mut engine = engine();
    // A recovery action on `reflection` with no failed job to re-queue.
    let execution = engine
        .mapek
        .execute(&plan_of(vec![planned(
            "action-2",
            PlanActionType::Recovery,
            "reflection",
        )]))
        .unwrap();

    assert!(!execution.overall_success);
    assert_eq!(execution.recorded_for_review, 0);
    let result = &execution.action_results[0];
    assert!(!result.success);
    assert_eq!(result.outcome, ActionOutcome::NotExecuted);
    assert!(result.review_proposal_id.is_none());
    let error = result.error.clone().expect("a non-success must name why");
    assert!(error.contains("no failed reflection job"), "{error}");

    // Nothing was written: no proposal, no job, no experience.
    assert!(engine.store.list_proposals(None).unwrap().is_empty());
    assert!(engine.store.list_pending_jobs().unwrap().is_empty());
    assert_eq!(engine.store.count_experiences().unwrap(), 0);
}

#[test]
fn mapek_requeues_failed_reflection_jobs_as_a_real_remediation() {
    let mut engine = engine();
    save_failed_job(&engine, "job-1", 1);

    let execution = engine
        .mapek
        .execute(&plan_of(vec![planned(
            "action-3",
            PlanActionType::Recovery,
            "reflection",
        )]))
        .unwrap();

    assert!(execution.overall_success);
    assert_eq!(execution.remediated_actions, 1);
    assert_eq!(execution.recorded_for_review, 0);
    assert!(execution.failure_reason().is_none());

    let result = &execution.action_results[0];
    assert!(result.success);
    assert_eq!(result.outcome, ActionOutcome::Remediated);
    assert!(result.error.is_none());
    assert!(result.review_proposal_id.is_none());
    assert!(
        result
            .output
            .contains("re-queued 1 failed reflection job(s)"),
        "{}",
        result.output
    );

    // The side effect is verifiable in the store, not just in the report.
    assert!(engine.store.list_failed_jobs(10).unwrap().is_empty());
    let pending = engine.store.list_pending_jobs().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "job-1");
    assert_eq!(pending[0].status, ReflectionJobStatus::Pending);
    // Retry accounting is preserved rather than reset.
    assert_eq!(pending[0].attempts, 1);
    // A remediation writes no operator-review proposal.
    assert!(engine.store.list_proposals(None).unwrap().is_empty());
}

#[test]
fn mapek_escalates_jobs_that_exhausted_their_retries() {
    let mut engine = engine();
    save_failed_job(&engine, "job-capped", 3);

    let execution = engine
        .mapek
        .execute(&plan_of(vec![planned(
            "action-4",
            PlanActionType::Recovery,
            "reflection",
        )]))
        .unwrap();

    assert!(!execution.overall_success);
    let result = &execution.action_results[0];
    assert!(!result.success);
    assert_eq!(result.outcome, ActionOutcome::RecordedForReview);
    let error = result.error.clone().unwrap();
    assert!(error.contains("retry cap"), "{error}");

    // The exhausted job is left alone and the situation reaches an operator.
    let failed = engine.store.list_failed_jobs(10).unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].attempts, 3);
    assert_eq!(
        engine.store.list_proposals(Some("pending")).unwrap().len(),
        1
    );
}

#[test]
fn mapek_cycle_remediates_what_it_can_and_reports_the_rest() {
    let mut engine = engine();
    save_failed_job(&engine, "job-1", 1);

    let result = engine.mapek.run_cycle().unwrap();

    // The runtime-level issue still has no actuator, so the cycle is not a
    // success — but the reflection failure it *could* fix really was fixed.
    assert!(!result.execution_success);
    assert_eq!(result.planned_actions, 2);
    assert_eq!(result.remediated_actions, 1);
    assert_eq!(result.recorded_for_review, 1);
    assert!(engine.store.list_failed_jobs(10).unwrap().is_empty());
    let reason = result.failure_reason.expect("failure must be explained");
    assert!(
        reason.contains("1 of 2 action(s) were not remediated"),
        "{reason}"
    );
}

#[test]
fn self_adaptation_does_not_report_unapplied_strategies_as_applied() {
    let mut engine = engine();
    let strategies = engine
        .self_adaptation
        .generate_strategies(&AdaptationTrigger::ModelTooSlow)
        .unwrap();
    let strategy = strategies
        .into_iter()
        .next()
        .expect("a strategy is generated for ModelTooSlow");

    let result = engine.self_adaptation.apply_adaptation(&strategy).unwrap();

    assert!(!result.applied);
    assert!(!result.improvement_detected);
    assert!(result.after_metrics.is_none());
    assert_eq!(result.not_applied.len(), strategy.actions.len());
    assert!(
        result.not_applied[0].contains("no automatic remediation"),
        "{}",
        result.not_applied[0]
    );
    assert_eq!(result.review_proposal_ids.len(), 1);
    assert!(!engine.self_adaptation.verify_adaptation(&result).unwrap());

    // The unapplied adaptation was recorded and surfaced, not dropped.
    let stored = engine
        .store
        .get_proposal(&result.review_proposal_ids[0])
        .unwrap()
        .expect("the escalation must be persisted");
    assert_eq!(stored.kind, ProposalKind::RecordIssue);
    assert_eq!(stored.status, ProposalStatus::Pending);
    assert_eq!(stored.details["strategy_id"], strategy.id);
    assert_eq!(stored.details["action"], "switch_model");
    assert_eq!(stored.details["value"], "gpt-3.5-turbo");
    assert!(
        stored.details["no_automatic_remediation"]
            .as_str()
            .unwrap_or_default()
            .contains("no automatic remediation")
    );
}

#[test]
fn self_adaptation_records_every_unapplied_action_of_a_multi_action_strategy() {
    let mut engine = engine();
    let strategy = AdaptationStrategy {
        id: "strategy-multi".to_string(),
        description: "test strategy".to_string(),
        actions: vec![
            AdaptationAction::SwitchModel("fast-model".to_string()),
            AdaptationAction::AdjustContextBudget("compress".to_string()),
        ],
        expected_benefit: "lower latency".to_string(),
        estimated_cost: "none".to_string(),
        risk_level: AdaptationRiskLevel::Low,
    };

    let result = engine.self_adaptation.apply_adaptation(&strategy).unwrap();

    assert!(!result.applied);
    assert_eq!(result.not_applied.len(), 2);
    assert_eq!(result.review_proposal_ids.len(), 2);
    // Both requests are visible to an operator, each one traceable.
    let pending = engine.store.list_proposals(Some("pending")).unwrap();
    assert_eq!(pending.len(), 2);
    for id in &result.review_proposal_ids {
        assert!(engine.store.get_proposal(id).unwrap().is_some());
    }
}

#[test]
fn legacy_action_result_payloads_do_not_claim_remediation() {
    // A record written before `outcome`/`review_proposal_id` existed keeps its
    // stored `success` flag, but the new semantic field defaults to the
    // conservative `NotExecuted`, so no reader can mistake it for a
    // remediation it never described.
    let legacy = serde_json::json!({
        "action_id": "action-legacy",
        "success": true,
        "output": "success reported without any recorded effect",
        "error": null,
    });
    let result: ActionResult = serde_json::from_value(legacy).unwrap();
    assert_eq!(result.action_id, "action-legacy");
    assert_eq!(result.outcome, ActionOutcome::NotExecuted);
    assert!(result.review_proposal_id.is_none());
}
