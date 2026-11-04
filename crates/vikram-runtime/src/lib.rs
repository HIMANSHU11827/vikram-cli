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
pub mod compaction;
pub mod computer;
pub mod config;
pub mod context;
pub mod context_engine;
pub mod error;
pub mod hive_chat;
pub mod learning;
pub mod lsp;
pub mod mcp;
pub mod memory;
pub mod memory_files;
pub mod memory_provider;
pub mod openai;
pub mod plugins;
pub mod provider;
pub mod pty;
pub mod rules;
pub mod runtime;
pub use vikram_sandbox as sandbox;
pub mod search;
pub mod session;
pub mod skills;
pub mod spill;
pub mod teams;
pub mod tickets;
pub mod titling;
pub mod tools;
pub mod turn;
pub mod types;
pub mod verification;
pub mod workspace;

pub use agents::{AgentManager, ManagedAgent, TaskManager};
pub use bus::EventBus;
pub use compaction::{
    COMPACTION_FOLDED_KEY, COMPACTION_SUMMARY_KEY, COMPACTION_THROUGH_KEY, CompactionPolicy,
    CompactionReport, compact, delegation_answer, delegation_brief, digest_tool_output,
    is_compaction_summary, project_from_journal, should_compact,
};
pub use config::{
    Config, McpConfig, McpServerConfig, SearchBackend, SearchConfig, database_path, vikram_home,
};
pub use context::{ContextBundle, ContextLoader};
pub use context_engine::*;
pub use error::{Result, RuntimeError};
pub use hive_chat::HiveManager;
pub use learning::{LearningAuditEntry, LearningManager, LearningPolicy, LearningProposal};
pub use lsp::{LspClient, LspError, LspPool, ServerCommand};
pub use mcp::McpRegistry;
pub use memory::MemoryStore;
pub use memory_provider::{BuiltInMemoryProvider, MemoryProvider, MemoryProviderManager};
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
