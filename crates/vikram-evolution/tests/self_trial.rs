use std::sync::{Arc, Mutex};

use vikram_evolution::self_config::{ConfigSurface, ConfigTarget, SelfConfigurationEngine};
use vikram_evolution::self_evaluation::{PerformanceMetrics, SelfEvaluationEngine};
use vikram_evolution::self_trial::{TRIAL_NAMESPACE, trial_change, trial_history};
use vikram_evolution::store::EvolutionStore;
use vikram_evolution::types::{Experience, ExperienceOutcome, Sensitivity, now_ms};

const KEY: &str = "context:memory_budget_tokens";

struct FakeSurface {
    values: Mutex<std::collections::HashMap<String, String>>,
    writable: &'static [&'static str],
}

impl FakeSurface {
    fn new(writable: &'static [&'static str]) -> Arc<Self> {
        Arc::new(Self {
            values: Mutex::new(std::collections::HashMap::new()),
            writable,
        })
    }

    fn get(&self, key: &str) -> Option<String> {
        self.values.lock().unwrap().get(key).cloned()
    }

    fn set(&self, key: &str, value: &str) {
        self.values
            .lock()
            .unwrap()
            .insert(key.to_owned(), value.to_owned());
    }
}

impl ConfigSurface for FakeSurface {
    fn read(&self, key: &str) -> Option<String> {
        self.get(key)
    }

    fn write(&self, key: &str, value: &str) -> Result<(), String> {
        self.set(key, value);
        Ok(())
    }

    fn writable(&self, key: &str) -> bool {
        self.writable.iter().any(|prefix| key.starts_with(prefix))
    }
}

fn expected() -> PerformanceMetrics {
    PerformanceMetrics {
        task_completion_rate: 0.9,
        task_correctness: 0.9,
        tool_success_rate: 0.9,
        model_performance: 0.0,
        agent_performance: 0.0,
        execution_time_ms: 0,
        token_consumption: 0,
        resource_consumption: 0.0,
        failure_rate: 0.0,
        recovery_rate: 0.0,
        regression_rate: 0.0,
        user_corrections: 0,
        skill_effectiveness: 0.0,
    }
}

