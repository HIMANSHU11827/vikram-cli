//! Self-Configuration Engine: Independent management of configurable parts.
//!
//! This engine allows Vikram to independently manage models, providers, runtime
//! settings, context, interface, agents, tools, and MCP integrations.
//!
//! DORMANT / process-local: nothing in the runtime or CLI calls this engine,
//! and `change_history` is an in-memory `Vec` — a restart loses every
//! recorded change. The engine only ever records *intent*; see
//! [`ConfigChange::applied`].

use crate::error::{EvolutionError, Result};
use crate::store::EvolutionStore;
use crate::types::{gen_id, now_ms};
use serde::{Deserialize, Serialize};

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

/// Configuration change record.
///
/// This crate has no live configuration surface: nothing outside this
/// in-memory history reads these records, so `applied` can never become
/// `true` through [`SelfConfigurationEngine::apply_change`] — see that
/// method for why. `previous_value`/`rollback_value` describe the *intended*
/// transition only; no runtime value is ever mutated.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigChange {
    pub id: String,
    pub target: ConfigTarget,
    pub previous_value: Option<String>,
    pub new_value: String,
    pub reason: String,
    pub created_ms: u64,
    /// Whether the change was applied to a live configuration target.
    /// Always `false` today: this engine only records intent.
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
}

impl SelfConfigurationEngine {
    pub fn new(store: EvolutionStore) -> Self {
        Self {
            store,
            change_history: Vec::new(),
        }
    }

    /// The shared store every subsystem reads and writes through.
    pub fn store(&self) -> &EvolutionStore {
        &self.store
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

        self.record_change(change.clone());
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

        self.record_change(change.clone());
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

        self.record_change(change.clone());
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

        self.record_change(change.clone());
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

        self.record_change(change.clone());
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

        self.record_change(change.clone());
        Ok(change)
    }

    /// Validate current configuration.
    pub fn validate_configuration(&self) -> Result<Vec<String>> {
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
    /// Because [`Self::apply_change`] never mutates a live target, an
    /// applied change never exists and there is nothing concrete to restore.
    /// Swapping `new_value`/`rollback_value` on the record would fabricate a
    /// restoration that never happened, so this reports the situation
    /// honestly instead. Unknown change ids are rejected.
    pub fn rollback_change(&mut self, change_id: &str) -> Result<()> {
        let change = self
            .change_history
            .iter()
            .find(|c| c.id == change_id)
            .ok_or_else(|| {
                EvolutionError::NotFound(format!("configuration change {change_id} not found"))
            })?;

        if change.applied {
            return Err(EvolutionError::Validation(format!(
                "configuration change {change_id} reports applied=true, which this \
                 engine cannot produce; refusing to fabricate a rollback"
            )));
        }

        Err(EvolutionError::Validation(format!(
            "configuration change {change_id} was never applied to a live target; \
             it is a recorded intent only and there is nothing to roll back"
        )))
    }

    /// Get change history.
    pub fn get_change_history(&self) -> &[ConfigChange] {
        &self.change_history
    }

    /// Apply a configuration change.
    ///
    /// This engine only *records* configuration intent: no provider, runtime,
    /// context, agent, tool, or MCP setting is mutated anywhere in this
    /// crate, so there is no real target `applied = true` could truthfully
    /// refer to. Rather than marking the record applied without a mutation,
    /// this rejects every known change with an explicit explanation and
    /// rejects unknown ids. The `configure_*`/`set_*`/`enable_*`/`disable_*`
    /// methods remain the way to record intended changes.
    pub fn apply_change(&mut self, change_id: &str) -> Result<()> {
        let change = self
            .change_history
            .iter()
            .find(|c| c.id == change_id)
            .ok_or_else(|| {
                EvolutionError::NotFound(format!("configuration change {change_id} not found"))
            })?;

        Err(EvolutionError::Validation(format!(
            "configuration change {change_id} targets {:?} but this engine has no \
             live configuration surface to apply it to; it remains recorded \
             intent only (applied=false)",
            change.target
        )))
    }

    fn record_change(&mut self, change: ConfigChange) {
        self.change_history.push(change);
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
        assert_eq!(engine.get_change_history().len(), 1);
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
        assert!(!engine.get_change_history()[0].applied);
    }

    #[test]
    fn rollback_reports_nothing_to_restore() {
        let mut engine = engine();
        let change = engine.disable_tool("write_file", "unused").unwrap();

        let err = engine.rollback_change(&change.id).unwrap_err();
        assert!(err.to_string().contains("nothing to roll back"));

        // The recorded intent is untouched: no fabricated value swap.
        let stored = &engine.get_change_history()[0];
        assert_eq!(stored.new_value, "disabled");
        assert_eq!(stored.rollback_value.as_deref(), Some("enabled"));
    }
}
