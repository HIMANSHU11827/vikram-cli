//! VIKRAM Self-Evolution Engine.
//!
//! Coordinates experience collection, layered memory, reflection, skill evolution,
//! hive learning, and reflective (GEPA-inspired) optimization.
//!
//! New autonomous capabilities:
//! - Self-Awareness: Runtime inspection and self-model maintenance
//! - Self-Configuration: Independent management of configurable parts
//! - Self-Adaptation: Recognize ineffective approaches and choose better strategies
//! - Self-Improvement: Proactively identify and execute improvements
//! - Self-Development: Controlled development of new capabilities
//! - Self-Evaluation: Measure whether Vikram is actually improving
//! - Self-Recovery: Recover from failed changes and operational problems
//! - MAPE-K Loop: Monitor-Analyze-Plan-Execute over shared Knowledge
//! - Autonomy Manager: Implement autonomy levels and safety controls
//! - Conversational Control: Natural language interface for evolution control
//!
//! # Concurrency model
//!
//! [`EvolutionStore`] is the single shared handle (an `Arc<Mutex<Connection>>`)
//! behind every subsystem; subsystems are cheap clones of it. The engine
//! itself is a plain struct of those subsystems — it is not `Sync` for
//! the stateful `self_*` engines (they hold in-memory `Vec`s), so the
//! runtime keeps the whole engine behind a `Mutex`, exactly as before.
//!
//! # Background runner
//!
//! There is no implicit thread: [`EvolutionEngine::recover_and_process`]
//! drains reflection jobs synchronously and is what the runtime calls on
//! boot (recovery) and after turn-end recording; a host that wants a
//! background loop can `spawn` a task that calls it periodically. See
//! `vikram-runtime`'s `Runtime::boot`, which calls it before serving,
//! and `learning::on_turn_end`, which invokes evolution on every
//! tool-bearing turn.
//!
//! # Store trait
//!
//! Subsystems take a concrete [`EvolutionStore`] rather than a trait
//! object: the store is already a cheap `Arc` clone, and a trait would
//! force `dyn` dispatch on every query. The `store()` accessors on each
//! subsystem expose the shared handle for composition (e.g. wiring a
//! custom collector into `ReflectionEngine::with_subsystems`).
//!
//! # Single source of truth for learning
//!
//! All durable learning data lives in this crate's SQLite store. The
//! runtime's `learning.rs` (`LearningManager`, JSON) is only the approval
//! surface: it stages proposals for the operator and hands model-shaped
//! proposals to the same policy gate. It never writes a second copy of
//! experiences, memories, or reflection jobs.

pub mod autonomy;
pub mod conversational;
pub mod dashboard;
pub mod error;
pub mod experience;
pub mod hive;
pub mod mapek;
pub mod memory;
pub mod metrics;
pub mod optimization;
pub mod redact;
pub mod reflection;
pub mod self_adaptation;
pub mod self_awareness;
pub mod self_config;
pub mod self_development;
pub mod self_evaluation;
pub mod self_improvement;
pub mod self_recovery;
pub mod skills;
pub mod store;
pub mod types;

pub use autonomy::{ActionType, AutonomyLevel, AutonomyManager};
pub use conversational::{CommandResponse, ConversationalCommand, ConversationalControl};
pub use dashboard::{Dashboard, LearningDashboard};
pub use error::{EvolutionError, Result};
pub use experience::ExperienceCollector;
pub use hive::HiveLearning;
pub use mapek::{CycleResult, HealthStatus, MapekLoop};
pub use memory::LayeredMemory;
pub use metrics::{
    CategoryStats, ExperienceStats, MIN_SAMPLE, OutcomeCounts, ProposalStats, ReflectionStats,
};
pub use optimization::OptimizationEngine;
pub use reflection::{HeuristicReflector, ReflectionEngine, Reflector};
pub use self_adaptation::{
    AdaptationConstraints, AdaptationStrategy, AdaptationTrigger, RiskLevel, SelfAdaptationEngine,
};
pub use self_awareness::{SelfAwarenessEngine, SelfModel};
pub use self_config::{ConfigChange, ModelSelectionCriteria, SelfConfigurationEngine};
pub use self_development::{DevelopmentProject, ProjectStatus, SelfDevelopmentEngine};
pub use self_evaluation::{EvaluationBenchmark, PerformanceMetrics, SelfEvaluationEngine};
pub use self_improvement::{ExecutionStatus, ImprovementExecution, SelfImprovementEngine};
pub use self_recovery::{RecoveryOperation, RecoveryType, SelfRecoveryEngine};
pub use skills::SkillEvolution;
pub use store::EvolutionStore;
pub use types::*;

