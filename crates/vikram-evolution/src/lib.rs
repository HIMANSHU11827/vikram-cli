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

pub mod autonomy;
pub mod conversational;
pub mod dashboard;
pub mod error;
pub mod experience;
pub mod hive;
pub mod mapek;
pub mod memory;
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
        Ok(Self {
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
        })
    }
    pub fn open_in_memory(skills_root: PathBuf) -> Result<Self> {
        let store = EvolutionStore::open_in_memory()?;
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
        Ok(Self {
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
        })
    }
}
