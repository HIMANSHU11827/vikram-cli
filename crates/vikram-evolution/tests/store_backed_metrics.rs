//! The MAPE-K and self-* engines must report what the store actually holds.
//!
//! Every test here seeds rows into an in-memory [`EvolutionStore`] and then
//! asserts that the derived health / performance / benchmark / opportunity
//! output tracks that data — and that an empty store is reported as
//! *unverified*, never as healthy or as a plausible-looking number.

use vikram_evolution::mapek::{MonitoringData, SubsystemHealth};
use vikram_evolution::self_improvement::ImprovementArea;
use vikram_evolution::*;

fn engine() -> EvolutionEngine {
    EvolutionEngine::open_in_memory(std::path::PathBuf::from("/tmp/skills")).unwrap()
}

fn experience(
    id: &str,
    outcome: ExperienceOutcome,
    category: &str,
    errors: &[&str],
    corrections: &[&str],
    duration_ms: Option<u64>,
    tokens: Option<u64>,
) -> Experience {
    let mut metadata = serde_json::Map::new();
    if let Some(duration) = duration_ms {
        metadata.insert("duration_ms".to_string(), serde_json::json!(duration));
    }
    if let Some(tokens) = tokens {
        metadata.insert("tokens_used".to_string(), serde_json::json!(tokens));
    }
    Experience {
        id: id.to_string(),
        timestamp_ms: now_ms(),
        agent_id: "rudra".to_string(),
        project_id: None,
        task_category: category.to_string(),
        user_request: "build the feature".to_string(),
        outcome,
        tools_used: vec![],
        errors: errors.iter().map(|e| e.to_string()).collect(),
        corrections: corrections.iter().map(|c| c.to_string()).collect(),
        feedback: None,
        reusable: false,
        sensitivity: Sensitivity::Public,
        metadata: serde_json::Value::Object(metadata),
    }
}

fn save(engine: &EvolutionEngine, exp: Experience) {
    engine.store.save_experience(&exp).unwrap();
}

fn save_success(engine: &EvolutionEngine, id: &str) {
    save(
        engine,
        experience(
            id,
            ExperienceOutcome::Success,
            "coding",
            &[],
            &[],
            Some(1_000),
            Some(100),
        ),
    );
}

fn save_failure(engine: &EvolutionEngine, id: &str) {
    save(
        engine,
        experience(
            id,
            ExperienceOutcome::Failure,
            "coding",
            &["cargo build failed"],
            &[],
            Some(2_000),
            Some(200),
        ),
    );
}

fn proposal(id: &str, kind: ProposalKind, status: ProposalStatus) -> LearningProposal {
    LearningProposal {
        id: id.to_string(),
        experience_id: None,
        agent_id: "rudra".to_string(),
        kind,
        summary: "s".to_string(),
        details: serde_json::json!({}),
        confidence: 0.5,
        requires_approval: false,
        created_ms: now_ms(),
        status,
    }
}

fn subsystem<'a>(data: &'a MonitoringData, name: &str) -> &'a SubsystemHealth {
    data.runtime_health
        .subsystems
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("subsystem `{name}` missing: {:?}", data.runtime_health))
}

// ---------- MAPE-K monitoring ----------

#[test]
fn empty_store_is_unverified_not_healthy() {
    let mut engine = engine();
    let data = engine.mapek.monitor().unwrap();

    assert_eq!(data.runtime_health.overall_status, HealthStatus::Unknown);
    assert!(data.execution_outcomes.is_empty());
    assert!(data.failures.is_empty());

    // Provider health cannot be asserted without a sample.
    assert_eq!(data.provider_status.sample_count, 0);
    assert!(!data.provider_status.available);
    assert_eq!(data.provider_status.error_rate, 0.0);
    assert_eq!(data.provider_status.latency_ms, 0);
    assert!(
        data.provider_status
            .notes
            .iter()
            .any(|n| n.contains("unverified")),
        "notes must explain the missing sample: {:?}",
        data.provider_status.notes
    );

    // Resource metrics are measured (store footprint) and say where each
    // number came from; the unobservable one is not claimed.
    assert!(
        data.resource_metrics
            .notes
            .iter()
            .any(|n| n.contains("evolution store occupies")),
        "notes must record the store footprint: {:?}",
        data.resource_metrics.notes
    );
    assert!(!data.resource_metrics.network_active);
    assert!((0.0..=1.0).contains(&data.resource_metrics.disk_usage));
}