use std::path::PathBuf;
use std::sync::Arc;

/// Top-level facade.
///
/// Fields are `pub` for ergonomic composition (`engine.memory.remember(..)`)
/// but each subsystem is immutable through `&self` (they only hold the
/// shared store handle), so callers cannot corrupt engine state by
/// mutating a field — the only mutable state lives behind the store's
/// mutex or inside the `self_*` engines' own `Vec`s, which require `&mut`.
pub struct EvolutionEngine {
    pub store: EvolutionStore,
    pub experience: ExperienceCollector,
    pub memory: LayeredMemory,
    pub reflection: ReflectionEngine,
    pub skills: SkillEvolution,
    pub optimization: OptimizationEngine,
    pub hive: HiveLearning,
    pub dashboard: Dashboard,
    pub self_awareness: SelfAwarenessEngine,
    pub self_config: SelfConfigurationEngine,
    pub self_adaptation: SelfAdaptationEngine,
    pub self_improvement: SelfImprovementEngine,
    pub self_development: SelfDevelopmentEngine,
    pub self_evaluation: SelfEvaluationEngine,
    pub self_recovery: SelfRecoveryEngine,
    pub mapek: MapekLoop,
    pub autonomy: AutonomyManager,
    pub conversational: ConversationalControl,
}

impl EvolutionEngine {
    pub fn open(db_path: PathBuf, skills_root: PathBuf) -> Result<Self> {
        let store = EvolutionStore::open(db_path)?;
        Ok(Self::from_store(store, skills_root))
    }
    pub fn open_in_memory(skills_root: PathBuf) -> Result<Self> {
        let store = EvolutionStore::open_in_memory()?;
        Ok(Self::from_store(store, skills_root))
    }

    fn from_store(store: EvolutionStore, skills_root: PathBuf) -> Self {
        let experience = ExperienceCollector::new(store.clone());
        let memory = LayeredMemory::new(store.clone());
        let reflection = ReflectionEngine::new(store.clone(), Arc::new(HeuristicReflector));
        let skills = SkillEvolution::new(store.clone(), skills_root);
        let optimization = OptimizationEngine::new(store.clone());
        let hive = HiveLearning::new(store.clone());
        let dashboard = Dashboard::new(store.clone());
        let self_awareness = SelfAwarenessEngine::new(store.clone());
        let self_config = SelfConfigurationEngine::new(store.clone());
        let self_adaptation = SelfAdaptationEngine::new(store.clone());
        let self_improvement = SelfImprovementEngine::new(store.clone());
        let self_development = SelfDevelopmentEngine::new(store.clone());
        let self_evaluation = SelfEvaluationEngine::new(store.clone());
        let self_recovery = SelfRecoveryEngine::new(store.clone());
        let mapek = MapekLoop::new(store.clone());
        let autonomy = AutonomyManager::new(store.clone());
        let conversational = ConversationalControl::new(store.clone());
        Self {
            store,
            experience,
            memory,
            reflection,
            skills,
            optimization,
            hive,
            dashboard,
            self_awareness,
            self_config,
            self_adaptation,
            self_improvement,
            self_development,
            self_evaluation,
            self_recovery,
            mapek,
            autonomy,
            conversational,
        }
    }

    /// Recover interrupted reflection jobs (e.g. after a crash) and drain
    /// any that were pending. Returns `(recovered, processed)`.
    /// Safe to call on every boot; idempotent.
    pub fn recover_and_process(&self) -> Result<(usize, usize)> {
        let recovered = self.reflection.recover_jobs()?;
        let processed = self.reflection.process_pending()?;
        Ok((recovered, processed))
    }
}
