//! Self-Configuration Engine: Independent management of configurable parts.
//!
//! This engine allows Vikram to independently manage models, providers, runtime
//! settings, context, interface, agents, tools, and MCP integrations.

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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigChange {
    pub id: String,
    pub target: ConfigTarget,
    pub previous_value: Option<String>,
    pub new_value: String,
    pub reason: String,
    pub created_ms: u64,
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
                    "Pending change not applied: {} = {}",
                    format!("{:?}", change.target),
                    change.new_value
                ));
            }
        }

        Ok(issues)
    }

    /// Rollback a configuration change.
    pub fn rollback_change(&mut self, change_id: &str) -> Result<()> {
        if let Some(change) = self.change_history.iter_mut().find(|c| c.id == change_id) {
            if let Some(rollback_value) = &change.rollback_value {
                let previous = change.previous_value.clone();
                change.previous_value = Some(change.new_value.clone());
                change.new_value = rollback_value.clone();
                change.rollback_value = previous;
                change.applied = false;
            }
        }
        Ok(())
    }

    /// Get change history.
    pub fn get_change_history(&self) -> &[ConfigChange] {
        &self.change_history
    }

    /// Apply a configuration change.
    pub fn apply_change(&mut self, change_id: &str) -> Result<()> {
        if let Some(change) = self.change_history.iter_mut().find(|c| c.id == change_id) {
            change.applied = true;
            let _ = self.store.save_event(&crate::types::LearningEvent {
                id: gen_id("evt"),
                timestamp_ms: now_ms(),
                agent_id: "system".to_string(),
                kind: "config_applied".to_string(),
                summary: format!("Configuration applied: {:?}", change.target),
                payload: serde_json::json!({"change_id": change.id}),
            });
        }
        Ok(())
    }

    fn record_change(&mut self, change: ConfigChange) {
        self.change_history.push(change);
    }
}
