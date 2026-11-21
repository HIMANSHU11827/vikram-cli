//! Runtime inspection and self-model maintenance.
//!
//! DORMANT / store-scoped: nothing in the workspace calls this engine, but
//! its self-model is no longer constants — [`SelfAwarenessEngine::inspect_runtime`]
//! persists a [`SelfModel`] to `engine_records` and reports two kinds of
//! data:
//!
//! - **Store-derived state** ([`SelfModel::store_state`], provider signals,
//!   open issues, failure patterns): real aggregates read back from the
//!   [`EvolutionStore`] — experience/proposal/reflection tallies, live
//!   memory counts per layer, recorded skill versions, and the
//!   config-shaped signals (`config.model`, `config.provider`,
//!   `config.context_window`) producers wrote into experience metadata.
//!   Those signals are *recorded intent*, not verified live configuration.
//! - **Compiled-surface declarations** (architecture, capabilities, tools,
//!   agents, context, resources): what this workspace contains as built.
//!   They describe the code, not a probed runtime — nothing here performs
//!   network or process inspection, so `providers.status` is
//!   [`ProviderStatus::Unknown`] rather than a guessed `Available`.

use crate::error::{EvolutionError, Result};
use crate::metrics::{self, ExperienceStats, ProposalStats, ReflectionStats};
use crate::store::{EvolutionStore, from_json_value, to_json_value};
use crate::types::{LearningEvent, ProposalKind, gen_id, now_ms};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Snapshot of Vikram's architecture.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchitectureSnapshot {
    pub crates: Vec<CrateInfo>,
    pub modules: Vec<ModuleInfo>,
    pub subsystems: Vec<SubsystemInfo>,
    pub version: String,
    pub rust_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CrateInfo {
    pub name: String,
    pub version: String,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleInfo {
    pub crate_name: String,
    pub module_path: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubsystemInfo {
    pub name: String,
    pub status: SubsystemStatus,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubsystemStatus {
    Implemented,
    Partial,
    Missing,
}

/// Inventory of available capabilities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityInventory {
    pub planning: bool,
    pub task_decomposition: bool,
    pub memory_retrieval: bool,
    pub context_management: bool,
    pub tool_execution: bool,
    pub agent_delegation: bool,
    pub hive_collaboration: bool,
    pub mcp_integration: bool,
    pub skill_system: bool,
    pub learning: bool,
    pub evolution: bool,
}

/// Inventory of available tools.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInventory {
    pub builtin: Vec<ToolInfo>,
    pub mcp: Vec<McpToolInfo>,
    pub plugin: Vec<ToolInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub permission_shape: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
}

/// Inventory of providers and models *as recorded in the store*.
///
/// The engine holds only the [`EvolutionStore`], so it cannot see the
/// runtime's live provider configuration. `current_model`,
/// `available_models`, `context_window`, and `observed_provider` come from
/// config-shaped signals producers recorded into experience metadata —
/// recorded intent, newest first, not verified live state. `base_url` is
/// left empty because no signal records one. `status` is
/// [`ProviderStatus::Unknown`]: reachability cannot be checked from here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderInventory {
    pub base_url: String,
    pub current_model: String,
    pub available_models: Vec<ModelInfo>,
    pub context_window: Option<u64>,
    pub status: ProviderStatus,
    /// Provider name from the newest recorded `config.provider` signal, if
    /// any. A name signal, not a verified endpoint.
    #[serde(default)]
    pub observed_provider: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub context_window: Option<u64>,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderStatus {
    Available,
    Unavailable,
    Degraded,
    /// The store cannot answer whether the provider is reachable.
    Unknown,
}

/// Inventory of agents, bots, and hive capabilities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentInventory {
    pub main_agent: AgentInfo,
    pub sub_agents: Vec<AgentInfo>,
    pub bots: Vec<AgentInfo>,
    pub hive_teams: Vec<HiveTeamInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub role: String,
    pub specialization: Vec<String>,
    pub model: Option<String>,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HiveTeamInfo {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
    pub strategy: String,
}

/// Inventory of memory systems.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryInventory {
    pub sqlite_memory: bool,
    pub layered_memory: bool,
    pub episodic: bool,
    pub semantic: bool,
    pub procedural: bool,
    pub user_memory: bool,
    pub project_memory: bool,
    pub agent_memory: bool,
}

