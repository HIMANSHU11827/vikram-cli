//! Self-Configuration Engine: Independent management of configurable parts.
//!
//! This engine allows Vikram to independently manage models, providers, runtime
//! settings, context, interface, agents, tools, and MCP integrations.
//!
//! Durable and host-surface scoped: `change_history` is persisted to
//! `engine_records` under `self_config.change` and rehydrated on the first
//! mutating call, so recorded intent survives a restart.
//!
//! With a [`ConfigSurface`] attached, `apply_change` and `rollback_change` are
//! real writes and `applied` tracks them. Without one — the daemon and the CLI
//! today — every apply is refused with an honest reason and a record stays
//! intent only.

use crate::error::{EvolutionError, Result};
use crate::store::{EvolutionStore, from_json_value, to_json_value};
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};

/// `engine_records` namespace holding every recorded [`ConfigChange`].
const CHANGE_NAMESPACE: &str = "self_config.change";

/// The live configuration a host hands to this engine.
///
/// `vikram-evolution` cannot depend on `vikram-runtime` (the dependency runs the other way), so the
/// engine cannot reach into a settings file itself. A host supplies this instead. The surface, not
/// the engine, decides which keys may be written: the engine asks by name and accepts a refusal.
/// Keys that change what the assistant is *allowed* to do are never writable — a learning engine
/// must not be able to widen its own permissions.
pub trait ConfigSurface: Send + Sync {
    /// The current value of `key`, or `None` when the surface does not know that key.
    fn read(&self, key: &str) -> Option<String>;

    /// Write `value` for `key`. Refusals carry the reason the engine reports back to the operator.
    fn write(&self, key: &str, value: &str) -> std::result::Result<(), String>;

    /// Whether this key may be written at all. Defaults to refusing every key.
    fn writable(&self, key: &str) -> bool {
        let _ = key;
        false
    }
}

/// Configuration target types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigTarget {
    Model(String),
    Provider(String),
    Runtime(String),
    Context(String),
    Interface(String),
    Agent(String),
    Tool(String),
    Mcp(String),
}

impl ConfigTarget {
    /// The surface key this target addresses, e.g. `context:memory_budget_tokens`.
    pub fn key(&self) -> String {
        match self {
            Self::Model(name) => format!("model:{name}"),
            Self::Provider(name) => format!("provider:{name}"),
            Self::Runtime(name) => format!("runtime:{name}"),
            Self::Context(name) => format!("context:{name}"),
            Self::Interface(name) => format!("interface:{name}"),
            Self::Agent(name) => format!("agent:{name}"),
            Self::Tool(name) => format!("tool:{name}"),
            Self::Mcp(name) => format!("mcp:{name}"),
        }
    }

    /// Read a surface key back into a target, so a command line can name one.
    pub fn parse(key: &str) -> Option<Self> {
        let (family, name) = key.trim().split_once(':')?;
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        match family {
            "model" => Some(Self::Model(name.to_owned())),
            "provider" => Some(Self::Provider(name.to_owned())),
            "runtime" => Some(Self::Runtime(name.to_owned())),
            "context" => Some(Self::Context(name.to_owned())),
            "interface" => Some(Self::Interface(name.to_owned())),
            "agent" => Some(Self::Agent(name.to_owned())),
            "tool" => Some(Self::Tool(name.to_owned())),
            "mcp" => Some(Self::Mcp(name.to_owned())),
            _ => None,
        }
    }
}

/// Configuration change record.
///
/// `applied` becomes `true` only when a host surface accepted the write (see
/// [`SelfConfigurationEngine::apply_change`]); a rollback clears it again. A
/// record with `applied = false` is recorded intent, not a claim that this
/// engine mutated anything. `previous_value`/`rollback_value` describe the
/// transition a rollback would restore.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigChange {
    pub id: String,
    pub target: ConfigTarget,
    pub previous_value: Option<String>,
    pub new_value: String,
    pub reason: String,
    pub created_ms: u64,
    /// Whether a host surface accepted the write. `false` for a recorded
    /// intent and again after a rollback.
    pub applied: bool,
    pub rollback_value: Option<String>,
}

/// Model selection criteria.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelSelectionCriteria {
    pub task_type: String,
    pub complexity: String,
    pub latency_requirement: Option<String>,
    pub cost_constraint: Option<String>,
    pub token_budget: Option<u64>,
}