#[test]
fn health_tracks_recorded_outcomes() {
    let mut engine = engine();

    for i in 0..6 {
        save_success(&engine, &format!("ok-{i}"));
    }
    let healthy = engine.mapek.monitor().unwrap();
    assert_eq!(healthy.runtime_health.overall_status, HealthStatus::Healthy);
    assert_eq!(healthy.provider_status.sample_count, 6);
    assert!(healthy.provider_status.available);
    assert_eq!(healthy.provider_status.error_rate, 0.0);
    assert_eq!(healthy.provider_status.latency_ms, 1_000);
    assert_eq!(healthy.execution_outcomes.len(), 6);

    // The same engine, now with a failure-dominated sample: the derived
    // status must move, and the failures must show up as records.
    for i in 0..6 {
        save_failure(&engine, &format!("bad-{i}"));
    }
    let unhealthy = engine.mapek.monitor().unwrap();
    assert_eq!(
        unhealthy.runtime_health.overall_status,
        HealthStatus::Unhealthy
    );
    assert_eq!(unhealthy.provider_status.sample_count, 12);
    assert!(!unhealthy.provider_status.available);
    assert!((unhealthy.provider_status.error_rate - 0.5).abs() < 1e-6);
    assert_eq!(unhealthy.failures.len(), 6);
    assert!(
        unhealthy
            .failures
            .iter()
            .all(|f| f.component == "coding" && f.message.contains("cargo build failed")),
        "failure records must quote the stored errors: {:?}",
        unhealthy.failures
    );
}

#[test]
fn handful_of_rows_is_unverified_not_healthy() {
    let mut engine = engine();
    // Fewer than MIN_SAMPLE experiences cannot establish health.
    save_success(&engine, "ok-1");
    save_success(&engine, "ok-2");

    let data = engine.mapek.monitor().unwrap();
    assert_eq!(data.runtime_health.overall_status, HealthStatus::Unknown);
    assert_eq!(
        subsystem(&data, "experience_pipeline").status,
        HealthStatus::Unknown
    );
    assert_eq!(data.provider_status.sample_count, 2);
    assert!(!data.provider_status.available);
}

#[test]
fn partial_failures_degrade_instead_of_unhealthy() {
    let mut engine = engine();
    for i in 0..4 {
        save_success(&engine, &format!("ok-{i}"));
    }
    for i in 0..2 {
        save_failure(&engine, &format!("bad-{i}"));
    }

    let data = engine.mapek.monitor().unwrap();
    assert_eq!(data.runtime_health.overall_status, HealthStatus::Degraded);
    let pipeline = subsystem(&data, "experience_pipeline");
    assert_eq!(pipeline.status, HealthStatus::Degraded);
    assert!(pipeline.details.contains("33%"), "{}", pipeline.details);
}

#[test]
fn pending_recorded_issues_degrade_then_break_health() {
    let mut engine = engine();
    for i in 0..6 {
        save_success(&engine, &format!("ok-{i}"));
    }
    assert_eq!(
        engine
            .mapek
            .monitor()
            .unwrap()
            .runtime_health
            .overall_status,
        HealthStatus::Healthy
    );

    engine
        .store
        .save_proposal(&proposal(
            "p1",
            ProposalKind::RecordIssue,
            ProposalStatus::Pending,
        ))
        .unwrap();
    let degraded = engine.mapek.monitor().unwrap();
    assert_eq!(
        degraded.runtime_health.overall_status,
        HealthStatus::Degraded
    );
    assert_eq!(
        subsystem(&degraded, "learning_proposals").status,
        HealthStatus::Degraded
    );

    for id in ["p2", "p3"] {
        engine
            .store
            .save_proposal(&proposal(
                id,
                ProposalKind::RecordIssue,
                ProposalStatus::Pending,
            ))
            .unwrap();
    }
    let unhealthy = engine.mapek.monitor().unwrap();
    assert_eq!(
        unhealthy.runtime_health.overall_status,
        HealthStatus::Unhealthy
    );
    assert_eq!(
        subsystem(&unhealthy, "learning_proposals").status,
        HealthStatus::Unhealthy
    );
}