/// Inventory of context management.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextInventory {
    pub context_loader: bool,
    pub token_budgeting: bool,
    pub mtime_invalidation: bool,
    pub deduplication: bool,
    pub caching: bool,
}

/// Known limitations of the system.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnownLimitations {
    pub token_limits: Vec<String>,
    pub tool_constraints: Vec<String>,
    pub provider_constraints: Vec<String>,
    pub resource_constraints: Vec<String>,
}

/// Known bugs and issues.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnownBugs {
    pub open_issues: Vec<IssueInfo>,
    pub known_failures: Vec<FailurePattern>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssueInfo {
    pub id: String,
    pub description: String,
    pub severity: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailurePattern {
    pub pattern: String,
    pub frequency: u32,
    pub last_occurred: u64,
}

/// Feature implementation status.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeatureStatus {
    pub fully_implemented: Vec<String>,
    pub partially_implemented: Vec<String>,
    pub missing: Vec<String>,
}

/// Resource snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub cpu_usage: Option<f32>,
    pub memory_usage: Option<u64>,
    pub disk_usage: Option<u64>,
    pub network_available: bool,
    pub sandbox_available: bool,
}

/// Store-derived awareness: aggregates read back from the
/// [`EvolutionStore`]. This is the part of the self-model grounded in
/// recorded data; everything else in [`SelfModel`] describes the compiled
/// surface. Empty store → zero counts and `None` averages, never guesses.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreAwareness {
    /// Experience outcome/duration/token aggregates over the default
    /// recency window and all time.
    pub experiences: ExperienceStats,
    /// Proposal tallies by status, including pending `record_issue` count.
    pub proposals: ProposalStats,
    /// Reflection-job tallies by status plus mean done-job latency.
    pub reflection_jobs: ReflectionStats,
    /// Live (non-superseded, non-expired) memory entries.
    pub live_memories: usize,
    /// Live memory entries grouped by layer name (`episodic`, `semantic`, …).
    pub memory_by_layer: BTreeMap<String, usize>,
    /// Total learning events recorded.
    pub learning_events: usize,
    /// Highest recorded version per skill name.
    pub skill_versions: Vec<(String, u32)>,
}

/// Complete self-model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelfModel {
    pub id: String,
    pub architecture: ArchitectureSnapshot,
    pub capabilities: CapabilityInventory,
    pub tools: ToolInventory,
    pub providers: ProviderInventory,
    pub agents: AgentInventory,
    pub memory: MemoryInventory,
    pub context: ContextInventory,
    pub limitations: KnownLimitations,
    pub bugs: KnownBugs,
    pub features: FeatureStatus,
    pub resources: ResourceSnapshot,
    /// What the store actually holds right now.
    #[serde(default)]
    pub store_state: StoreAwareness,
    pub last_sync: u64,
    pub version: u32,
}

pub struct SelfAwarenessEngine {
    store: EvolutionStore,
    current_model: Option<SelfModel>,
}

