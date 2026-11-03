//! VIKRAM Self-Evolution Engine.
//!
//! Coordinates experience collection, layered memory, reflection, skill evolution,
//! hive learning, and reflective (GEPA-inspired) optimization.

pub mod dashboard;
pub mod error;
pub mod experience;
pub mod hive;
pub mod memory;
pub mod optimization;
pub mod redact;
pub mod reflection;
pub mod skills;
pub mod store;
pub mod types;

pub use dashboard::{Dashboard, LearningDashboard};
pub use error::{EvolutionError, Result};
pub use experience::ExperienceCollector;
pub use hive::HiveLearning;
pub use memory::LayeredMemory;
pub use optimization::OptimizationEngine;
pub use reflection::{HeuristicReflector, ReflectionEngine, Reflector};
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
        Ok(Self {
            store,
            experience,
            memory,
            reflection,
            skills,
            optimization,
            hive,
            dashboard,
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
        Ok(Self {
            store,
            experience,
            memory,
            reflection,
            skills,
            optimization,
            hive,
            dashboard,
        })
    }
}