#[test]
fn failed_reflection_jobs_are_reported_and_degrade_health() {
    let mut engine = engine();
    for i in 0..6 {
        save_success(&engine, &format!("ok-{i}"));
    }
    for i in 0..3 {
        engine
            .store
            .save_job(&ReflectionJob {
                id: format!("job-{i}"),
                experience_id: "ok-0".to_string(),
                agent_id: "rudra".to_string(),
                status: ReflectionJobStatus::Failed,
                attempts: 1,
                created_ms: now_ms() - 10,
                updated_ms: now_ms(),
                error: Some("reflector blew up".to_string()),
            })
            .unwrap();
    }

    let data = engine.mapek.monitor().unwrap();
    assert_eq!(data.runtime_health.overall_status, HealthStatus::Unhealthy);
    assert_eq!(
        subsystem(&data, "reflection").status,
        HealthStatus::Unhealthy
    );
    let reflection_failures: Vec<_> = data
        .failures
        .iter()
        .filter(|f| f.component == "reflection")
        .collect();
    assert_eq!(reflection_failures.len(), 3);
    assert_eq!(reflection_failures[0].error_type, "reflection_job_failed");
    assert_eq!(reflection_failures[0].message, "reflector blew up");
}

#[test]
fn provider_latency_falls_back_to_real_reflection_latency() {
    let mut engine = engine();
    // Successes with no recorded duration: no experience latency exists.
    for i in 0..6 {
        save(
            &engine,
            experience(
                &format!("ok-{i}"),
                ExperienceOutcome::Success,
                "coding",
                &[],
                &[],
                None,
                None,
            ),
        );
    }
    engine
        .store
        .save_job(&ReflectionJob {
            id: "done-1".to_string(),
            experience_id: "ok-0".to_string(),
            agent_id: "rudra".to_string(),
            status: ReflectionJobStatus::Done,
            attempts: 1,
            created_ms: 1_000,
            updated_ms: 1_750,
            error: None,
        })
        .unwrap();

    let data = engine.mapek.monitor().unwrap();
    assert_eq!(data.provider_status.latency_ms, 750);
    assert!(
        data.provider_status
            .notes
            .iter()
            .any(|n| n.contains("reflection-job latency")),
        "{:?}",
        data.provider_status.notes
    );
}

// ---------- self-adaptation ----------

#[test]
fn adaptation_metrics_come_from_experiences() {
    let engine = engine();
    let empty = engine.self_adaptation.recent_performance().unwrap();
    assert_eq!(empty.reliability, None);
    assert_eq!(empty.success_rate, None);
    assert_eq!(empty.latency_ms, None);
    assert_eq!(empty.cost_tokens, None);
    assert!(
        engine
            .self_adaptation
            .detect_adaptation_opportunities()
            .unwrap()
            .is_empty()
    );

    for i in 0..6 {
        save(
            &engine,
            experience(
                &format!("bad-{i}"),
                ExperienceOutcome::Failure,
                "coding",
                &["tool exploded"],
                &[],
                Some(20_000),
                Some(200_000),
            ),
        );
    }

    let metrics = engine.self_adaptation.recent_performance().unwrap();
    assert_eq!(metrics.success_rate, Some(0.0));
    assert_eq!(metrics.reliability, Some(0.0));
    assert_eq!(metrics.latency_ms, Some(20_000));
    assert_eq!(metrics.cost_tokens, Some(200_000));
    // No correctness signal is stored, so accuracy stays unclaimed.
    assert_eq!(metrics.accuracy, None);

    let triggers = engine
        .self_adaptation
        .detect_adaptation_opportunities()
        .unwrap();
    assert!(triggers.contains(&AdaptationTrigger::ProviderFailing));
    assert!(triggers.contains(&AdaptationTrigger::ModelTooSlow));
    assert!(triggers.contains(&AdaptationTrigger::ExcessiveContext));
}

// ---------- self-evaluation ----------

fn expected_metrics() -> PerformanceMetrics {
    PerformanceMetrics {
        task_completion_rate: 0.7,
        task_correctness: 0.7,
        tool_success_rate: 0.7,
        model_performance: 0.5,
        agent_performance: 0.5,
        execution_time_ms: 10_000,
        token_consumption: 1_000,
        resource_consumption: 0.5,
        failure_rate: 0.5,
        recovery_rate: 0.5,
        regression_rate: 0.5,
        user_corrections: 0,
        skill_effectiveness: 0.5,
    }
}