impl SelfAwarenessEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            current_model: None,
        }
    }

    /// Perform full runtime inspection and build self-model.
    ///
    /// The store-derived half ([`SelfModel::store_state`], provider
    /// signals, open issues, failure patterns) reflects recorded data at
    /// call time; the compiled-surface half is a declaration of what this
    /// workspace contains. The model is persisted to `engine_records`
    /// under `self_awareness.model/latest`, so [`Self::get_self_model`]
    /// survives a restart, and `version` increments from the last
    /// persisted model.
    pub fn inspect_runtime(&mut self) -> Result<SelfModel> {
        let version = match self.load_model()? {
            Some(previous) => previous.version.saturating_add(1),
            None => 1,
        };
        let model = SelfModel {
            id: gen_id("self-model"),
            architecture: self.inspect_architecture()?,
            capabilities: self.inspect_capabilities()?,
            tools: self.inspect_tools()?,
            providers: self.inspect_providers()?,
            agents: self.inspect_agents()?,
            memory: self.inspect_memory()?,
            context: self.inspect_context()?,
            limitations: self.inspect_limitations()?,
            bugs: self.inspect_bugs()?,
            features: self.inspect_features()?,
            resources: self.inspect_resources()?,
            store_state: self.inspect_store_state()?,
            last_sync: now_ms(),
            version,
        };

        self.current_model = Some(model.clone());
        self.save_model(&model)?;
        Ok(model)
    }

    /// Synchronize self-model with actual implementation.
    pub fn sync_model(&mut self) -> Result<SelfModel> {
        let latest = self.inspect_runtime()?;
        let _changes = self.detect_changes(&latest)?;
        self.current_model = Some(latest.clone());
        Ok(latest)
    }

    /// Verify that a claimed capability actually exists.
    pub fn verify_capability(&self, capability: &str) -> Result<bool> {
        if let Some(model) = &self.current_model {
            match capability {
                "planning" => Ok(model.capabilities.planning),
                "task_decomposition" => Ok(model.capabilities.task_decomposition),
                "memory_retrieval" => Ok(model.capabilities.memory_retrieval),
                "context_management" => Ok(model.capabilities.context_management),
                "tool_execution" => Ok(model.capabilities.tool_execution),
                "agent_delegation" => Ok(model.capabilities.agent_delegation),
                "hive_collaboration" => Ok(model.capabilities.hive_collaboration),
                "mcp_integration" => Ok(model.capabilities.mcp_integration),
                "skill_system" => Ok(model.capabilities.skill_system),
                "learning" => Ok(model.capabilities.learning),
                "evolution" => Ok(model.capabilities.evolution),
                _ => Ok(false),
            }
        } else {
            Err(EvolutionError::NotFound(
                "Self-model not initialized".to_string(),
            ))
        }
    }

    /// Detect changes since last sync.
    pub fn detect_changes(&self, new_model: &SelfModel) -> Result<Vec<String>> {
        let mut changes = Vec::new();
        if let Some(old) = &self.current_model {
            if old.capabilities != new_model.capabilities {
                changes.push("Capabilities changed".to_string());
            }
            if old.tools != new_model.tools {
                changes.push("Tools changed".to_string());
            }
            if old.providers != new_model.providers {
                changes.push("Providers changed".to_string());
            }
            if old.agents != new_model.agents {
                changes.push("Agents changed".to_string());
            }
        }
        Ok(changes)
    }

    /// Get current self-model.
    ///
    /// Falls back to the model persisted by the last
    /// [`Self::inspect_runtime`] in `engine_records`, so a fresh engine
    /// over the same store still answers with the last inspection.
    pub fn get_self_model(&self) -> Result<SelfModel> {
        if let Some(model) = &self.current_model {
            return Ok(model.clone());
        }
        self.load_model()?
            .ok_or_else(|| EvolutionError::NotFound("Self-model not available".to_string()))
    }

    fn inspect_architecture(&self) -> Result<ArchitectureSnapshot> {
        Ok(ArchitectureSnapshot {
            crates: vec![
                CrateInfo {
                    name: "vikram-contracts".to_string(),
                    version: "0.1.0".to_string(),
                    features: vec![],
                },
                CrateInfo {
                    name: "vikram-policy".to_string(),
                    version: "0.1.0".to_string(),
                    features: vec![],
                },
                CrateInfo {
                    name: "vikram-store".to_string(),
                    version: "0.1.0".to_string(),
                    features: vec![],
                },
                CrateInfo {
                    name: "vikram-runtime".to_string(),
                    version: "0.1.0".to_string(),
                    features: vec![],
                },
                CrateInfo {
                    name: "vikram-evolution".to_string(),
                    version: "0.1.0".to_string(),
                    features: vec![],
                },
            ],
            modules: vec![],
            subsystems: vec![
                SubsystemInfo {
                    name: "runtime".to_string(),
                    status: SubsystemStatus::Implemented,
                    capabilities: vec![
                        "sessions".to_string(),
                        "agents".to_string(),
                        "tools".to_string(),
                        "mcp".to_string(),
                        "skills".to_string(),
                        "memory".to_string(),
                        "context".to_string(),
                    ],
                },
                SubsystemInfo {
                    name: "evolution".to_string(),
                    status: SubsystemStatus::Implemented,
                    capabilities: vec![
                        "experience_collection".to_string(),
                        "layered_memory".to_string(),
                        "reflection".to_string(),
                        "skill_evolution".to_string(),
                        "optimization".to_string(),
                    ],
                },
            ],
            version: "0.1.0".to_string(),
            rust_version: "1.86".to_string(),
        })
    }

    fn inspect_capabilities(&self) -> Result<CapabilityInventory> {
        Ok(CapabilityInventory {
            planning: true,
            task_decomposition: true,
            memory_retrieval: true,
            context_management: true,
            tool_execution: true,
            agent_delegation: true,
            hive_collaboration: true,
            mcp_integration: true,
            skill_system: true,
            learning: true,
            evolution: true,
        })
    }

    fn inspect_tools(&self) -> Result<ToolInventory> {
        Ok(ToolInventory {
            builtin: vec![
                ToolInfo {
                    name: "read_file".to_string(),
                    description: "Read file contents".to_string(),
                    enabled: true,
                    permission_shape: "read".to_string(),
                },
                ToolInfo {
                    name: "write_file".to_string(),
                    description: "Write file contents".to_string(),
                    enabled: true,
                    permission_shape: "write".to_string(),
                },
                ToolInfo {
                    name: "edit_file".to_string(),
                    description: "Edit file with string replacement".to_string(),
                    enabled: true,
                    permission_shape: "write".to_string(),
                },
                ToolInfo {
                    name: "run_shell".to_string(),
                    description: "Execute shell command".to_string(),
                    enabled: true,
                    permission_shape: "execute".to_string(),
                },
            ],
            mcp: vec![],
            plugin: vec![],
        })
    }

    /// Provider/model inventory built from recorded signals only.
    ///
    /// Scans the newest experiences' `metadata.config` object — the shape
    /// `vikram-runtime`'s `config_signals` writes (`model`, `provider`,
    /// `fallback_model`, `context_window`). Newest signal wins for
    /// `current_model`/`observed_provider`/`context_window`; every
    /// distinct observed model name becomes a [`ModelInfo`] with no
    /// claimed capabilities or window. With no recorded signals the
    /// inventory is empty and `status` is [`ProviderStatus::Unknown`].
    fn inspect_providers(&self) -> Result<ProviderInventory> {
        let mut current_model = String::new();
        let mut observed_provider: Option<String> = None;
        let mut context_window: Option<u64> = None;
        let mut models: Vec<ModelInfo> = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        for exp in self.store.list_experiences(None, 200)? {
            let Some(config) = exp.metadata.get("config") else {
                continue;
            };
            if current_model.is_empty()
                && let Some(m) = config.get("model").and_then(|v| v.as_str())
            {
                current_model = m.to_string();
            }
            if observed_provider.is_none()
                && let Some(p) = config.get("provider").and_then(|v| v.as_str())
            {
                observed_provider = Some(p.to_string());
            }
            if context_window.is_none()
                && let Some(w) = config.get("context_window").and_then(json_u64)
            {
                context_window = Some(w);
            }
            for key in ["model", "fallback_model"] {
                if let Some(m) = config.get(key).and_then(|v| v.as_str())
                    && !seen.iter().any(|s| s.as_str() == m)
                {
                    seen.push(m.to_string());
                    models.push(ModelInfo {
                        name: m.to_string(),
                        context_window: None,
                        capabilities: vec![],
                    });
                }
            }
        }

        Ok(ProviderInventory {
            base_url: String::new(),
            current_model,
            available_models: models,
            context_window,
            status: ProviderStatus::Unknown,
            observed_provider,
        })
    }

    fn inspect_agents(&self) -> Result<AgentInventory> {
        Ok(AgentInventory {
            main_agent: AgentInfo {
                id: "vikram".to_string(),
                name: "Vikram".to_string(),
                role: "main_agent".to_string(),
                specialization: vec![],
                // No runtime handle is reachable from the store, so the
                // model and liveness of the main agent are unknown rather
                // than the "gpt-4"/"active" this used to fabricate.
                model: None,
                status: "unknown".to_string(),
            },
            sub_agents: vec![],
            bots: vec![],
            hive_teams: vec![],
        })
    }

    fn inspect_memory(&self) -> Result<MemoryInventory> {
        Ok(MemoryInventory {
            sqlite_memory: true,
            layered_memory: true,
            episodic: true,
            semantic: true,
            procedural: true,
            user_memory: true,
            project_memory: true,
            agent_memory: true,
        })
    }

    fn inspect_context(&self) -> Result<ContextInventory> {
        Ok(ContextInventory {
            context_loader: true,
            token_budgeting: true,
            mtime_invalidation: true,
            deduplication: true,
            caching: true,
        })
    }

    fn inspect_limitations(&self) -> Result<KnownLimitations> {
        Ok(KnownLimitations {
            token_limits: vec!["Context window limited by model".to_string()],
            tool_constraints: vec![
                "Tool execution requires permission".to_string(),
                "Network access requires permission".to_string(),
            ],
            provider_constraints: vec!["Dependent on external API availability".to_string()],
            resource_constraints: vec!["Limited by system resources".to_string()],
        })
    }

    /// Known bugs/issues, derived from recorded store state.
    ///
    /// `open_issues` are the pending `record_issue` proposals — failures
    /// the learning loop already flagged and nobody has resolved.
    /// `known_failures` are `task_category`s with recorded failures in the
    /// default window (frequency = failure count, `last_occurred` = newest
    /// experience timestamp for that category) plus failed reflection
    /// jobs, whose `updated_ms` is the real last-failure time.
    fn inspect_bugs(&self) -> Result<KnownBugs> {
        let open_issues = self
            .store
            .list_proposals(Some("pending"))?
            .into_iter()
            .filter(|p| p.kind == ProposalKind::RecordIssue)
            .map(|p| IssueInfo {
                id: p.id,
                description: p.summary,
                severity: "untriaged".to_string(),
                status: p.status.as_str().to_string(),
            })
            .collect();

        let since = now_ms().saturating_sub(metrics::DEFAULT_WINDOW_MS);
        let mut known_failures = Vec::new();
        for category in self.store.category_stats(Some(since), 10)? {
            if category.failures == 0 {
                continue;
            }
            let (_, last) = self
                .store
                .experience_time_bounds_for(Some(&category.category))?;
            known_failures.push(FailurePattern {
                pattern: category.category,
                frequency: category.failures.min(u32::MAX as usize) as u32,
                last_occurred: last.unwrap_or(0),
            });
        }
        let failed_jobs = self.store.list_failed_jobs(50)?;
        if !failed_jobs.is_empty() {
            known_failures.push(FailurePattern {
                pattern: "reflection_job_failed".to_string(),
                frequency: failed_jobs.len().min(u32::MAX as usize) as u32,
                last_occurred: failed_jobs.iter().map(|j| j.updated_ms).max().unwrap_or(0),
            });
        }

        Ok(KnownBugs {
            open_issues,
            known_failures,
        })
    }

    /// Feature lists reflect the documented surface status in
    /// `docs/evolution-surface.md`, not a runtime probe.
    fn inspect_features(&self) -> Result<FeatureStatus> {
        Ok(FeatureStatus {
            fully_implemented: vec![
                "experience_collection".to_string(),
                "layered_memory".to_string(),
                "reflection".to_string(),
                "skill_evolution".to_string(),
                "optimization".to_string(),
                "hive_learning".to_string(),
                "self_recovery_snapshots".to_string(),
            ],
            partially_implemented: vec![
                "self_awareness".to_string(),
                "self_configuration".to_string(),
                "self_adaptation".to_string(),
                "self_development".to_string(),
                "self_evaluation".to_string(),
                "self_improvement".to_string(),
                "mapek_loop".to_string(),
                "conversational_control".to_string(),
                "autonomy".to_string(),
            ],
            missing: vec!["autonomous_source_modification".to_string()],
        })
    }

    fn inspect_resources(&self) -> Result<ResourceSnapshot> {
        Ok(ResourceSnapshot {
            cpu_usage: None,
            memory_usage: None,
            disk_usage: None,
            network_available: true,
            sandbox_available: true,
        })
    }

    /// Aggregate what the store actually holds: experience/proposal/
    /// reflection tallies, live memory counts per layer, recorded skill
    /// versions, and learning-event totals. Every number is a real
    /// aggregate; an empty store yields zeros and `None` rates.
    fn inspect_store_state(&self) -> Result<StoreAwareness> {
        let mut memory_by_layer: BTreeMap<String, usize> = BTreeMap::new();
        for entry in self.store.list_memory(None)? {
            *memory_by_layer
                .entry(entry.layer.as_str().to_string())
                .or_default() += 1;
        }

        Ok(StoreAwareness {
            experiences: self.store.experience_stats(metrics::DEFAULT_WINDOW_MS)?,
            proposals: self.store.proposal_stats()?,
            reflection_jobs: self.store.reflection_stats()?,
            live_memories: self.store.count_memory()?,
            memory_by_layer,
            learning_events: self.store.count_events()?,
            skill_versions: self.store.latest_skill_versions()?,
        })
    }

    fn save_model(&self, model: &SelfModel) -> Result<()> {
        self.store
            .save_engine_record(MODEL_NAMESPACE, MODEL_RECORD_ID, &to_json_value(model)?)?;
        self.store.save_event(&LearningEvent {
            id: gen_id("evt"),
            timestamp_ms: now_ms(),
            agent_id: "system".to_string(),
            kind: "self_model_update".to_string(),
            summary: "Self-model updated".to_string(),
            payload: serde_json::json!({"model_id": model.id, "version": model.version}),
        })?;
        Ok(())
    }

    /// The last persisted self-model, if [`Self::inspect_runtime`] has
    /// ever run against this store. A payload that does not decode is an
    /// error, not a silently-empty model.
    fn load_model(&self) -> Result<Option<SelfModel>> {
        let records = self.store.list_engine_records(MODEL_NAMESPACE)?;
        for (id, payload) in records {
            if id != MODEL_RECORD_ID {
                continue;
            }
            return Ok(Some(from_json_value(payload)?));
        }
        Ok(None)
    }
}

