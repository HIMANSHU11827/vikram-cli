//! Plugin discovery, manifests and lifecycle.
//!
//! Discovery: a plugin is loaded when its manifest parses, it is enabled, and a declared entry
//! point exists. Nothing here executes plugin code on its own, and installed != loaded on purpose.
//!
//! Lifecycle: a plugin is a directory under the plugins dir holding a `plugin.json` manifest. The
//! manifest understands both the rich schema ([`PluginManifest`]) and the original three-field form
//! (`name`/`version`/`entry` plus `enabled`) — every field defaults, so the legacy shape keeps
//! parsing. Lifecycle operations are local filesystem work only: there is no remote marketplace and
//! no cryptographic signature verification.
//!
//! Dependencies: [`PluginRegistry::install_with_dependencies`] resolves a manifest's declared
//! `dependencies` against local sources only — a sibling directory of the plugin being installed and
//! the local source registry ([`registry_dir`], a plain directory of plugin folders indexed by
//! [`PluginRegistry::registry_index`]). Each missing dependency is installed depth-first *before*
//! the plugin that needs it, and a dependency that cannot be found is reported, never fetched: no
//! network, no package ecosystem. [`PluginRegistry::validate`] and
//! [`PluginRegistry::resolve_dependencies`] still report problems rather than fixing them.
//!
//! Integrity: a plugin directory may carry `plugin.sha256`, a file holding the hex SHA-256 of its
//! `plugin.json` (computed by [`sha256_hex`], a self-contained implementation — this crate links no
//! hash or crypto library). When the file is present, install and validate verify it and refuse a
//! mismatch. `plugin.sig` is only ever reported as present: nothing here checks a signature
//! cryptographically, and [`PluginRegistry::inspect`] says so in as many words.
//!
//! Permissions: a manifest's `permissions` list is parsed into the closed [`PluginPermission`]
//! vocabulary, and [`PluginRegistry::execute`] refuses to spawn an entry point unless the manifest
//! declares `process` or `network`. Unknown permissions and unknown component types are warnings
//! from [`PluginRegistry::validate`], never install-blockers.

use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::vikram_home;

/// The version of this VIKRAM build. Used to check `min_vikram_version` requirements.
pub fn current_vikram_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The file holding the hex SHA-256 of `plugin.json`, when a plugin ships one.
pub const PLUGIN_HASH_FILE: &str = "plugin.sha256";

/// The optional detached-signature file. Recorded, never cryptographically checked.
pub const PLUGIN_SIG_FILE: &str = "plugin.sig";

/// The local plugin source registry: `$VIKRAM_HOME/plugin-registry`.
///
/// It is an ordinary directory of plugin folders — one subdirectory per plugin, each holding a
/// `plugin.json`. It is what [`PluginRegistry::install_with_dependencies`] searches for declared
/// dependencies, and what [`PluginRegistry::registry_index`] lists. Nothing here fetches it from
/// anywhere: populating the registry is the operator's job.
pub fn registry_dir() -> PathBuf {
    vikram_home().join("plugin-registry")
}

/// Optional component declarations, e.g. `{"tools": ["a"], "skills": ["b"], "hooks": []}`.
pub type PluginComponents = BTreeMap<String, Vec<String>>;

/// The component types a manifest may declare under `components`.
///
/// A key outside this list is reported by [`PluginRegistry::validate`] as a warning; it never
/// blocks installation.
pub const PLUGIN_COMPONENT_TYPES: [&str; 10] = [
    "tools",
    "skills",
    "agents",
    "hooks",
    "commands",
    "mcp",
    "providers",
    "channels",
    "services",
    "ui",
];

pub use crate::plugins::exec::*;
pub use crate::plugins::integrity::*;
pub use crate::plugins::manifest::*;
pub use crate::plugins::permission::*;
pub use crate::plugins::registry::*;
pub(crate) use crate::plugins::util::*;
pub use crate::plugins::version::*;

pub(crate) mod audit;
pub(crate) mod exec;
pub(crate) mod install;
pub(crate) mod integrity;
pub(crate) mod manifest;
pub(crate) mod permission;
pub(crate) mod registry;
pub(crate) mod util;
pub(crate) mod version;

#[cfg(test)]
mod tests;
