use std::path::{Path, PathBuf};

/// A declared dependency: another plugin by name, and the version requirement it must satisfy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dependency {
    pub name: String,
    pub version_req: String,
}

/// The plugin manifest (`plugin.json`).
///
/// Every field defaults so that the original `{name, version, entry, enabled}` manifests keep
/// parsing unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginManifest {
    /// Stable identifier; defaults to `name` when omitted.
    pub id: String,
    pub name: String,
    /// Semver string (`MAJOR.MINOR.PATCH`).
    pub version: String,
    pub description: String,
    pub author: String,
    pub license: String,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub min_vikram_version: Option<String>,
    pub api_version: Option<String>,
    pub components: Option<PluginComponents>,
    pub dependencies: Vec<Dependency>,
    pub permissions: Vec<String>,
    pub entry: String,
    /// Absent manifests are treated as enabled, matching discovery behaviour.
    pub enabled: bool,
}

impl Default for PluginManifest {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            version: String::new(),
            description: String::new(),
            author: String::new(),
            license: String::new(),
            homepage: None,
            repository: None,
            min_vikram_version: None,
            api_version: None,
            components: None,
            dependencies: Vec::new(),
            permissions: Vec::new(),
            entry: String::new(),
            enabled: true,
        }
    }
}

impl PluginManifest {
    /// The identifier used on disk and in the lockfile: `id` when set, otherwise `name`.
    pub fn id(&self) -> &str {
        if self.id.trim().is_empty() {
            &self.name
        } else {
            &self.id
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        serde_json::from_str(raw).map_err(|e| e.to_string())
    }

    /// Read `<dir>/plugin.json`.
    pub fn from_dir(dir: &Path) -> Result<Self, String> {
        let path = dir.join("plugin.json");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&raw)
    }

    /// Declared entry point, treating the empty string as "not declared".
    pub fn entry_point(&self) -> Option<&str> {
        let entry = self.entry.trim();
        if entry.is_empty() { None } else { Some(entry) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginState {
    pub name: String,
    pub version: Option<String>,
    pub path: PathBuf,
    pub loaded: bool,
    pub error: Option<String>,
    pub entry: Option<String>,
}

impl PluginState {
    /// The plugin id: the manifest id when it differs from the name, else the name.
    pub fn id(&self) -> String {
        match PluginManifest::from_dir(&self.path) {
            Ok(manifest) if !manifest.id.trim().is_empty() => manifest.id().to_owned(),
            _ => self.name.clone(),
        }
    }
}

use crate::plugins::*;