/// `engine_records` namespace + record id holding the latest [`SelfModel`].
const MODEL_NAMESPACE: &str = "self_awareness.model";
const MODEL_RECORD_ID: &str = "latest";

/// Numeric config signal: producers may record `context_window` as a
/// JSON number or a numeric string.
fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Experience, ExperienceOutcome, KnowledgeStatus, LearningProposal, MemoryEntry, MemoryLayer,
        Sensitivity, SkillVersion,
    };

    fn engine(store: EvolutionStore) -> SelfAwarenessEngine {
        SelfAwarenessEngine::new(store)
    }

    fn experience(
        id: &str,
        category: &str,
        outcome: ExperienceOutcome,
        metadata: serde_json::Value,
    ) -> Experience {
        Experience {
            id: id.to_string(),
            timestamp_ms: now_ms(),
            agent_id: "agent-1".to_string(),
            project_id: None,
            task_category: category.to_string(),
            user_request: "do a thing".to_string(),
            outcome,
            tools_used: vec![],
            errors: vec![],
            corrections: vec![],
            feedback: None,
            reusable: false,
            sensitivity: Sensitivity::Private,
            metadata,
        }
    }

    fn memory(id: &str, layer: MemoryLayer, key: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            layer,
            scope: "global".to_string(),
            key: key.to_string(),
            value: "v".to_string(),
            status: KnowledgeStatus::Observed,
            confidence: 0.5,
            source_experience: None,
            provenance: "test".to_string(),
            created_ms: now_ms(),
            updated_ms: now_ms(),
            expires_ms: None,
            superseded_by: None,
        }
    }

    #[test]
    fn empty_store_produces_zeroed_store_state_not_constants() {
        let store = EvolutionStore::open_in_memory().unwrap();
        let mut engine = engine(store);
        let model = engine.inspect_runtime().unwrap();

        assert_eq!(model.store_state.experiences.all_time.total, 0);
        assert_eq!(model.store_state.live_memories, 0);
        assert!(model.store_state.skill_versions.is_empty());
        assert!(model.store_state.memory_by_layer.is_empty());
        assert_eq!(model.providers.status, ProviderStatus::Unknown);
        assert!(model.providers.current_model.is_empty());
        assert!(model.providers.available_models.is_empty());
        assert!(model.bugs.open_issues.is_empty());
        assert!(model.bugs.known_failures.is_empty());
    }

    #[test]
    fn inspect_reports_real_store_data() {
        let store = EvolutionStore::open_in_memory().unwrap();
        store
            .save_experience(&experience(
                "exp-1",
                "coding",
                ExperienceOutcome::Failure,
                serde_json::json!({"config": {
                    "model": "gpt-4o",
                    "provider": "openrouter",
                    "context_window": 128000
                }}),
            ))
            .unwrap();
        store
            .save_experience(&experience(
                "exp-2",
                "coding",
                ExperienceOutcome::Success,
                serde_json::json!({}),
            ))
            .unwrap();
        store
            .save_proposal(&LearningProposal {
                id: "prop-1".to_string(),
                experience_id: None,
                agent_id: "self_adaptation".to_string(),
                kind: ProposalKind::RecordIssue,
                summary: "tool flakiness".to_string(),
                details: serde_json::json!({}),
                confidence: 1.0,
                requires_approval: true,
                created_ms: now_ms(),
                status: crate::types::ProposalStatus::Pending,
            })
            .unwrap();
        store
            .save_memory(&memory("mem-1", MemoryLayer::Semantic, "fact"))
            .unwrap();
        store
            .save_skill_version(&SkillVersion {
                skill_name: "demo".to_string(),
                version: 3,
                content: "body".to_string(),
                created_ms: now_ms(),
                parent_version: Some(2),
                change_description: "iter".to_string(),
            })
            .unwrap();

        let mut engine = engine(store);
        let model = engine.inspect_runtime().unwrap();

        // Provider signals come from recorded experience metadata.
        assert_eq!(model.providers.current_model, "gpt-4o");
        assert_eq!(
            model.providers.observed_provider.as_deref(),
            Some("openrouter")
        );
        assert_eq!(model.providers.context_window, Some(128000));
        assert_eq!(model.providers.available_models.len(), 1);
        assert_eq!(model.providers.available_models[0].name, "gpt-4o");
        assert_eq!(model.providers.status, ProviderStatus::Unknown);

        // Bugs reflect real pending proposals and recorded failures.
        assert_eq!(model.bugs.open_issues.len(), 1);
        assert_eq!(model.bugs.open_issues[0].id, "prop-1");
        assert!(
            model
                .bugs
                .known_failures
                .iter()
                .any(|f| f.pattern == "coding" && f.frequency == 1)
        );

        // Store aggregates are real counts.
        assert_eq!(model.store_state.experiences.all_time.total, 2);
        assert_eq!(model.store_state.experiences.all_time.failure, 1);
        assert_eq!(model.store_state.proposals.pending, 1);
        assert_eq!(model.store_state.proposals.pending_record_issue, 1);
        assert_eq!(model.store_state.live_memories, 1);
        assert_eq!(model.store_state.memory_by_layer.get("semantic"), Some(&1));
        assert_eq!(
            model.store_state.skill_versions,
            vec![("demo".to_string(), 3)]
        );
    }

    #[test]
    fn self_model_persists_and_version_increments_across_engines() {
        let store = EvolutionStore::open_in_memory().unwrap();
        {
            let mut engine = engine(store.clone());
            let first = engine.inspect_runtime().unwrap();
            assert_eq!(first.version, 1);
        }
        // A fresh engine over the same store sees the persisted model.
        let restored = engine(store.clone()).get_self_model().unwrap();
        assert_eq!(restored.version, 1);

        let mut engine = engine(store);
        let second = engine.inspect_runtime().unwrap();
        assert_eq!(second.version, 2);
    }
}
