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
pub(crate) use crate::tools::fs::*;
pub use crate::tools::interact::*;
pub(crate) use crate::tools::lsp::*;
pub use crate::tools::mcp::*;
pub(crate) use crate::tools::memory::*;
pub(crate) use crate::tools::plugins::*;
pub(crate) use crate::tools::process::*;
pub(crate) use crate::tools::search::*;
pub(crate) use crate::tools::shell::*;
pub use crate::tools::skills::*;
pub(crate) use crate::tools::teams::*;
pub(crate) use crate::tools::verify::*;
pub(crate) use crate::tools::web::*;

pub(crate) mod browser;
pub(crate) mod core;
pub(crate) mod fs;
pub(crate) mod interact;
pub(crate) mod lsp;
pub(crate) mod mcp;
pub(crate) mod memory;
pub(crate) mod plugins;
pub(crate) mod process;
pub(crate) mod search;
pub(crate) mod shell;
pub(crate) mod skills;
pub(crate) mod teams;
pub(crate) mod verify;
pub(crate) mod web;

#[cfg(test)]
mod tests;