fn seed(store: &EvolutionStore, prefix: &str, outcome: ExperienceOutcome, n: usize) {
    for i in 0..n {
        store
            .save_experience(&Experience {
                id: format!("{prefix}-{i}"),
                timestamp_ms: now_ms(),
                agent_id: "agent".to_owned(),
                project_id: None,
                task_category: "coding".to_owned(),
                user_request: "request".to_owned(),
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
}

struct Harness {
    store: EvolutionStore,
    surface: Arc<FakeSurface>,
    config: SelfConfigurationEngine,
    evaluation: SelfEvaluationEngine,
    benchmark_id: String,
}

fn harness() -> Harness {
    let store = EvolutionStore::open_in_memory().unwrap();
    let surface = FakeSurface::new(&["context:"]);
    surface.set(KEY, "1200");
    let mut config = SelfConfigurationEngine::new(store.clone());
    config.attach_surface(surface.clone());
    let mut evaluation = SelfEvaluationEngine::new(store.clone()).unwrap();
    let benchmark = evaluation
        .create_benchmark("trial", "coding quality", "coding", expected())
        .unwrap();
    Harness {
        store,
        surface,
        config,
        evaluation,
        benchmark_id: benchmark.id,
    }
}

#[test]
fn a_self_change_that_worsens_the_benchmark_is_rolled_back() {
    let mut h = harness();
    seed(&h.store, "good", ExperienceOutcome::Success, 5);
    h.evaluation.run_benchmark(&h.benchmark_id).unwrap();
    seed(&h.store, "bad", ExperienceOutcome::Failure, 5);

    let outcome = trial_change(
        &mut h.config,
        &mut h.evaluation,
        ConfigTarget::Context("memory_budget_tokens".to_owned()),
        "2400",
        &h.benchmark_id,
    )
    .unwrap();

    assert!(outcome.rolled_back);
    assert!(!outcome.applied);
    let before = outcome.before.as_ref().expect("baseline metric");
    let after = outcome.after.as_ref().expect("post-change metric");
    assert_eq!(before.task_correctness, 1.0);
    assert_eq!(after.task_correctness, 0.5);
    assert!(outcome.reason.contains("got worse"), "{}", outcome.reason);
    assert_eq!(h.surface.get(KEY).as_deref(), Some("1200"));
    assert_eq!(trial_history(&h.store).unwrap().len(), 1);
}

#[test]
fn a_self_change_that_improves_the_benchmark_is_kept() {
    let mut h = harness();
    seed(&h.store, "bad", ExperienceOutcome::Failure, 5);
    h.evaluation.run_benchmark(&h.benchmark_id).unwrap();
    seed(&h.store, "good", ExperienceOutcome::Success, 5);

    let outcome = trial_change(
        &mut h.config,
        &mut h.evaluation,
        ConfigTarget::Context("memory_budget_tokens".to_owned()),
        "2400",
        &h.benchmark_id,
    )
    .unwrap();

    assert!(outcome.applied);
    assert!(!outcome.rolled_back);
    assert!(outcome.reason.contains("improved"), "{}", outcome.reason);
    assert_eq!(outcome.after.unwrap().task_correctness, 0.5);
    assert_eq!(h.surface.get(KEY).as_deref(), Some("2400"));
    let history = trial_history(&h.store).unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].applied);
}

#[test]
fn an_unmeasurable_change_is_rolled_back_not_kept() {
    let mut h = harness();

    let outcome = trial_change(
        &mut h.config,
        &mut h.evaluation,
        ConfigTarget::Context("memory_budget_tokens".to_owned()),
        "2400",
        &h.benchmark_id,
    )
    .unwrap();

    assert!(outcome.rolled_back);
    assert!(!outcome.applied);
    assert!(outcome.before.is_none());
    assert!(outcome.after.is_none());
    assert!(
        outcome.reason.contains("unmeasurable"),
        "{}",
        outcome.reason
    );
    assert_eq!(h.surface.get(KEY).as_deref(), Some("1200"));
    assert_eq!(trial_history(&h.store).unwrap().len(), 1);
}

#[test]
fn a_benchmark_error_rolls_back_and_reports_it() {
    let mut h = harness();

    let outcome = trial_change(
        &mut h.config,
        &mut h.evaluation,
        ConfigTarget::Context("memory_budget_tokens".to_owned()),
        "2400",
        "bench-missing",
    )
    .unwrap();

    assert!(outcome.rolled_back);
    assert!(!outcome.applied);
    assert!(
        outcome.reason.contains("failed after the change"),
        "{}",
        outcome.reason
    );
    assert!(
        outcome.reason.contains("Benchmark not found"),
        "{}",
        outcome.reason
    );
    assert_eq!(h.surface.get(KEY).as_deref(), Some("1200"));
    assert_eq!(trial_history(&h.store).unwrap().len(), 1);
}

#[test]
fn a_key_the_surface_vetoes_is_refused_before_anything_is_written() {
    let mut h = harness();

    let err = trial_change(
        &mut h.config,
        &mut h.evaluation,
        ConfigTarget::Tool("shell".to_owned()),
        "enabled",
        &h.benchmark_id,
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("may not change what the assistant is allowed to do"),
        "{err}"
    );
    assert_eq!(h.surface.get("tool:shell"), None);
    assert!(h.config.get_change_history().unwrap().is_empty());
    assert!(trial_history(&h.store).unwrap().is_empty());
}

#[test]
fn a_trial_without_a_surface_is_refused() {
    let store = EvolutionStore::open_in_memory().unwrap();
    let mut config = SelfConfigurationEngine::new(store.clone());
    let mut evaluation = SelfEvaluationEngine::new(store.clone()).unwrap();
    let benchmark = evaluation
        .create_benchmark("trial", "coding quality", "coding", expected())
        .unwrap();

    let err = trial_change(
        &mut config,
        &mut evaluation,
        ConfigTarget::Context("memory_budget_tokens".to_owned()),
        "2400",
        &benchmark.id,
    )
    .unwrap_err();

    assert!(
        err.to_string().contains("no live configuration surface"),
        "{err}"
    );
    assert!(trial_history(&store).unwrap().is_empty());
}

#[test]
fn a_trial_record_is_visible_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evolution.db");

    let change_id;
    {
        let store = EvolutionStore::open(&path).unwrap();
        let surface = FakeSurface::new(&["context:"]);
        surface.set(KEY, "1200");
        let mut config = SelfConfigurationEngine::new(store.clone());
        config.attach_surface(surface);
        let mut evaluation = SelfEvaluationEngine::new(store.clone()).unwrap();
        let benchmark = evaluation
            .create_benchmark("trial", "coding quality", "coding", expected())
            .unwrap();

        seed(&store, "good", ExperienceOutcome::Success, 5);
        evaluation.run_benchmark(&benchmark.id).unwrap();
        seed(&store, "bad", ExperienceOutcome::Failure, 5);

        let outcome = trial_change(
            &mut config,
            &mut evaluation,
            ConfigTarget::Context("memory_budget_tokens".to_owned()),
            "2400",
            &benchmark.id,
        )
        .unwrap();
        change_id = outcome.change_id;
    }

    let store = EvolutionStore::open(&path).unwrap();
    assert_eq!(store.list_engine_records(TRIAL_NAMESPACE).unwrap().len(), 1);
    let history = trial_history(&store).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].change_id, change_id);
    assert!(history[0].rolled_back);
    assert_eq!(history[0].target, KEY);

    let mut config = SelfConfigurationEngine::new(store);
    assert_eq!(config.get_change_history().unwrap().len(), 1);
}
