use crate::hooks::HookRule;
use crate::pipeline::StagePoint;
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
    /// Hook ordering: higher runs first, ties broken by plugin id.
    pub priority: i32,
    /// Declarative hook rules, evaluated in-process for every loaded plugin.
    pub hook_rules: Vec<HookRule>,
    /// File operations the plugin asks the operator to apply under a managed root.
    ///
    /// Applied explicitly by `vikram transforms apply`, tracked in `~/.vikram/transforms.json`
    /// and reversible with `vikram transforms rollback`. An absent or empty list means the plugin
    /// declares no transforms.
    pub transforms: Vec<TransformOp>,
    /// Tools the plugin offers to the model, each callable as `plugin.<name>`.
    ///
    /// A plugin that declares none keeps the single generic `plugin.execute` door it has always
    /// had, so old manifests change behaviour in no way.
    pub tools: Vec<PluginTool>,
}

/// One tool a plugin offers, described well enough for the model to call it.
///
/// Parameters are typed as strings on purpose: a manifest is operator-authored JSON, and a
/// free-form schema there is one typo away from a tool the model can never fill in correctly.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginTool {
    /// The name after the `plugin.` prefix.
    pub name: String,
    pub description: String,
    /// Parameter name → what it is for.
    pub params: std::collections::BTreeMap<String, String>,
    /// Parameters the model must supply.
    pub required: Vec<String>,
}

/// One file a plugin asks to place under a managed transform root.
///
/// `path` is relative to the root (`skills/` is the only root v1 allows) and must stay inside it.
/// Content is either inline UTF-8 `content` or base64 `content_b64`; exactly one must be set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransformOp {
    /// `add_file` refuses a target that already exists unowned; `write_file` creates or replaces.
    pub op: String,
    pub path: String,
    pub content: Option<String>,
    pub content_b64: Option<String>,
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
            priority: 0,
            hook_rules: Vec::new(),
            transforms: Vec::new(),
            tools: Vec::new(),
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

    /// The stage points this plugin asks to handle, from `components.stages`.
    ///
    /// Names parse with [`StagePoint::parse`]; spellings that do not parse come back in the
    /// second list so the loader reports them rather than dropping them silently.
    pub fn declared_stages(&self) -> (Vec<StagePoint>, Vec<String>) {
        let mut points = Vec::new();
        let mut unknown = Vec::new();
        let declared: &[String] = self
            .components
            .as_ref()
            .and_then(|components| components.get("stages"))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for raw in declared {
            match StagePoint::parse(raw) {
                Some(point) if !points.contains(&point) => points.push(point),
                Some(_) => {}
                None => unknown.push(raw.clone()),
            }
        }
        (points, unknown)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginState {
    pub name: String,
    pub version: Option<String>,
    pub path: PathBuf,
    pub loaded: bool,
    /// True only for plugins under the operator plugins dir. Workspace-discovered plugins are
    /// listed but never executed or hooked: a cloned repo must not gain code execution for free.
    pub installed: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_manifest_still_parses_without_tools() {
        let raw = r#"{"name":"demo","version":"1.0.0","entry":"main.wasm"}"#;
        let manifest = PluginManifest::parse(raw).expect("a minimal manifest parses");
        assert!(manifest.tools.is_empty());
    }

    #[test]
    fn a_declared_tool_keeps_its_parameters() {
        let raw = r#"{
            "name": "demo",
            "version": "1.0.0",
            "entry": "main.wasm",
            "tools": [{
                "name": "send",
                "description": "Post a message.",
                "params": {"channel": "Where it goes."},
                "required": ["channel"]
            }]
        }"#;
        let manifest = PluginManifest::parse(raw).expect("a manifest with tools parses");
        assert_eq!(manifest.tools.len(), 1);
        assert_eq!(manifest.tools[0].name, "send");
        assert_eq!(
            manifest.tools[0].params.get("channel").map(String::as_str),
            Some("Where it goes.")
        );
        assert_eq!(manifest.tools[0].required, vec!["channel".to_owned()]);
    }
}