/// Runtime settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeSettings {
    pub timeout_ms: Option<u64>,
    pub max_concurrent_ops: Option<u32>,
    pub retry_strategy: String,
    pub resource_limits: ResourceLimits,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_memory_mb: Option<u64>,
    pub max_cpu_percent: Option<f32>,
    pub max_disk_mb: Option<u64>,
}

/// Context management settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextSettings {
    pub budget_strategy: String,
    pub compression_enabled: bool,
    pub deduplication_enabled: bool,
    pub cache_size_mb: Option<u64>,
}

/// Agent configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub agent_id: String,
    pub model: Option<String>,
    pub tools: Vec<String>,
    pub permissions: Vec<String>,
    pub specialization: Vec<String>,
}

pub struct SelfConfigurationEngine {
    store: EvolutionStore,
    change_history: Vec<ConfigChange>,
    /// Whether `change_history` has been rehydrated from `engine_records`.
    hydrated: bool,
    /// The live settings this engine may actually change, supplied by the host.
    surface: Option<std::sync::Arc<dyn ConfigSurface>>,
}

impl SelfConfigurationEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            change_history: Vec::new(),
            hydrated: false,
            surface: None,
        }
    }

    /// Give the engine a live surface. Without one it stays intent-only and every apply is refused
    /// with an honest reason, which is what the daemon and the CLI run as.
    pub fn attach_surface(&mut self, surface: std::sync::Arc<dyn ConfigSurface>) {
        self.surface = Some(surface);
    }

    pub fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    /// The live surface, if a host attached one.
    pub fn surface(&self) -> Option<std::sync::Arc<dyn ConfigSurface>> {
        self.surface.clone()
    }

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
    }

    /// Load persisted changes into memory once. Mutating entry points call
    /// this first so post-restart callers see the durable history; a
    /// corrupt payload is an error, not a silently-empty list.
    fn hydrate(&mut self) -> Result<()> {
        if self.hydrated {
            return Ok(());
        }
        let mut changes = Vec::new();
        for (id, payload) in self.store.list_engine_records(CHANGE_NAMESPACE)? {
            let mut change: ConfigChange = from_json_value(payload)?;
            if change.id.is_empty() {
                change.id = id;
            }
            changes.push(change);
        }
        changes.sort_by(|a, b| {
            a.created_ms
                .cmp(&b.created_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        self.change_history = changes;
        self.hydrated = true;
        Ok(())
    }

    /// Select optimal model for a task based on criteria.
    pub fn select_model_for_task(
        &mut self,
        criteria: &ModelSelectionCriteria,
        available_models: &[String],
    ) -> Result<String> {
        let selected = if criteria.latency_requirement.as_deref() == Some("low") {
            available_models
                .iter()
                .find(|m| m.contains("3.5") || m.contains("turbo"))
                .cloned()
        } else if criteria.complexity == "high" {
            available_models
                .iter()
                .find(|m| m.contains("4") || m.contains("claude"))
                .cloned()
        } else {
            available_models.first().cloned()
        };

        selected.ok_or_else(|| {
            EvolutionError::NotFound("No suitable model found for criteria".to_string())
        })
    }

    /// Configure provider settings.
    pub fn configure_provider(
        &mut self,
        provider_id: &str,
        config: serde_json::Value,
        reason: &str,
    ) -> Result<ConfigChange> {
        let change = ConfigChange {
            id: gen_id("config"),
            target: ConfigTarget::Provider(provider_id.to_string()),
            previous_value: None,
            new_value: config.to_string(),
            reason: reason.to_string(),
            created_ms: now_ms(),
            applied: false,
            rollback_value: None,
        };

        self.record_change(change.clone())?;
        Ok(change)
    }

    /// Set runtime setting.
    pub fn set_runtime_setting(
        &mut self,
        setting: &str,
        value: &str,
        reason: &str,
    ) -> Result<ConfigChange> {
        let change = ConfigChange {
            id: gen_id("config"),
            target: ConfigTarget::Runtime(setting.to_string()),
            previous_value: None,
            new_value: value.to_string(),
            reason: reason.to_string(),
            created_ms: now_ms(),
            applied: false,
            rollback_value: None,
        };

        self.record_change(change.clone())?;
        Ok(change)
    }

    /// Manage context budget strategy.
    pub fn manage_context_budget(&mut self, strategy: &str, reason: &str) -> Result<ConfigChange> {
        let change = ConfigChange {
            id: gen_id("config"),
            target: ConfigTarget::Context("budget_strategy".to_string()),
            previous_value: None,
            new_value: strategy.to_string(),
            reason: reason.to_string(),
            created_ms: now_ms(),
            applied: false,
            rollback_value: None,
        };

        self.record_change(change.clone())?;
        Ok(change)
    }

    /// Configure agent settings.
    pub fn configure_agent(&mut self, config: &AgentConfig, reason: &str) -> Result<ConfigChange> {
        let value =
            serde_json::to_string(config).map_err(|e| EvolutionError::Serde(e.to_string()))?;

        let change = ConfigChange {
            id: gen_id("config"),
            target: ConfigTarget::Agent(config.agent_id.clone()),
            previous_value: None,
            new_value: value,
            reason: reason.to_string(),
            created_ms: now_ms(),
            applied: false,
            rollback_value: None,
        };

        self.record_change(change.clone())?;
        Ok(change)
    }

    /// Enable a tool.
    pub fn enable_tool(&mut self, tool_name: &str, reason: &str) -> Result<ConfigChange> {
        let change = ConfigChange {
            id: gen_id("config"),
            target: ConfigTarget::Tool(tool_name.to_string()),
            previous_value: Some("disabled".to_string()),
            new_value: "enabled".to_string(),
            reason: reason.to_string(),
            created_ms: now_ms(),
            applied: false,
            rollback_value: Some("disabled".to_string()),
        };

        self.record_change(change.clone())?;
        Ok(change)
    }

    /// Disable a tool.
    pub fn disable_tool(&mut self, tool_name: &str, reason: &str) -> Result<ConfigChange> {
        let change = ConfigChange {
            id: gen_id("config"),
            target: ConfigTarget::Tool(tool_name.to_string()),
            previous_value: Some("enabled".to_string()),
            new_value: "disabled".to_string(),
            reason: reason.to_string(),
            created_ms: now_ms(),
            applied: false,
            rollback_value: Some("enabled".to_string()),
        };

        self.record_change(change.clone())?;
        Ok(change)
    }

    /// Record an intent to set one target to `value`, without writing it.
    ///
    /// Used by [`crate::self_trial`], which needs the intent record before it
    /// applies and measures the change.
    pub fn record_target_change(
        &mut self,
        target: ConfigTarget,
        value: &str,
        reason: &str,
    ) -> Result<ConfigChange> {
        let change = ConfigChange {
            id: gen_id("config"),
            target,
            previous_value: None,
            new_value: value.to_string(),
            reason: reason.to_string(),
            created_ms: now_ms(),
            applied: false,
            rollback_value: None,
        };

        self.record_change(change.clone())?;
        Ok(change)
    }

    /// Validate current configuration.
    ///
    /// Hydrates the persisted history first, so a fresh engine reports the
    /// durable pending changes, not just this session's.
    pub fn validate_configuration(&mut self) -> Result<Vec<String>> {
        self.hydrate()?;
        let mut issues = Vec::new();

        for change in &self.change_history {
            if !change.applied {
                issues.push(format!(
                    "Pending change not applied: {:?} = {}",
                    change.target, change.new_value
                ));
            }
        }

        Ok(issues)
    }

    /// Roll back a configuration change.
    ///
    /// Only a change this engine actually applied can be rolled back: the surface is asked to
    /// restore the value that was live before the write. An intent-only record (never applied)
    /// reports that honestly instead of fabricating a restoration. Unknown ids are rejected.
    pub fn rollback_change(&mut self, change_id: &str) -> Result<()> {
        self.hydrate()?;
        let index = self
            .change_history
            .iter()
            .position(|c| c.id == change_id)
            .ok_or_else(|| {
                EvolutionError::NotFound(format!("configuration change {change_id} not found"))
            })?;
        let change = self.change_history[index].clone();
        if !change.applied {
            return Err(EvolutionError::Validation(format!(
                "configuration change {change_id} was never applied to a live target; \
                 it is a recorded intent only and there is nothing to roll back"
            )));
        }
        let surface = self.surface.clone().ok_or_else(|| {
            EvolutionError::Validation(format!(
                "configuration change {change_id} was applied by a host that is no longer \
                 attached; refusing to guess where to restore it"
            ))
        })?;
        let key = change.target.key();
        let restore = change
            .rollback_value
            .clone()
            .or_else(|| change.previous_value.clone())
            .ok_or_else(|| {
                EvolutionError::Validation(format!(
                    "configuration change {change_id} recorded no previous value for {key}"
                ))
            })?;
        surface
            .write(&key, &restore)
            .map_err(EvolutionError::Validation)?;

        let mut rolled_back = change;
        rolled_back.applied = false;
        self.save_change(&rolled_back)?;
        self.change_history[index] = rolled_back;
        Ok(())
    }

    /// Get change history.
    ///
    /// Hydrates the persisted history first, so a fresh engine returns the
    /// durable record, not an empty slice.
    pub fn get_change_history(&mut self) -> Result<&[ConfigChange]> {
        self.hydrate()?;
        Ok(&self.change_history)
    }

    /// Apply a configuration change.
    ///
    /// With a surface attached this is a real write: the live value is read first (so a rollback
    /// has something true to restore), the surface is asked to change it, and only then is the
    /// record marked applied. Without a surface — the daemon, the CLI — every apply is refused
    /// with the honest reason that nothing intends to mutate. The surface also gets a veto: a key
    /// that would widen what the assistant is permitted to do is refused here, not written.
    pub fn apply_change(&mut self, change_id: &str) -> Result<()> {
        self.hydrate()?;
        let index = self
            .change_history
            .iter()
            .position(|c| c.id == change_id)
            .ok_or_else(|| {
                EvolutionError::NotFound(format!("configuration change {change_id} not found"))
            })?;
        let change = self.change_history[index].clone();
        if change.applied {
            return Ok(());
        }
        let key = change.target.key();
        let surface = self.surface.clone().ok_or_else(|| {
            EvolutionError::Validation(format!(
                "configuration change {change_id} targets {key} but this engine has \
                 no live configuration surface attached; it remains recorded \
                 intent only (applied=false)"
            ))
        })?;
        if !surface.writable(&key) {
            return Err(EvolutionError::Validation(format!(
                "the live surface refuses {key}: a learning engine may not change \
                 what the assistant is allowed to do"
            )));
        }
        let previous = surface.read(&key);
        surface
            .write(&key, &change.new_value)
            .map_err(EvolutionError::Validation)?;

        let mut applied = change;
        applied.applied = true;
        if previous.is_some() {
            applied.previous_value = previous.clone();
            applied.rollback_value = previous;
        }
        self.save_change(&applied)?;
        self.change_history[index] = applied;
        Ok(())
    }

    /// Persist the change, then record it in memory. Persistence comes
    /// first so a store failure never leaves memory claiming a change the
    /// durable record does not have.
    fn record_change(&mut self, change: ConfigChange) -> Result<()> {
        self.hydrate()?;
        self.store
            .save_engine_record(CHANGE_NAMESPACE, &change.id, &to_json_value(&change)?)?;
        self.change_history.push(change);
        Ok(())
    }

    fn save_change(&self, change: &ConfigChange) -> Result<()> {
        self.store
            .save_engine_record(CHANGE_NAMESPACE, &change.id, &to_json_value(change)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::EvolutionStore;

    fn engine() -> SelfConfigurationEngine {
        SelfConfigurationEngine::new(EvolutionStore::open_in_memory().unwrap())
    }

    #[test]
    fn configure_methods_record_intent_without_applying() {
        let mut engine = engine();
        let change = engine
            .set_runtime_setting("timeout_ms", "5000", "tune")
            .unwrap();
        assert!(!change.applied);
        assert_eq!(engine.get_change_history().unwrap().len(), 1);
    }

    #[test]
    fn change_history_survives_a_new_engine_over_the_same_store() {
        let store = EvolutionStore::open_in_memory().unwrap();
        {
            let mut engine = SelfConfigurationEngine::new(store.clone());
            engine.enable_tool("read_file", "needed").unwrap();
        }
        // A fresh engine rehydrates the durable record on first use.
        let mut engine = SelfConfigurationEngine::new(store);
        let history = engine.get_change_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].new_value, "enabled");
        assert!(!history[0].applied);
    }

    #[test]
    fn apply_and_rollback_reject_unknown_ids() {
        let mut engine = engine();
        assert!(engine.apply_change("config-nope").is_err());
        assert!(engine.rollback_change("config-nope").is_err());
    }

    #[test]
    fn apply_reports_intent_only_instead_of_fake_mutation() {
        let mut engine = engine();
        let change = engine.enable_tool("read_file", "needed").unwrap();

        let err = engine.apply_change(&change.id).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no live configuration surface"), "{msg}");

        // The record must remain unapplied — nothing was mutated.
        assert!(!engine.get_change_history().unwrap()[0].applied);
    }

    #[test]
    fn rollback_reports_nothing_to_restore() {
        let mut engine = engine();
        let change = engine.disable_tool("write_file", "unused").unwrap();

        let err = engine.rollback_change(&change.id).unwrap_err();
        assert!(err.to_string().contains("nothing to roll back"));

        // The recorded intent is untouched: no fabricated value swap.
        let stored = &engine.get_change_history().unwrap()[0];
        assert_eq!(stored.new_value, "disabled");
        assert_eq!(stored.rollback_value.as_deref(), Some("enabled"));
    }

    /// A minimal host surface: a map plus the prefixes it is willing to write.
    struct FakeSurface {
        values: std::sync::Mutex<std::collections::HashMap<String, String>>,
        writable: &'static [&'static str],
    }

    impl FakeSurface {
        fn new(writable: &'static [&'static str]) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                values: std::sync::Mutex::new(std::collections::HashMap::new()),
                writable,
            })
        }

        fn get(&self, key: &str) -> Option<String> {
            self.values.lock().unwrap().get(key).cloned()
        }
    }

    impl ConfigSurface for FakeSurface {
        fn read(&self, key: &str) -> Option<String> {
            self.get(key)
        }

        fn write(&self, key: &str, value: &str) -> std::result::Result<(), String> {
            self.values
                .lock()
                .unwrap()
                .insert(key.to_owned(), value.to_owned());
            Ok(())
        }

        fn writable(&self, key: &str) -> bool {
            self.writable.iter().any(|prefix| key.starts_with(prefix))
        }
    }

    fn engine_with(surface: std::sync::Arc<FakeSurface>) -> SelfConfigurationEngine {
        let mut engine = SelfConfigurationEngine::new(EvolutionStore::open_in_memory().unwrap());
        engine.attach_surface(surface);
        engine
    }

    #[test]
    fn an_attached_surface_makes_an_apply_real() {
        let surface = FakeSurface::new(&["context:"]);
        let mut engine = engine_with(surface.clone());
        let change = engine
            .set_runtime_setting("memory_budget_tokens", "2400", "memory got tight")
            .unwrap();

        // `set_runtime_setting` records a Runtime target; address it as context to hit the
        // writable prefix, which is exactly the key the host would receive.
        let mut change = change;
        change.target = ConfigTarget::Context("memory_budget_tokens".to_owned());
        engine.change_history[0] = change.clone();

        engine.apply_change(&change.id).unwrap();
        assert_eq!(
            surface.get("context:memory_budget_tokens").as_deref(),
            Some("2400")
        );
        assert!(engine.get_change_history().unwrap()[0].applied);
    }

    #[test]
    fn a_rollback_restores_the_value_the_surface_had_before() {
        let surface = FakeSurface::new(&["context:"]);
        surface
            .write("context:memory_budget_tokens", "1200")
            .unwrap();
        let mut engine = engine_with(surface.clone());
        engine.change_history.push(ConfigChange {
            id: "config-1".to_owned(),
            target: ConfigTarget::Context("memory_budget_tokens".to_owned()),
            previous_value: None,
            new_value: "2400".to_owned(),
            reason: "more recall".to_owned(),
            created_ms: 1,
            applied: false,
            rollback_value: None,
        });
        engine.hydrated = true;

        engine.apply_change("config-1").unwrap();
        assert_eq!(
            surface.get("context:memory_budget_tokens").as_deref(),
            Some("2400")
        );

        engine.rollback_change("config-1").unwrap();
        assert_eq!(
            surface.get("context:memory_budget_tokens").as_deref(),
            Some("1200")
        );
        assert!(!engine.get_change_history().unwrap()[0].applied);
    }

    #[test]
    fn the_surface_vetoes_a_key_that_would_widen_permissions() {
        let surface = FakeSurface::new(&["context:"]);
        let mut engine = engine_with(surface);
        let change = engine.enable_tool("shell", "needed for the build").unwrap();

        let err = engine.apply_change(&change.id).unwrap_err();
        assert!(
            err.to_string()
                .contains("may not change what the assistant is allowed to do"),
            "{err}"
        );
        assert!(!engine.get_change_history().unwrap()[0].applied);
    }
}
