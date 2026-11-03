//! The VIKRAM host.
//!
//! The kernel decides *whether* an effect may run; this crate supplies the things the kernel is
//! deliberately ignorant of: where sessions live, which model answers, which tools exist, which
//! MCP servers and plugins are configured, and how observations reach a connected interface.
//!
//! Nothing here formats terminal output. The TUI is one subscriber among possible others.

pub mod agents;
pub mod browser;
pub mod bus;
pub mod computer;
pub mod config;
pub mod context;
pub mod error;
pub mod hive_chat;
pub mod learning;
pub mod lsp;
pub mod mcp;
pub mod memory;
pub mod openai;
pub mod plugins;
pub mod provider;
pub mod pty;
pub mod rules;
pub mod runtime;
pub mod sandbox;
pub mod search;
pub mod session;
pub mod skills;
pub mod teams;
pub mod tickets;
pub mod tools;
pub mod turn;
pub mod types;
pub mod verification;
pub mod workspace;

pub use agents::{AgentManager, ManagedAgent, TaskManager};
pub use bus::EventBus;
pub use config::{
    Config, McpConfig, McpServerConfig, SearchBackend, SearchConfig, database_path, vikram_home,
};
pub use context::{ContextBundle, ContextLoader};
pub use error::{Result, RuntimeError};
pub use hive_chat::HiveManager;
pub use learning::{LearningAuditEntry, LearningManager, LearningPolicy, LearningProposal};
pub use lsp::{LspClient, LspError, LspPool, ServerCommand};
pub use mcp::McpRegistry;
pub use memory::MemoryStore;
pub use plugins::PluginRegistry;
pub use provider::{CompletionRequest, Provider, ProviderEvent, ProviderStream, ToolSpec};
pub use rules::RuleBook;
pub use runtime::{AgentControl, Runtime, RuntimeHandle};
pub use session::SessionRuntime;
pub use skills::{Skill, SkillRegistry};
pub use teams::{Team, TeamManager};
pub use tickets::TicketManager;
pub use tools::{Tool, ToolContext, ToolOutput, ToolRegistry};
pub use types::*;
pub use workspace::WorkspaceManager;
