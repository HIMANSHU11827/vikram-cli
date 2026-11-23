//! Tool executor.
//!
//! A tool never decides whether it may run. It only declares the permission shape of a call
//! ([`Tool::permission`]); the host evaluates that against policy and only then invokes
//! [`Tool::run`]. Tool output is external content by definition, so it re-enters the conversation
//! as [`vikram_contracts::Trust::External`].
//!
//! One file per family of tools; [`core`] holds the shared contract (trait, context, registry) and
//! the helpers every family reads through. Everything is re-exported here so callers keep using
//! `crate::tools::X` and `vikram_runtime::tools::X`.

pub use crate::tools::core::*;

pub use crate::tools::browser::*;
pub(crate) use crate::tools::codeindex::*;
pub(crate) use crate::tools::cross_check::*;
pub(crate) use crate::tools::db::*;
pub(crate) use crate::tools::deploy::*;
pub(crate) use crate::tools::doc::*;
pub(crate) use crate::tools::fs::*;
pub(crate) use crate::tools::git::*;
pub(crate) use crate::tools::hive_roster::*;
pub use crate::tools::interact::*;
pub(crate) use crate::tools::lsp::*;
pub use crate::tools::mcp::*;
pub(crate) use crate::tools::memory::*;
pub(crate) use crate::tools::message::*;
pub(crate) use crate::tools::plugins::*;
pub(crate) use crate::tools::process::*;
pub(crate) use crate::tools::relations::*;
pub(crate) use crate::tools::research::*;
pub(crate) use crate::tools::search::*;
pub(crate) use crate::tools::session_search::*;
pub(crate) use crate::tools::shell::*;
pub use crate::tools::skills::*;
pub(crate) use crate::tools::system::*;
pub(crate) use crate::tools::terminal::*;
pub(crate) use crate::tools::threat::*;
pub(crate) use crate::tools::verify::*;
pub(crate) use crate::tools::web::*;
pub(crate) use crate::tools::worker_note::*;

pub(crate) mod browser;
pub(crate) mod codeindex;
pub(crate) mod core;
pub(crate) mod cross_check;
pub(crate) mod db;
pub(crate) mod deploy;
pub(crate) mod doc;
pub(crate) mod fs;
pub(crate) mod git;
pub(crate) mod hive_roster;
pub(crate) mod interact;
pub(crate) mod lsp;
pub(crate) mod mcp;
pub(crate) mod memory;
pub(crate) mod message;
pub(crate) mod plugins;
pub(crate) mod process;
pub(crate) mod relations;
pub(crate) mod research;
pub(crate) mod search;
pub(crate) mod session_search;
pub(crate) mod shell;
pub(crate) mod skills;
pub(crate) mod system;
pub(crate) mod terminal;
pub(crate) mod threat;
pub(crate) mod verify;
pub(crate) mod web;
pub(crate) mod worker_note;

#[cfg(test)]
mod tests;

// The live plan is session state an interface draws beside the transcript, not a message, so the
// accessor is exported from the crate rather than living only inside the tool that writes it.
pub use crate::tools::process::{Plan, PlanStatus, PlanStep, all_plans, plan_for};