#[test]
fn benchmark_without_data_says_insufficient_data() {
    let mut engine = engine();
    let benchmark = engine.self_evaluation.create_benchmark(
        "coding bench",
        "does coding work?",
        "coding",
        expected_metrics(),
    );

    let result = engine.self_evaluation.run_benchmark(&benchmark.id).unwrap();
    assert_eq!(result.data_points, 0);
    assert!(!result.sufficient_data);
    assert!(!result.passed);
    assert!(
        result.notes.starts_with("insufficient data"),
        "{}",
        result.notes
    );
    assert!(
        result.notes.contains("no metrics were estimated"),
        "{}",
        result.notes
    );
    // Nothing was invented: every metric is zero.
    assert_eq!(result.actual_metrics.task_completion_rate, 0.0);
    assert_eq!(result.actual_metrics.execution_time_ms, 0);
    assert_eq!(result.actual_metrics.token_consumption, 0);
}

#[test]
fn benchmark_metrics_are_computed_from_recorded_experiences() {
    let mut engine = engine();
    save(
        &engine,
        experience(
            "c1",
            ExperienceOutcome::Success,
            "coding",
            &[],
            &[],
            Some(1_000),
            Some(100),
        ),
    );
    save(
        &engine,
        experience(
            "c2",
            ExperienceOutcome::Success,
            "coding",
            &[],
            &[],
            Some(2_000),
            Some(100),
        ),
    );
    save(
        &engine,
        experience(
            "c3",
            ExperienceOutcome::Success,
            "coding",
            &[],
            &[],
            Some(3_000),
            Some(100),
        ),
    );
    save(
        &engine,
        experience(
            "c4",
            ExperienceOutcome::Success,
            "coding",
            &[],
            &["use rust, not python"],
            Some(4_000),
            Some(100),
        ),
    );
    save(
        &engine,
        experience(
            "c5",
            ExperienceOutcome::Failure,
            "coding",
            &["edit failed"],
            &[],
            Some(5_000),
            Some(100),
        ),
    );
    // A different task type must not leak into this benchmark.
    save(
        &engine,
        experience(
            "r1",
            ExperienceOutcome::Failure,
            "research",
            &["boom"],
            &[],
            Some(9_000),
            Some(900),
        ),
    );

    let benchmark = engine.self_evaluation.create_benchmark(
        "coding bench",
        "does coding work?",
        "coding",
        expected_metrics(),
    );
    let result = engine.self_evaluation.run_benchmark(&benchmark.id).unwrap();

    assert_eq!(result.data_points, 5);
    assert!(result.sufficient_data);
    assert!((result.actual_metrics.task_completion_rate - 0.8).abs() < 1e-6);
    assert!((result.actual_metrics.task_correctness - 0.8).abs() < 1e-6);
    assert!((result.actual_metrics.tool_success_rate - 0.8).abs() < 1e-6);
    assert!((result.actual_metrics.failure_rate - 0.2).abs() < 1e-6);
    assert_eq!(result.actual_metrics.execution_time_ms, 3_000);
    assert_eq!(result.actual_metrics.token_consumption, 100);
    assert_eq!(result.actual_metrics.user_corrections, 1);
    assert!(result.passed);
    // Unmeasurable metrics are named, not silently invented.
    assert!(
        result.notes.contains("not measured from store data"),
        "{}",
        result.notes
    );
    assert!(
        result.notes.contains("model_performance"),
        "{}",
        result.notes
    );
    assert_eq!(result.actual_metrics.model_performance, 0.0);
    assert_eq!(result.actual_metrics.skill_effectiveness, 0.0);
}

// ---------- self-improvement ----------

#[test]
fn opportunities_are_empty_without_evidence() {
    let mut engine = engine();
    let opportunities = engine
        .self_improvement
        .identify_improvement_opportunities()
        .unwrap();
    assert!(opportunities.is_empty());
    let note = engine.self_improvement.last_opportunity_note().unwrap();
    assert!(note.contains("no recorded experiences"), "{note}");

    // A store full of successes still has no failure evidence.
    for i in 0..6 {
        save_success(&engine, &format!("ok-{i}"));
    }
    let opportunities = engine
        .self_improvement
        .identify_improvement_opportunities()
        .unwrap();
    assert!(opportunities.is_empty());
    let note = engine.self_improvement.last_opportunity_note().unwrap();
    assert!(
        note.contains("none of the 6 recorded experience(s) failed"),
        "{note}"
    );
}

#[test]
fn opportunities_are_derived_from_recorded_failures() {
    let mut engine = engine();
    save(
        &engine,
        experience(
            "c1",
            ExperienceOutcome::Failure,
            "coding",
            &["compile error"],
            &[],
            None,
            None,
        ),
    );
    save(
        &engine,
        experience(
            "c2",
            ExperienceOutcome::Failure,
            "coding",
            &[],
            &[],
            None,
            None,
        ),
    );
    save(
        &engine,
        experience(
            "s1",
            ExperienceOutcome::Partial,
            "shell",
            &["exit 1"],
            &[],
            None,
            None,
        ),
    );
    save_success(&engine, "ok-1");
    save_success(&engine, "ok-2");

    let opportunities = engine
        .self_improvement
        .identify_improvement_opportunities()
        .unwrap();
    let areas: Vec<ImprovementArea> = opportunities.iter().map(|o| o.area.clone()).collect();
    assert!(
        areas.contains(&ImprovementArea::CodingPerformance),
        "{areas:?}"
    );
    assert!(areas.contains(&ImprovementArea::ToolSelection), "{areas:?}");

    let coding = opportunities
        .iter()
        .find(|o| o.area == ImprovementArea::CodingPerformance)
        .unwrap();
    // Two coding failures and two coding successes; `problems` is the exact
    // union of "failed or errored", so the failing rows count once each.
    assert!(
        coding
            .evidence
            .iter()
            .any(|e| e.contains("2 of 4 recorded 'coding'")),
        "{:?}",
        coding.evidence
    );
    assert!(
        coding.priority >= 5,
        "priority should reflect a 50% problem rate: {}",
        coding.priority
    );
    assert!(coding.expected_value > 0.5);
    assert!(
        engine
            .self_improvement
            .last_opportunity_note()
            .unwrap()
            .contains("derived from")
    );
}

// ---------- new store query API ----------

#[test]
fn store_aggregates_cover_proposals_and_jobs() {
    let engine = engine();
    for (id, status) in [
        ("p1", ProposalStatus::Pending),
        ("p2", ProposalStatus::Pending),
        ("p3", ProposalStatus::Approved),
        ("p4", ProposalStatus::Rejected),
        ("p5", ProposalStatus::Applied),
    ] {
        engine
            .store
            .save_proposal(&proposal(id, ProposalKind::AddMemory, status))
            .unwrap();
    }
    let stats = engine.store.proposal_stats().unwrap();
    assert_eq!(stats.total, 5);
    assert_eq!(stats.pending, 2);
    assert_eq!(stats.approved, 1);
    assert_eq!(stats.rejected, 1);
    assert_eq!(stats.applied, 1);
    assert_eq!(stats.pending_record_issue, 0);

    engine
        .store
        .save_job(&ReflectionJob {
            id: "j1".to_string(),
            experience_id: "e1".to_string(),
            agent_id: "rudra".to_string(),
            status: ReflectionJobStatus::Done,
            attempts: 1,
            created_ms: 1_000,
            updated_ms: 1_250,
            error: None,
        })
        .unwrap();
    engine
        .store
        .save_job(&ReflectionJob {
            id: "j2".to_string(),
            experience_id: "e2".to_string(),
            agent_id: "rudra".to_string(),
            status: ReflectionJobStatus::Failed,
            attempts: 1,
            created_ms: 1_000,
            updated_ms: 1_100,
            error: Some("nope".to_string()),
        })
        .unwrap();
    let jobs = engine.store.reflection_stats().unwrap();
    assert_eq!(jobs.total, 2);
    assert_eq!(jobs.done, 1);
    assert_eq!(jobs.failed, 1);
    assert_eq!(jobs.avg_latency_ms, Some(250));
    assert_eq!(jobs.failure_rate(), Some(0.5));
}
