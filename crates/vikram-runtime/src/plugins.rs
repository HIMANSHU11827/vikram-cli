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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::{plugins_dir, vikram_home};

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

/// A capability a plugin declares in its manifest's `permissions` list.
///
/// The vocabulary is deliberately closed. [`parse_permission`] is the only way in, so a plugin
/// cannot invent a capability by typo, and [`PluginRegistry::validate`] reports spellings outside
/// this set instead of quietly trusting them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginPermission {
    FilesystemRead,
    FilesystemWrite,
    Network,
    Process,
    Environment,
    Secrets,
    Skills,
    Tools,
    Agents,
    Hive,
    Bots,
    Ui,
    Background,
}

impl PluginPermission {
    /// Every permission, in the canonical order.
    pub const ALL: [PluginPermission; 13] = [
        PluginPermission::FilesystemRead,
        PluginPermission::FilesystemWrite,
        PluginPermission::Network,
        PluginPermission::Process,
        PluginPermission::Environment,
        PluginPermission::Secrets,
        PluginPermission::Skills,
        PluginPermission::Tools,
        PluginPermission::Agents,
        PluginPermission::Hive,
        PluginPermission::Bots,
        PluginPermission::Ui,
        PluginPermission::Background,
    ];

    /// The canonical spelling, which [`parse_permission`] accepts back.
    pub fn as_str(self) -> &'static str {
        match self {
            PluginPermission::FilesystemRead => "fs.read",
            PluginPermission::FilesystemWrite => "fs.write",
            PluginPermission::Network => "network",
            PluginPermission::Process => "process",
            PluginPermission::Environment => "env",
            PluginPermission::Secrets => "secrets",
            PluginPermission::Skills => "skills",
            PluginPermission::Tools => "tools",
            PluginPermission::Agents => "agents",
            PluginPermission::Hive => "hive",
            PluginPermission::Bots => "bots",
            PluginPermission::Ui => "ui",
            PluginPermission::Background => "background",
        }
    }

    /// Does this permission allow a plugin's entry point to run at all?
    ///
    /// Only `process` and `network` do: a plugin that merely reads files or exposes components is
    /// not allowed to spawn its entry script.
    pub fn allows_execution(self) -> bool {
        matches!(self, PluginPermission::Process | PluginPermission::Network)
    }
}

/// Parse a declared permission string into the closed vocabulary.
///
/// Accepted spellings are case-insensitive, tolerate `_`/`:` in place of `.`, and ignore a
/// trailing `:scope` suffix (`fs.read:workspace` is `fs.read`). An unrecognised spelling is
/// `None` — never a grant.
pub fn parse_permission(raw: &str) -> Option<PluginPermission> {
    canonical_permission(raw).or_else(|| {
        let bare = raw.split(':').next().unwrap_or_default();
        canonical_permission(bare)
    })
}

fn canonical_permission(spelling: &str) -> Option<PluginPermission> {
    let normalized = spelling
        .trim()
        .to_ascii_lowercase()
        .replace(['_', ':'], ".");
    match normalized.as_str() {
        "fs.read" | "filesystem.read" => Some(PluginPermission::FilesystemRead),
        "fs.write" | "filesystem.write" => Some(PluginPermission::FilesystemWrite),
        "network" => Some(PluginPermission::Network),
        "process" | "exec" => Some(PluginPermission::Process),
        "env" | "environment" => Some(PluginPermission::Environment),
        "secrets" => Some(PluginPermission::Secrets),
        "skills" => Some(PluginPermission::Skills),
        "tools" => Some(PluginPermission::Tools),
        "agents" => Some(PluginPermission::Agents),
        "hive" => Some(PluginPermission::Hive),
        "bots" => Some(PluginPermission::Bots),
        "ui" | "tui" => Some(PluginPermission::Ui),
        "background" => Some(PluginPermission::Background),
        _ => None,
    }
}

/// Parse a manifest's declared permissions, dropping duplicates and unknown spellings.
///
/// Unknown spellings are surfaced by [`PluginRegistry::validate`], not here: this function only
/// answers "what was actually granted".
pub fn parse_permissions(declared: &[String]) -> Vec<PluginPermission> {
    let mut granted: Vec<PluginPermission> = Vec::new();
    for raw in declared {
        if let Some(permission) = parse_permission(raw) {
            if !granted.contains(&permission) {
                granted.push(permission);
            }
        }
    }
    granted
}

/// Refuse execution unless the manifest declares `process` or `network`.
///
/// Silence is not consent: a manifest with no `permissions` key at all is refused, because the
/// legacy three-field shape never asked for the right to run.
fn ensure_execution_permitted(plugin_name: &str, manifest: &PluginManifest) -> Result<(), String> {
    if manifest.permissions.is_empty() {
        return Err(format!(
            "plugin `{plugin_name}` declares no permissions; execution refused \
             (add `process` to plugin.json permissions to allow it)"
        ));
    }
    if !parse_permissions(&manifest.permissions)
        .iter()
        .any(|permission| permission.allows_execution())
    {
        return Err(format!(
            "plugin `{plugin_name}` declares no execution permission (add `process`)"
        ));
    }
    Ok(())
}

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

#[derive(Clone, Debug, Default)]
pub struct PluginRegistry {
    plugins: Vec<PluginState>,
    /// Directory the registry was scanned from; lifecycle operations target it. Falling back to
    /// [`plugins_dir`] keeps `PluginRegistry::default()` usable.
    dir: Option<PathBuf>,
    /// Source registry searched for dependencies. `None` means [`registry_dir`]; tests and callers
    /// that keep their own marketplace point it somewhere else with [`Self::with_registry_dir`].
    registry: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct LockEntry {
    id: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct LockBody {
    plugins: Vec<LockEntry>,
}

impl PluginRegistry {
    pub fn scan(dir: &Path) -> Self {
        let mut plugins = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Self {
                plugins,
                dir: Some(dir.to_path_buf()),
                registry: None,
            };
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("plugin.json");
            if !manifest_path.exists() {
                continue;
            }
            let raw = match std::fs::read_to_string(&manifest_path) {
                Ok(raw) => raw,
                Err(err) => {
                    plugins.push(PluginState {
                        name: dir_name(&path),
                        version: None,
                        path,
                        loaded: false,
                        error: Some(err.to_string()),
                        entry: None,
                    });
                    continue;
                }
            };
            match PluginManifest::parse(&raw) {
                Ok(manifest) => {
                    // A manifest without an entry point keeps the legacy "nothing to check" rule.
                    let entry_exists = manifest
                        .entry_point()
                        .map(|entry| path.join(entry).exists())
                        .unwrap_or(true);
                    let loaded = manifest.enabled && entry_exists;
                    let entry = manifest.entry_point().map(str::to_owned);
                    let name = if manifest.name.trim().is_empty() {
                        dir_name(&path)
                    } else {
                        manifest.name.clone()
                    };
                    let version = if manifest.version.trim().is_empty() {
                        None
                    } else {
                        Some(manifest.version.clone())
                    };
                    plugins.push(PluginState {
                        name,
                        version,
                        path,
                        loaded,
                        error: if entry_exists {
                            None
                        } else {
                            Some("declared entry point is missing".into())
                        },
                        entry,
                    });
                }
                Err(err) => plugins.push(PluginState {
                    name: dir_name(&path),
                    version: None,
                    path,
                    loaded: false,
                    error: Some(format!("manifest: {err}")),
                    entry: None,
                }),
            }
        }
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            plugins,
            dir: Some(dir.to_path_buf()),
            registry: None,
        }
    }

    pub fn from_default_dir() -> Self {
        Self::scan(&plugins_dir())
    }

    pub fn installed(&self) -> usize {
        self.plugins.len()
    }

    pub fn loaded(&self) -> usize {
        self.plugins.iter().filter(|p| p.loaded).count()
    }

    pub fn plugins(&self) -> &[PluginState] {
        &self.plugins
    }

    /// The directory lifecycle operations write to.
    pub fn root(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(plugins_dir)
    }

    /// Point dependency resolution at a different local source registry.
    ///
    /// The default is [`registry_dir`]; an explicit directory keeps a private marketplace (or a
    /// test fixture) from reaching into `$VIKRAM_HOME` at all.
    pub fn with_registry_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.registry = Some(dir.into());
        self
    }

    /// The source registry this instance reads: [`Self::with_registry_dir`], else [`registry_dir`].
    pub fn registry_root(&self) -> PathBuf {
        self.registry.clone().unwrap_or_else(registry_dir)
    }

    /// Scan the source registry for plugin folders and return `(id, name, version)` per plugin.
    ///
    /// This is a local marketplace index and nothing more: folder names are not rewritten, no
    /// network is consulted, and a subdirectory whose `plugin.json` is missing or unparseable is
    /// skipped rather than reported. A registry directory that does not exist is an empty index.
    pub fn registry_index(&self) -> Result<Vec<(String, String, String)>, String> {
        let dir = self.registry_root();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(format!("cannot read {}: {err}", dir.display())),
        };
        let mut index: Vec<(String, String, String)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || !path.join("plugin.json").is_file() {
                continue;
            }
            let Ok(manifest) = PluginManifest::from_dir(&path) else {
                continue;
            };
            let id = manifest.id().trim().to_owned();
            if id.is_empty() {
                continue;
            }
            index.push((id, manifest.name, manifest.version));
        }
        index.sort();
        Ok(index)
    }

    /// Run a loaded plugin's entry point as a subprocess.
    ///
    /// The manifest's permissions gate this: execution needs `process` or `network`. A plugin
    /// that declares no permissions at all is refused, so the legacy manifest shape keeps
    /// installing and inspecting but cannot run until it asks for the privilege.
    pub fn execute<A>(&self, plugin_name: &str, args: A) -> Result<String, String>
    where
        A: PluginArgs,
    {
        let plugin = self
            .plugins
            .iter()
            .find(|p| p.name == plugin_name)
            .ok_or_else(|| format!("plugin not found: {plugin_name}"))?;
        if !plugin.loaded {
            return Err(format!("plugin not loaded: {plugin_name}"));
        }
        let manifest = PluginManifest::from_dir(&plugin.path)
            .map_err(|e| format!("cannot read the manifest of plugin `{plugin_name}`: {e}"))?;
        ensure_execution_permitted(plugin_name, &manifest)?;
        let entry = plugin
            .entry
            .clone()
            .ok_or_else(|| format!("plugin has no entry point: {plugin_name}"))?;
        let entry_path = plugin.path.join(&entry);
        if !entry_path.exists() {
            return Err(format!(
                "plugin entry point is missing: {}",
                entry_path.display()
            ));
        }

        let args = args.to_args();

        let mut cmd = if entry.ends_with(".py") {
            let mut c = std::process::Command::new("python3");
            c.arg(&entry_path);
            c
        } else if entry.ends_with(".sh") {
            let mut c = std::process::Command::new("sh");
            c.arg(&entry_path);
            c
        } else if entry.ends_with(".js") {
            let mut c = std::process::Command::new("node");
            c.arg(&entry_path);
            c
        } else {
            if !is_executable(&entry_path) {
                return Err(format!(
                    "plugin entry is not executable: {}",
                    entry_path.display()
                ));
            }
            std::process::Command::new(&entry_path)
        };
        cmd.args(&args);
        cmd.current_dir(&plugin.path);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err("plugin timed out after 30 seconds".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        let output = child.wait_with_output().map_err(|e| e.to_string())?;
        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(format!("exit: {code}\nstdout: {stdout}\nstderr: {stderr}"))
    }

    pub fn merge(&mut self, other: Self) {
        for plugin in other.plugins {
            if !self.plugins.iter().any(|p| p.name == plugin.name) {
                self.plugins.push(plugin);
            }
        }
        self.plugins.sort_by(|a, b| a.name.cmp(&b.name));
    }

    pub fn for_workspace(workspace: &Path) -> Self {
        let mut reg = Self::from_default_dir();
        for sub in [".vikram/plugins", "plugins"] {
            let dir = workspace.join(sub);
            if dir.is_dir() {
                reg.merge(Self::scan(&dir));
            }
        }
        reg
    }

    // ---- lifecycle -------------------------------------------------------

    /// Install the plugin directory `source` as `<root>/<id>`, returning the installed id.
    ///
    /// The manifest must exist and parse, the id must be usable as a directory name, a shipped
    /// `plugin.sha256` must match, and an existing plugin of the same id is never overwritten.
    ///
    /// When the manifest declares `dependencies`, this delegates to
    /// [`Self::install_with_dependencies`] so the dependencies land first and the plugin itself
    /// last; the returned id is still the plugin's own. A manifest without dependencies keeps the
    /// plain single-directory copy.
    pub fn install(&self, source: &Path) -> Result<String, String> {
        match PluginManifest::from_dir(source) {
            Ok(manifest) if !manifest.dependencies.is_empty() => {
                let order = self.install_with_dependencies(source)?;
                order
                    .last()
                    .cloned()
                    .ok_or_else(|| format!("nothing was installed from {}", source.display()))
            }
            _ => self.install_one(source),
        }
    }

    /// Install `source` and every dependency it declares, dependencies first.
    ///
    /// Returns the ids in the order they were installed, so the plugin itself is always last. Each
    /// dependency already installed at a satisfying version is left alone; each missing one is
    /// looked for, in order, at `<parent-of-source>/<dep-name>` and
    /// `<registry-root>/<dep-name>`, and installed recursively. Cycles are refused. A dependency
    /// that cannot be found is an error naming it and the directories searched — nothing is ever
    /// downloaded.
    ///
    /// There is no rollback: a dependency installed by an earlier branch of the walk stays
    /// installed if a later one fails, because each plugin is an independent directory that the
    /// caller can [`Self::uninstall`].
    pub fn install_with_dependencies(&self, source: &Path) -> Result<Vec<String>, String> {
        let mut chain: Vec<String> = Vec::new();
        self.install_tree(source, &mut chain)
    }

    fn install_tree(&self, source: &Path, chain: &mut Vec<String>) -> Result<Vec<String>, String> {
        let manifest = self.candidate_manifest(source)?;
        let id = manifest.id().trim().to_owned();
        if let Some(position) = chain.iter().position(|seen| *seen == id) {
            let mut path: Vec<String> = chain[position..].to_vec();
            path.push(id);
            return Err(format!("dependency cycle detected: {}", quote_chain(&path)));
        }
        chain.push(id.clone());
        let resolved = self.install_dependencies_of(source, &manifest, &id, chain);
        chain.pop();
        let mut order = resolved?;
        // The plugin itself goes in only once everything it needs is on disk.
        order.push(self.install_one(source)?);
        Ok(order)
    }

    /// Install the dependencies one manifest declares, returning their ids in install order.
    fn install_dependencies_of(
        &self,
        source: &Path,
        manifest: &PluginManifest,
        id: &str,
        chain: &mut Vec<String>,
    ) -> Result<Vec<String>, String> {
        let mut order: Vec<String> = Vec::new();
        for dependency in &manifest.dependencies {
            let name = dependency.name.trim();
            if name.is_empty() {
                return Err(format!(
                    "plugin `{id}` declares a dependency with no `name` in its manifest"
                ));
            }
            if let Some(found) = self.dependency_version(name) {
                if !requirement_satisfied(&found, &dependency.version_req) {
                    return Err(format!(
                        "dependency `{name}` is installed at `{found}` but `{id}` requires `{}`; \
                         uninstall it first to pick up a matching version",
                        requirement_label(&dependency.version_req)
                    ));
                }
                continue;
            }
            let searched = self.dependency_candidates(source, name);
            let Some(found_at) = searched
                .iter()
                .find(|candidate| candidate.join("plugin.json").is_file())
            else {
                let places = searched
                    .iter()
                    .map(|candidate| candidate.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "missing dependency `{name}` (requires `{}`) for plugin `{id}`; searched: {places}",
                    requirement_label(&dependency.version_req)
                ));
            };
            if chain.iter().any(|seen| seen == name) {
                let mut path = chain.clone();
                path.push(name.to_string());
                return Err(format!("dependency cycle detected: {}", quote_chain(&path)));
            }
            let mut nested = self.install_tree(found_at, chain)?;
            order.append(&mut nested);
        }
        Ok(order)
    }

    /// Where a dependency named `name` may live, most specific first.
    fn dependency_candidates(&self, source: &Path, name: &str) -> Vec<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(parent) = source.parent() {
            candidates.push(parent.join(name));
        }
        let registry = self.registry_root().join(name);
        if !candidates.contains(&registry) {
            candidates.push(registry);
        }
        candidates
    }

    /// The version of an installed dependency, checking the target directory as well as the scan.
    ///
    /// The scan is a snapshot taken before this install run, so a dependency that a previous
    /// branch of the same run already installed is only visible on disk.
    fn dependency_version(&self, name: &str) -> Option<String> {
        let dir = self.root().join(name);
        if dir.join("plugin.json").is_file() {
            if let Ok(manifest) = PluginManifest::from_dir(&dir) {
                return Some(manifest.version);
            }
        }
        self.installed_version(name)
    }

    /// Read and check a candidate directory's manifest without copying anything.
    ///
    /// Covers the install-time invariants: a parsable `plugin.json`, a usable id, a present entry
    /// point and — when the plugin ships one — a matching `plugin.sha256`.
    fn candidate_manifest(&self, source: &Path) -> Result<PluginManifest, String> {
        let manifest_path = source.join("plugin.json");
        if !manifest_path.is_file() {
            return Err(format!(
                "not a plugin directory (no plugin.json): {}",
                source.display()
            ));
        }
        let manifest = PluginManifest::from_dir(source)
            .map_err(|e| format!("invalid manifest at {}: {e}", manifest_path.display()))?;
        let id = manifest.id().trim().to_owned();
        if id.is_empty() {
            return Err("manifest declares neither `id` nor `name`".to_string());
        }
        if id == "." || id == ".." || id.contains('/') || id.contains('\\') {
            return Err(format!("`{id}` is not a usable plugin id"));
        }
        if let Some(entry) = manifest.entry_point() {
            if !source.join(entry).exists() {
                return Err(format!(
                    "declared entry point `{entry}` is missing from {}",
                    source.display()
                ));
            }
        }
        verify_integrity(source, &id)?;
        Ok(manifest)
    }

    /// Copy one plugin directory into `<root>/<id>`; no dependency work.
    fn install_one(&self, source: &Path) -> Result<String, String> {
        let manifest = self.candidate_manifest(source)?;
        let id = manifest.id().trim().to_owned();
        let root = self.root();
        let target = root.join(&id);
        if target.exists() {
            return Err(format!(
                "plugin `{id}` is already installed at {}; uninstall it first",
                target.display()
            ));
        }
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("cannot create {}: {e}", root.display()))?;
        copy_dir(source, &target)?;
        Ok(id)
    }

    /// Remove an installed plugin's directory.
    pub fn uninstall(&self, id: &str) -> Result<(), String> {
        let dir = self
            .plugin_dir(id)
            .ok_or_else(|| format!("plugin not found: {id}"))?;
        std::fs::remove_dir_all(&dir).map_err(|e| format!("cannot remove {}: {e}", dir.display()))
    }

    pub fn enable(&self, id: &str) -> Result<(), String> {
        self.set_enabled(id, true)
    }

    pub fn disable(&self, id: &str) -> Result<(), String> {
        self.set_enabled(id, false)
    }

    /// Rewrite `plugin.json` with `enabled` flipped, atomically (temp file + rename).
    fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        let dir = self
            .plugin_dir(id)
            .ok_or_else(|| format!("plugin not found: {id}"))?;
        let manifest_path = dir.join("plugin.json");
        let raw = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
        let mut value: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| format!("invalid manifest at {}: {e}", manifest_path.display()))?;
        let object = value.as_object_mut().ok_or_else(|| {
            format!(
                "manifest at {} is not a JSON object",
                manifest_path.display()
            )
        })?;
        object.insert("enabled".to_string(), serde_json::Value::Bool(enabled));
        let body = format!(
            "{}\n",
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
        );
        write_atomic(&manifest_path, &body)
    }

    /// Check a candidate plugin directory without touching the installed set.
    ///
    /// `Err` means the manifest is missing or unparseable; `Ok` carries the list of problems found
    /// (each prefixed `error:` or `warning:`), which may be empty.
    ///
    /// A shipped `plugin.sha256` is verified here and a mismatch is an `error:`. The *absence* of a
    /// hash is not reported — a hand-authored plugin without one is not broken — and callers who
    /// want that advisory ask [`Self::validate_integrity`] for it.
    pub fn validate(&self, source: &Path) -> Result<Vec<String>, String> {
        let manifest_path = source.join("plugin.json");
        if !manifest_path.is_file() {
            return Err(format!(
                "not a plugin directory (no plugin.json): {}",
                source.display()
            ));
        }
        let manifest = PluginManifest::from_dir(source)
            .map_err(|e| format!("invalid manifest at {}: {e}", manifest_path.display()))?;
        let mut issues: Vec<String> = Vec::new();

        if manifest.name.trim().is_empty() && manifest.id.trim().is_empty() {
            issues.push("error: manifest declares no `name`".to_string());
        }
        if manifest.version.trim().is_empty() {
            issues.push("error: manifest declares no `version`".to_string());
        } else if !is_semver(&manifest.version) {
            issues.push(format!(
                "error: `version` `{}` is not semver (expected MAJOR.MINOR.PATCH)",
                manifest.version
            ));
        }
        match manifest.entry_point() {
            None => issues.push("error: manifest declares no `entry` point".to_string()),
            Some(entry) if !source.join(entry).exists() => issues.push(format!(
                "error: declared entry point `{entry}` does not exist"
            )),
            Some(_) => {}
        }
        // A shipped hash is checked; its absence is reported by [`Self::validate_integrity`] so
        // that a hand-authored plugin without a hash is not permanently noisy.
        match verify_integrity(source, manifest.id()) {
            Ok(_) => {}
            Err(err) => issues.push(format!("error: {err}")),
        }
        if let Some(min) = manifest
            .min_vikram_version
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            match numeric_parts(min) {
                None => issues.push(format!(
                    "error: `min_vikram_version` `{min}` is not a version"
                )),
                Some(minimum) => {
                    let current = numeric_parts(current_vikram_version()).unwrap_or((0, 0, 0));
                    if current < minimum {
                        issues.push(format!(
                            "error: requires vikram >= {min} but this build is {}",
                            current_vikram_version()
                        ));
                    }
                }
            }
        }
        if manifest.description.trim().is_empty() {
            issues.push("warning: manifest has no `description`".to_string());
        }
        for declared in &manifest.permissions {
            if parse_permission(declared).is_none() {
                issues.push(format!("warning: unknown permission `{declared}`"));
            }
        }
        if let Some(components) = &manifest.components {
            for kind in components.keys() {
                let normalized = kind.trim().to_ascii_lowercase();
                if !PLUGIN_COMPONENT_TYPES.contains(&normalized.as_str()) {
                    issues.push(format!("warning: unknown component type `{kind}`"));
                }
            }
        }
        for dep in &manifest.dependencies {
            match self.installed_version(&dep.name) {
                None => issues.push(format!(
                    "error: dependency `{}` is not installed (requires `{}`)",
                    dep.name,
                    requirement_label(&dep.version_req)
                )),
                Some(found) if !requirement_satisfied(&found, &dep.version_req) => {
                    issues.push(format!(
                        "error: dependency `{}` requires `{}` but `{found}` is installed",
                        dep.name,
                        requirement_label(&dep.version_req)
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(issues)
    }

    /// Check only the integrity side of a candidate directory: hash and signature.
    ///
    /// Three outcomes, all as `Ok` issue lists so a caller can render them:
    /// `plugin.sha256` present and matching yields nothing, present and mismatching yields the same
    /// `error: integrity check failed for ...` line [`Self::validate`] returns, and absent yields
    /// `warning: no plugin.sha256 present (integrity not verified)` — an advisory, never a block.
    ///
    /// This lives beside [`Self::validate`] rather than inside it because `plugin.validate` renders
    /// an empty issue list as "no problems found"; a warning every hashless plugin trips would
    /// silence that fast path permanently.
    pub fn validate_integrity(&self, source: &Path) -> Result<Vec<String>, String> {
        let manifest_path = source.join("plugin.json");
        if !manifest_path.is_file() {
            return Err(format!(
                "not a plugin directory (no plugin.json): {}",
                source.display()
            ));
        }
        let manifest = PluginManifest::from_dir(source)
            .map_err(|e| format!("invalid manifest at {}: {e}", manifest_path.display()))?;
        let id = manifest.id().trim().to_owned();
        if id.is_empty() {
            return Err("manifest declares neither `id` nor `name`".to_string());
        }
        let mut issues: Vec<String> = Vec::new();
        match verify_integrity(source, &id) {
            Ok(Some(_)) => {}
            Ok(None) => issues
                .push("warning: no plugin.sha256 present (integrity not verified)".to_string()),
            Err(err) => issues.push(format!("error: {err}")),
        }
        Ok(issues)
    }
    pub fn inspect(&self, id: &str) -> Result<String, String> {
        let dir = self
            .plugin_dir(id)
            .ok_or_else(|| format!("plugin not found: {id}"))?;
        let manifest = PluginManifest::from_dir(&dir).map_err(|e| {
            format!(
                "invalid manifest at {}: {e}",
                dir.join("plugin.json").display()
            )
        })?;
        let mut out = String::new();
        out.push_str(&format!("id: {}\n", manifest.id()));
        out.push_str(&format!("name: {}\n", or_none(&manifest.name)));
        out.push_str(&format!("version: {}\n", or_none(&manifest.version)));
        out.push_str(&format!(
            "description: {}\n",
            or_none(&manifest.description)
        ));
        out.push_str(&format!("author: {}\n", or_none(&manifest.author)));
        out.push_str(&format!("license: {}\n", or_none(&manifest.license)));
        out.push_str(&format!(
            "homepage: {}\n",
            or_none(manifest.homepage.as_deref().unwrap_or_default())
        ));
        out.push_str(&format!(
            "repository: {}\n",
            or_none(manifest.repository.as_deref().unwrap_or_default())
        ));
        out.push_str(&format!(
            "api_version: {}\n",
            or_none(manifest.api_version.as_deref().unwrap_or_default())
        ));
        out.push_str(&format!(
            "min_vikram_version: {}\n",
            or_none(manifest.min_vikram_version.as_deref().unwrap_or_default())
        ));
        out.push_str(&format!(
            "entry: {}\n",
            or_none(manifest.entry_point().unwrap_or_default())
        ));
        out.push_str(&format!("enabled: {}\n", manifest.enabled));
        out.push_str(&format!("integrity: {}\n", integrity_summary(&dir)));
        out.push_str(&format!(
            "signature: {}\n",
            if signature_present(&dir) {
                "present (not cryptographically verified)"
            } else {
                "(none)"
            }
        ));
        out.push_str(&format!("path: {}\n", dir.display()));
        match &manifest.components {
            Some(components) if !components.is_empty() => {
                let mut parts = Vec::new();
                for (kind, items) in components {
                    parts.push(format!("{kind}: {}", items.join(", ")));
                }
                out.push_str(&format!("components: {}\n", parts.join("; ")));
            }
            _ => out.push_str("components: (none)\n"),
        }
        if manifest.dependencies.is_empty() {
            out.push_str("dependencies: (none)\n");
        } else {
            let deps = manifest
                .dependencies
                .iter()
                .map(|d| format!("{} {}", d.name, requirement_label(&d.version_req)))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("dependencies: {deps}\n"));
        }
        if manifest.permissions.is_empty() {
            out.push_str("permissions: (none)");
        } else {
            out.push_str(&format!("permissions: {}", manifest.permissions.join(", ")));
        }
        Ok(out)
    }

    /// Every installed plugin with at least one dependency that is missing or unsatisfied.
    ///
    /// Plugins whose dependencies are all satisfied are omitted, so an empty result means the
    /// installed set is consistent.
    pub fn resolve_dependencies(&self) -> Vec<(String, Vec<String>)> {
        let mut installed: BTreeMap<String, String> = BTreeMap::new();
        let mut manifests: Vec<(String, PluginManifest)> = Vec::new();
        for plugin in &self.plugins {
            let version = plugin.version.clone().unwrap_or_default();
            installed.insert(plugin.name.clone(), version.clone());
            if let Ok(manifest) = PluginManifest::from_dir(&plugin.path) {
                installed.insert(manifest.id().to_string(), manifest.version.clone());
                manifests.push((manifest.id().to_string(), manifest));
            }
        }
        let mut problems: Vec<(String, Vec<String>)> = Vec::new();
        for (id, manifest) in &manifests {
            let mut missing: Vec<String> = Vec::new();
            for dep in &manifest.dependencies {
                match installed.get(&dep.name) {
                    None => missing.push(format!(
                        "missing dependency `{}` (requires `{}`)",
                        dep.name,
                        requirement_label(&dep.version_req)
                    )),
                    Some(found) if !requirement_satisfied(found, &dep.version_req) => {
                        missing.push(format!(
                            "dependency `{}` requires `{}` but `{found}` is installed",
                            dep.name,
                            requirement_label(&dep.version_req)
                        ));
                    }
                    Some(_) => {}
                }
            }
            if !missing.is_empty() {
                problems.push((id.clone(), missing));
            }
        }
        problems.sort_by(|a, b| a.0.cmp(&b.0));
        problems
    }

    /// Write `<root>/.lock`: the installed id/version list, sorted by id.
    pub fn lockfile(&self) -> Result<PathBuf, String> {
        let root = self.root();
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("cannot create {}: {e}", root.display()))?;
        let mut entries: Vec<LockEntry> = self
            .plugins
            .iter()
            .map(|plugin| match PluginManifest::from_dir(&plugin.path) {
                Ok(manifest) => LockEntry {
                    id: manifest.id().to_string(),
                    version: manifest.version,
                },
                Err(_) => LockEntry {
                    id: dir_name(&plugin.path),
                    version: plugin.version.clone().unwrap_or_default(),
                },
            })
            .collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries.dedup_by(|a, b| a.id == b.id);
        let body = format!(
            "{}\n",
            serde_json::to_string_pretty(&LockBody { plugins: entries })
                .map_err(|e| e.to_string())?
        );
        let path = root.join(".lock");
        write_atomic(&path, &body)?;
        Ok(path)
    }

    /// The permissions the plugin `id` declares, canonicalised and de-duplicated.
    ///
    /// Unknown spellings are dropped and an unknown plugin yields an empty list rather than an
    /// error; use [`Self::check_permissions`] when the plugin must exist and the grant must hold.
    pub fn permissions(&self, id: &str) -> Vec<PluginPermission> {
        self.plugin_dir(id)
            .and_then(|dir| PluginManifest::from_dir(&dir).ok())
            .map(|manifest| parse_permissions(&manifest.permissions))
            .unwrap_or_default()
    }

    /// Check that the plugin `id` declares every permission in `required`.
    ///
    /// `Err` names the missing permissions, or reports that the plugin is not installed. An empty
    /// `required` list only requires the plugin to exist.
    pub fn check_permissions(&self, id: &str, required: &[PluginPermission]) -> Result<(), String> {
        let dir = self
            .plugin_dir(id)
            .ok_or_else(|| format!("plugin not found: {id}"))?;
        let manifest = PluginManifest::from_dir(&dir)
            .map_err(|e| format!("cannot read the manifest of plugin `{id}`: {e}"))?;
        let granted = parse_permissions(&manifest.permissions);
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|permission| !granted.contains(permission))
            .map(PluginPermission::as_str)
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let plural = if missing.len() == 1 { "" } else { "s" };
        let list = missing
            .iter()
            .map(|permission| format!("`{permission}`"))
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!(
            "plugin `{id}` does not declare required permission{plural}: {list}"
        ))
    }

    /// The installed version of a plugin, by id or by name.
    pub fn installed_version(&self, id: &str) -> Option<String> {
        self.plugins
            .iter()
            .find(|p| p.name == id || dir_name(&p.path) == id || p.id() == id)
            .map(|p| p.version.clone().unwrap_or_default())
    }

    /// Resolve an id (or plugin name) to its directory on disk.
    fn plugin_dir(&self, id: &str) -> Option<PathBuf> {
        let direct = self.root().join(id);
        if direct.join("plugin.json").is_file() {
            return Some(direct);
        }
        self.plugins
            .iter()
            .find(|p| p.name == id || dir_name(&p.path) == id)
            .map(|p| p.path.clone())
            .or_else(|| {
                self.plugins
                    .iter()
                    .find(|p| p.id() == id)
                    .map(|p| p.path.clone())
            })
    }
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string())
}

fn or_none(value: &str) -> &str {
    if value.trim().is_empty() {
        "(none)"
    } else {
        value
    }
}

fn requirement_label(req: &str) -> String {
    let req = req.trim();
    if req.is_empty() {
        "*".to_string()
    } else {
        req.to_string()
    }
}

fn copy_dir(source: &Path, target: &Path) -> Result<(), String> {
    std::fs::create_dir_all(target)
        .map_err(|e| format!("cannot create {}: {e}", target.display()))?;
    let entries =
        std::fs::read_dir(source).map_err(|e| format!("cannot read {}: {e}", source.display()))?;
    for entry in entries.flatten() {
        let from = entry.path();
        let to = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| format!("cannot stat {}: {e}", from.display()))?;
        if file_type.is_dir() {
            copy_dir(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)
                .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))?;
        }
        // Symlinks and other special files are deliberately skipped: copying them blindly would
        // let a plugin manifest pull arbitrary paths into the plugins dir.
    }
    Ok(())
}

fn write_atomic(path: &Path, body: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin.json".to_string());
    let tmp = parent.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, body).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", path.display())
    })
}

// ---- integrity -----------------------------------------------------------

/// The SHA-256 round constants: the first 32 bits of the fractional parts of the cube roots of
/// the first 64 primes.
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The SHA-256 initial hash values: the first 32 bits of the fractional parts of the square roots
/// of the first 8 primes.
const SHA256_H: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// The SHA-256 digest of `data`, as 32 bytes, big-endian words.
///
/// Self-contained on purpose: verifying a plugin's `plugin.sha256` should not force a hash crate
/// into the dependency graph, and the algorithm is small enough to keep honest here.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state = SHA256_H;
    let bit_length = (data.len() as u64).wrapping_mul(8);

    // Padding: 0x80, then zeroes until the block is 56 bytes long, then the 64-bit bit length.
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for block in message.chunks_exact(64) {
        let mut schedule = [0u32; 64];
        for (slot, word) in schedule.iter_mut().zip(block.chunks_exact(4)) {
            *slot = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = schedule[i - 15].rotate_right(7)
                ^ schedule[i - 15].rotate_right(18)
                ^ (schedule[i - 15] >> 3);
            let s1 = schedule[i - 2].rotate_right(17)
                ^ schedule[i - 2].rotate_right(19)
                ^ (schedule[i - 2] >> 10);
            schedule[i] = schedule[i - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[i - 7])
                .wrapping_add(s1);
        }

        let mut working = state;
        for (constant, word) in SHA256_K.iter().zip(schedule.iter()) {
            let s1 = working[4].rotate_right(6)
                ^ working[4].rotate_right(11)
                ^ working[4].rotate_right(25);
            let choose = (working[4] & working[5]) ^ (!working[4] & working[6]);
            let temp1 = working[7]
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(*constant)
                .wrapping_add(*word);
            let s0 = working[0].rotate_right(2)
                ^ working[0].rotate_right(13)
                ^ working[0].rotate_right(22);
            let majority =
                (working[0] & working[1]) ^ (working[0] & working[2]) ^ (working[1] & working[2]);
            let temp2 = s0.wrapping_add(majority);

            working[7] = working[6];
            working[6] = working[5];
            working[5] = working[4];
            working[4] = working[3].wrapping_add(temp1);
            working[3] = working[2];
            working[2] = working[1];
            working[1] = working[0];
            working[0] = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip(working.iter()) {
            *slot = slot.wrapping_add(*value);
        }
    }

    let mut digest = [0u8; 32];
    for (word, bytes) in state.iter().zip(digest.chunks_exact_mut(4)) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// The lowercase hex SHA-256 of `data` — the form `plugin.sha256` holds.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in sha256(data) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The hex SHA-256 of a directory's `plugin.json`.
fn manifest_digest(dir: &Path) -> Result<String, String> {
    let path = dir.join("plugin.json");
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

/// The hash a directory's `plugin.sha256` asks for, `None` when the file is absent.
fn expected_manifest_digest(dir: &Path) -> Result<Option<String>, String> {
    let path = dir.join(PLUGIN_HASH_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    // Tolerate the `sha256sum` output form (`<hex>  plugin.json`) as well as a bare hex line.
    let field = raw.split_whitespace().next().unwrap_or_default();
    Ok(Some(field.trim().to_ascii_lowercase()))
}

/// Verify a plugin directory against its shipped `plugin.sha256`.
///
/// `Ok(None)` means the plugin ships no hash (nothing to check), `Ok(Some(hash))` means the hash
/// matched, and `Err` means it did not — the message names the plugin and the file, and says
/// exactly which comparison failed.
fn verify_integrity(dir: &Path, id: &str) -> Result<Option<String>, String> {
    let Some(expected) = expected_manifest_digest(dir)? else {
        return Ok(None);
    };
    let actual = manifest_digest(dir)?;
    if actual == expected {
        return Ok(Some(actual));
    }
    Err(format!(
        "integrity check failed for `{id}`: plugin.json hash does not match {PLUGIN_HASH_FILE}"
    ))
}

/// One line describing a directory's integrity state, for [`PluginRegistry::inspect`].
fn integrity_summary(dir: &Path) -> String {
    match expected_manifest_digest(dir) {
        Err(err) => format!("cannot be checked ({err})"),
        Ok(None) => "not verified (no plugin.sha256)".to_string(),
        Ok(Some(expected)) => match manifest_digest(dir) {
            Err(err) => format!("cannot be checked ({err})"),
            Ok(actual) if actual == expected => format!("plugin.sha256 matches ({actual})"),
            Ok(actual) => format!("plugin.sha256 MISMATCH: plugin.json hashes to {actual}"),
        },
    }
}

/// Is there a non-empty `plugin.sig`? Presence only — nothing here verifies a signature.
fn signature_present(dir: &Path) -> bool {
    let path = dir.join(PLUGIN_SIG_FILE);
    std::fs::read_to_string(&path)
        .map(|raw| !raw.trim().is_empty())
        .unwrap_or(false)
}

/// Render a dependency chain as `` `a` -> `b` -> `a` ``.
fn quote_chain(chain: &[String]) -> String {
    chain
        .iter()
        .map(|link| format!("`{link}`"))
        .collect::<Vec<_>>()
        .join(" -> ")
}

// ---- versions ------------------------------------------------------------

/// Strict semver core check: exactly `MAJOR.MINOR.PATCH`, all numeric.
pub fn is_semver(raw: &str) -> bool {
    let core = numeric_core(raw);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The part of a version string before any pre-release (`-`) or build (`+`) metadata.
fn numeric_core(raw: &str) -> &str {
    let trimmed = raw.trim();
    let cut = trimmed.find(['-', '+']).unwrap_or(trimmed.len());
    &trimmed[..cut]
}

/// Parse up to three numeric components; missing trailing components are zero.
fn numeric_parts(raw: &str) -> Option<(u64, u64, u64)> {
    let core = numeric_core(raw);
    if core.is_empty() {
        return None;
    }
    let mut numbers = [0u64; 3];
    let mut count = 0;
    for part in core.split('.') {
        if count >= 3 {
            return None;
        }
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        numbers[count] = part.parse().ok()?;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    Some((numbers[0], numbers[1], numbers[2]))
}

/// Does `version` satisfy the requirement `req`?
///
/// Supported forms: exact (`1.2.3`, optional `=` prefix), caret (`^1.2.3`), tilde (`~1.2.3`) and
/// `>=1.2.3`. An empty requirement or `*` matches anything. Anything else does not match.
pub fn requirement_satisfied(version: &str, req: &str) -> bool {
    let req = req.trim();
    if req.is_empty() || req == "*" {
        return true;
    }
    let Some(found) = numeric_parts(version) else {
        return false;
    };
    if let Some(rest) = req.strip_prefix(">=") {
        return numeric_parts(rest).map(|min| found >= min).unwrap_or(false);
    }
    if let Some(rest) = req.strip_prefix('^') {
        let Some(min) = numeric_parts(rest) else {
            return false;
        };
        if found < min {
            return false;
        }
        return if min.0 > 0 {
            found.0 == min.0
        } else if min.1 > 0 {
            found.0 == 0 && found.1 == min.1
        } else {
            found == min
        };
    }
    if let Some(rest) = req.strip_prefix('~') {
        let Some(min) = numeric_parts(rest) else {
            return false;
        };
        return found >= min && found.0 == min.0 && found.1 == min.1;
    }
    let exact = req.strip_prefix('=').unwrap_or(req);
    numeric_parts(exact)
        .map(|want| found == want)
        .unwrap_or(false)
}

pub trait PluginArgs {
    fn to_args(&self) -> Vec<String>;
}

impl PluginArgs for &str {
    fn to_args(&self) -> Vec<String> {
        if self.trim().is_empty() {
            Vec::new()
        } else {
            self.split_whitespace().map(str::to_owned).collect()
        }
    }
}

impl PluginArgs for String {
    fn to_args(&self) -> Vec<String> {
        self.as_str().to_args()
    }
}

impl<A> PluginArgs for &[A]
where
    A: AsRef<str>,
{
    fn to_args(&self) -> Vec<String> {
        self.iter().map(|s| s.as_ref().to_owned()).collect()
    }
}

impl<A> PluginArgs for Vec<A>
where
    A: AsRef<str>,
{
    fn to_args(&self) -> Vec<String> {
        self.iter().map(|s| s.as_ref().to_owned()).collect()
    }
}

impl<A, const N: usize> PluginArgs for [A; N]
where
    A: AsRef<str>,
{
    fn to_args(&self) -> Vec<String> {
        self.iter().map(|s| s.as_ref().to_owned()).collect()
    }
}

impl<A, const N: usize> PluginArgs for &[A; N]
where
    A: AsRef<str>,
{
    fn to_args(&self) -> Vec<String> {
        self.iter().map(|s| s.as_ref().to_owned()).collect()
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_plugin(root: &Path, dir: &str, body: &str) {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("plugin.json"), body).unwrap();
    }

    #[test]
    fn installed_and_loaded_are_distinct_counts() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "good",
            r#"{"name":"good","version":"1.0","entry":"main.js"}"#,
        );
        std::fs::write(dir.path().join("good/main.js"), "//").unwrap();
        write_plugin(dir.path(), "off", r#"{"name":"off","enabled":false}"#);
        write_plugin(dir.path(), "broken", "{not json");
        std::fs::write(dir.path().join("broken.txt"), "x").unwrap();

        let registry = PluginRegistry::scan(dir.path());
        assert_eq!(registry.installed(), 3);
        assert_eq!(registry.loaded(), 1);
    }

    #[test]
    fn a_missing_entry_point_is_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path(), "p", r#"{"name":"p","entry":"missing.js"}"#);
        let registry = PluginRegistry::scan(dir.path());
        assert_eq!(registry.installed(), 1);
        assert_eq!(registry.loaded(), 0);
        assert!(registry.plugins()[0].error.is_some());
    }

    #[test]
    fn a_missing_directory_is_an_empty_registry_not_an_error() {
        let registry = PluginRegistry::scan(Path::new("/nonexistent/vikram/plugins"));
        assert_eq!(registry.installed(), 0);
        assert_eq!(registry.loaded(), 0);
    }

    // ---- manifests -------------------------------------------------------

    #[test]
    fn legacy_manifest_still_parses() {
        let manifest = PluginManifest::parse(
            r#"{"name":"old","version":"1.0.0","entry":"main.js","enabled":true}"#,
        )
        .unwrap();
        assert_eq!(manifest.name, "old");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.entry, "main.js");
        assert!(manifest.enabled);
        // id falls back to name; everything else takes its default.
        assert_eq!(manifest.id(), "old");
        assert!(manifest.description.is_empty());
        assert!(manifest.dependencies.is_empty());
        assert!(manifest.components.is_none());
    }

    #[test]
    fn legacy_manifest_without_enabled_is_enabled() {
        let manifest = PluginManifest::parse(r#"{"name":"p","entry":"x.sh"}"#).unwrap();
        assert!(manifest.enabled);
    }

    #[test]
    fn rich_manifest_parses_every_field() {
        let raw = r#"{
            "id": "acme.tools",
            "name": "acme",
            "version": "1.4.2",
            "description": "acme tools",
            "author": "acme",
            "license": "MIT",
            "homepage": "https://example.com",
            "repository": "https://github.com/acme/tools",
            "min_vikram_version": "0.1.0",
            "api_version": "1",
            "components": {"tools": ["run"], "skills": ["s1"]},
            "dependencies": [{"name": "core", "version_req": "^1.0.0"}],
            "permissions": ["fs.write", "network"],
            "entry": "bin/main.sh",
            "enabled": true
        }"#;
        let manifest = PluginManifest::parse(raw).unwrap();
        assert_eq!(manifest.id(), "acme.tools");
        assert_eq!(manifest.name, "acme");
        assert_eq!(manifest.version, "1.4.2");
        assert_eq!(manifest.description, "acme tools");
        assert_eq!(manifest.author, "acme");
        assert_eq!(manifest.license, "MIT");
        assert_eq!(manifest.homepage.as_deref(), Some("https://example.com"));
        assert_eq!(
            manifest.repository.as_deref(),
            Some("https://github.com/acme/tools")
        );
        assert_eq!(manifest.min_vikram_version.as_deref(), Some("0.1.0"));
        assert_eq!(manifest.api_version.as_deref(), Some("1"));
        let components = manifest.components.as_ref().unwrap();
        assert_eq!(components["tools"], vec!["run".to_string()]);
        assert_eq!(components["skills"], vec!["s1".to_string()]);
        assert_eq!(manifest.dependencies.len(), 1);
        assert_eq!(manifest.dependencies[0].name, "core");
        assert_eq!(manifest.dependencies[0].version_req, "^1.0.0");
        assert_eq!(manifest.permissions, vec!["fs.write", "network"]);
        assert_eq!(manifest.entry, "bin/main.sh");
        assert!(manifest.enabled);
    }

    #[test]
    fn scan_reads_the_rich_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "rich",
            r#"{"id":"rich.id","name":"rich","version":"2.0.0","entry":"main.sh","enabled":false}"#,
        );
        let registry = PluginRegistry::scan(dir.path());
        assert_eq!(registry.plugins()[0].name, "rich");
        assert_eq!(registry.plugins()[0].version.as_deref(), Some("2.0.0"));
        assert_eq!(registry.plugins()[0].id(), "rich.id");
        assert!(!registry.plugins()[0].loaded);
    }

    // ---- semver ----------------------------------------------------------

    #[test]
    fn semver_shape_is_checked_strictly() {
        assert!(is_semver("1.2.3"));
        assert!(!is_semver("1.2"));
        assert!(!is_semver("1"));
        assert!(!is_semver("v1.2.3"));
        assert!(!is_semver(""));
        assert!(is_semver("1.2.3-beta.1"));
    }

    #[test]
    fn exact_requirements_match_only_the_same_version() {
        assert!(requirement_satisfied("1.2.3", "1.2.3"));
        assert!(requirement_satisfied("1.2.3", "=1.2.3"));
        assert!(!requirement_satisfied("1.2.4", "1.2.3"));
        assert!(!requirement_satisfied("2.0.0", "1.2.3"));
    }

    #[test]
    fn caret_requirements_stay_within_a_compatible_major() {
        assert!(requirement_satisfied("1.2.3", "^1.2.3"));
        assert!(requirement_satisfied("1.9.0", "^1.2.3"));
        assert!(!requirement_satisfied("2.0.0", "^1.2.3"));
        assert!(!requirement_satisfied("1.2.2", "^1.2.3"));
        // 0.x is minor-locked, 0.0.x is exact.
        assert!(requirement_satisfied("0.3.5", "^0.3.1"));
        assert!(!requirement_satisfied("0.4.0", "^0.3.1"));
        assert!(requirement_satisfied("0.0.3", "^0.0.3"));
        assert!(!requirement_satisfied("0.0.4", "^0.0.3"));
    }

    #[test]
    fn tilde_requirements_stay_within_a_minor() {
        assert!(requirement_satisfied("1.2.9", "~1.2.3"));
        assert!(!requirement_satisfied("1.3.0", "~1.2.3"));
        assert!(!requirement_satisfied("1.2.2", "~1.2.3"));
        assert!(requirement_satisfied("2.4.0", "~2.4"));
    }

    #[test]
    fn greater_or_equal_requirements_have_no_upper_bound() {
        assert!(requirement_satisfied("1.2.3", ">=1.2.3"));
        assert!(requirement_satisfied("9.9.9", ">=1.2.3"));
        assert!(!requirement_satisfied("1.2.2", ">=1.2.3"));
    }

    #[test]
    fn mismatched_and_unknown_requirements_do_not_satisfy() {
        assert!(!requirement_satisfied("1.0.0", "2.0.0"));
        assert!(!requirement_satisfied("not-a-version", "1.0.0"));
        assert!(!requirement_satisfied("1.0.0", "<2.0.0"));
        // A wildcard requirement accepts anything, including an unparseable version.
        assert!(requirement_satisfied("whatever", "*"));
        assert!(requirement_satisfied("whatever", ""));
    }

    // ---- dependency resolution ------------------------------------------

    #[test]
    fn resolve_dependencies_reports_a_missing_dependency() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "needs",
            r#"{"name":"needs","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
        );
        std::fs::write(dir.path().join("needs/main.sh"), "#!/bin/sh\n").unwrap();
        let registry = PluginRegistry::scan(dir.path());
        let problems = registry.resolve_dependencies();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, "needs");
        assert!(problems[0].1[0].contains("missing dependency `core`"));
    }

    #[test]
    fn resolve_dependencies_reports_an_unsatisfied_version() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "core",
            r#"{"name":"core","version":"1.0.0","entry":"main.sh"}"#,
        );
        std::fs::write(dir.path().join("core/main.sh"), "#!/bin/sh\n").unwrap();
        write_plugin(
            dir.path(),
            "needs",
            r#"{"name":"needs","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^2.0.0"}]}"#,
        );
        std::fs::write(dir.path().join("needs/main.sh"), "#!/bin/sh\n").unwrap();
        let registry = PluginRegistry::scan(dir.path());
        let problems = registry.resolve_dependencies();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, "needs");
        assert!(problems[0].1[0].contains("requires `^2.0.0`"));
        assert!(problems[0].1[0].contains("1.0.0"));
    }

    #[test]
    fn resolve_dependencies_is_empty_when_everything_is_satisfied() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "core",
            r#"{"name":"core","version":"1.5.0","entry":"main.sh"}"#,
        );
        std::fs::write(dir.path().join("core/main.sh"), "#!/bin/sh\n").unwrap();
        write_plugin(
            dir.path(),
            "needs",
            r#"{"name":"needs","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
        );
        std::fs::write(dir.path().join("needs/main.sh"), "#!/bin/sh\n").unwrap();
        let registry = PluginRegistry::scan(dir.path());
        assert!(registry.resolve_dependencies().is_empty());
    }

    // ---- permissions -----------------------------------------------------

    /// Write a plugin whose entry point echoes, so execution is observable.
    fn exec_plugin(dir: &Path, name: &str, permissions: &str) {
        let body = if permissions.trim().is_empty() {
            format!(r#"{{"name":"{name}","version":"1.0.0","entry":"main.sh"}}"#)
        } else {
            format!(
                r#"{{"name":"{name}","version":"1.0.0","entry":"main.sh","permissions":{permissions}}}"#
            )
        };
        write_plugin(dir, name, &body);
        std::fs::write(dir.join(name).join("main.sh"), "#!/bin/sh\necho hi\n").unwrap();
    }

    #[test]
    fn parse_permission_accepts_every_documented_spelling() {
        let cases: [(&str, PluginPermission); 18] = [
            ("fs.read", PluginPermission::FilesystemRead),
            ("filesystem.read", PluginPermission::FilesystemRead),
            ("fs.write", PluginPermission::FilesystemWrite),
            ("filesystem.write", PluginPermission::FilesystemWrite),
            ("network", PluginPermission::Network),
            ("process", PluginPermission::Process),
            ("exec", PluginPermission::Process),
            ("env", PluginPermission::Environment),
            ("environment", PluginPermission::Environment),
            ("secrets", PluginPermission::Secrets),
            ("skills", PluginPermission::Skills),
            ("tools", PluginPermission::Tools),
            ("agents", PluginPermission::Agents),
            ("hive", PluginPermission::Hive),
            ("bots", PluginPermission::Bots),
            ("ui", PluginPermission::Ui),
            ("tui", PluginPermission::Ui),
            ("background", PluginPermission::Background),
        ];
        for (spelling, expected) in cases {
            assert_eq!(parse_permission(spelling), Some(expected), "{spelling}");
        }
        // Case, surrounding space, scopes and separator variants are tolerated.
        assert_eq!(
            parse_permission(" FS.Read "),
            Some(PluginPermission::FilesystemRead)
        );
        assert_eq!(
            parse_permission("fs.read:workspace"),
            Some(PluginPermission::FilesystemRead)
        );
        assert_eq!(
            parse_permission("fs_write"),
            Some(PluginPermission::FilesystemWrite)
        );
        assert_eq!(
            parse_permission("fs:read"),
            Some(PluginPermission::FilesystemRead)
        );
        assert_eq!(parse_permission("PROCESS"), Some(PluginPermission::Process));
        // Anything else is unknown rather than a silent grant.
        for unknown in [
            "",
            "  ",
            "telepathy",
            "fs.exec",
            "network.admin",
            "telepathy:mind",
        ] {
            assert_eq!(parse_permission(unknown), None, "`{unknown}`");
        }
        // The canonical spelling always round-trips.
        for permission in PluginPermission::ALL {
            assert_eq!(parse_permission(permission.as_str()), Some(permission));
        }
    }

    #[test]
    fn execute_refuses_a_plugin_that_declares_no_permissions() {
        let dir = tempfile::tempdir().unwrap();
        exec_plugin(dir.path(), "quiet", "");
        let registry = PluginRegistry::scan(dir.path());
        assert_eq!(registry.loaded(), 1, "the plugin still loads");
        let err = registry.execute("quiet", "").unwrap_err();
        assert!(err.contains("declares no permissions"), "{err}");
        assert!(err.contains("quiet"), "{err}");
        assert!(err.contains("process"), "{err}");
    }

    #[test]
    fn execute_refuses_a_plugin_with_only_read_permissions() {
        let dir = tempfile::tempdir().unwrap();
        exec_plugin(dir.path(), "reader", r#"["fs.read"]"#);
        let registry = PluginRegistry::scan(dir.path());
        assert_eq!(registry.loaded(), 1);
        let err = registry.execute("reader", "").unwrap_err();
        assert!(err.contains("declares no execution permission"), "{err}");
        assert!(err.contains("process"), "{err}");
        assert!(
            registry
                .check_permissions("reader", &[PluginPermission::Process])
                .is_err()
        );
    }

    #[test]
    fn execute_runs_a_plugin_that_declares_an_execution_permission() {
        let dir = tempfile::tempdir().unwrap();
        exec_plugin(dir.path(), "runner", r#"["fs.read","process"]"#);
        let registry = PluginRegistry::scan(dir.path());
        let out = registry
            .execute("runner", "one two")
            .expect("`process` permits execution");
        assert!(out.contains("exit: 0"), "{out}");
        assert!(out.contains("hi"), "{out}");

        // `network` is the other execution-capable permission.
        exec_plugin(dir.path(), "net", r#"["network"]"#);
        let registry = PluginRegistry::scan(dir.path());
        assert!(registry.execute("net", "").is_ok());
    }

    #[test]
    fn permission_accessors_reflect_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        exec_plugin(
            dir.path(),
            "mixed",
            r#"["process","fs.read","fs.read","telepathy"]"#,
        );
        let registry = PluginRegistry::scan(dir.path());
        assert_eq!(
            registry.permissions("mixed"),
            vec![PluginPermission::Process, PluginPermission::FilesystemRead]
        );
        assert!(
            registry
                .check_permissions("mixed", &[PluginPermission::Process])
                .is_ok()
        );
        assert!(
            registry
                .check_permissions(
                    "mixed",
                    &[PluginPermission::FilesystemRead, PluginPermission::Process],
                )
                .is_ok()
        );
        assert!(registry.check_permissions("mixed", &[]).is_ok());

        let err = registry
            .check_permissions(
                "mixed",
                &[PluginPermission::Secrets, PluginPermission::Network],
            )
            .unwrap_err();
        assert!(err.contains("`secrets`"), "{err}");
        assert!(err.contains("`network`"), "{err}");
        assert!(!err.contains("`process`"), "{err}");

        // Unknown plugins resolve to no permissions, and checking them is an error.
        assert!(
            registry
                .permissions("definitely-not-installed-9f3a")
                .is_empty()
        );
        let err = registry
            .check_permissions("definitely-not-installed-9f3a", &[])
            .unwrap_err();
        assert!(err.contains("plugin not found"), "{err}");
    }

    #[test]
    fn validate_warns_about_unknown_permissions_and_components() {
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());
        let source = tempfile::tempdir().unwrap();
        write_plugin(
            source.path(),
            "odd",
            r#"{"name":"odd","version":"1.0.0","description":"d","entry":"main.sh",
                "permissions":["fs.read","telepathy"],
                "components":{"tools":["run"],"nonsense":["thing"],"UI":[]}}"#,
        );
        std::fs::write(source.path().join("odd/main.sh"), "#!/bin/sh\n").unwrap();
        let issues = registry.validate(&source.path().join("odd")).unwrap();
        let joined = issues.join("\n");
        assert!(
            joined.contains("warning: unknown permission `telepathy`"),
            "{joined}"
        );
        assert!(
            joined.contains("warning: unknown component type `nonsense`"),
            "{joined}"
        );
        // Known spellings — including a component type in the wrong case — are not reported.
        assert!(!joined.contains("fs.read"), "{joined}");
        assert!(!joined.contains("`tools`"), "{joined}");
        assert!(!joined.contains("`UI`"), "{joined}");
        // Warnings only: nothing here would block an install.
        assert!(
            issues.iter().all(|issue| issue.starts_with("warning:")),
            "{issues:?}"
        );
    }

    // ---- lifecycle -------------------------------------------------------

    fn plugin_source(root: &Path, name: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("plugin.json"), body).unwrap();
        std::fs::write(dir.join("main.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(dir.join("lib/helper.txt"), "helper").unwrap();
    }

    #[test]
    fn install_uninstall_round_trip() {
        let source = tempfile::tempdir().unwrap();
        plugin_source(
            source.path(),
            "src",
            r#"{"id":"acme","name":"acme","version":"1.0.0","entry":"main.sh"}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());

        let id = registry
            .install(&source.path().join("src"))
            .expect("install works");
        assert_eq!(id, "acme");
        assert!(installed.path().join("acme/plugin.json").is_file());
        assert!(installed.path().join("acme/main.sh").is_file());
        assert!(installed.path().join("acme/lib/helper.txt").is_file());

        let rescanned = PluginRegistry::scan(installed.path());
        assert_eq!(rescanned.installed(), 1);
        assert_eq!(rescanned.loaded(), 1);

        rescanned.uninstall("acme").expect("uninstall works");
        assert!(!installed.path().join("acme").exists());
        assert!(rescanned.uninstall("acme").is_err());
        assert_eq!(PluginRegistry::scan(installed.path()).installed(), 0);
    }

    #[test]
    fn install_refuses_to_overwrite() {
        let source = tempfile::tempdir().unwrap();
        plugin_source(
            source.path(),
            "src",
            r#"{"id":"acme","name":"acme","version":"1.0.0","entry":"main.sh"}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());
        registry.install(&source.path().join("src")).unwrap();
        let err = registry.install(&source.path().join("src")).unwrap_err();
        assert!(err.contains("already installed"), "{err}");
        assert!(err.contains("uninstall"), "{err}");
    }

    #[test]
    fn install_rejects_a_directory_without_a_manifest() {
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("empty")).unwrap();
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());
        let err = registry.install(&source.path().join("empty")).unwrap_err();
        assert!(err.contains("no plugin.json"), "{err}");
    }

    #[test]
    fn install_rejects_a_missing_entry_point() {
        let source = tempfile::tempdir().unwrap();
        write_plugin(
            source.path(),
            "src",
            r#"{"id":"acme","name":"acme","version":"1.0.0","entry":"nope.sh"}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());
        let err = registry.install(&source.path().join("src")).unwrap_err();
        assert!(err.contains("entry point"), "{err}");
    }

    #[test]
    fn enable_and_disable_rewrite_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "p",
            r#"{"name":"p","version":"1.0.0","entry":"main.sh","enabled":true}"#,
        );
        std::fs::write(dir.path().join("p/main.sh"), "#!/bin/sh\n").unwrap();
        let registry = PluginRegistry::scan(dir.path());
        assert_eq!(registry.loaded(), 1);

        registry.disable("p").expect("disable works");
        let manifest = PluginManifest::from_dir(&dir.path().join("p")).unwrap();
        assert!(!manifest.enabled);
        assert_eq!(PluginRegistry::scan(dir.path()).loaded(), 0);

        registry.enable("p").expect("enable works");
        let manifest = PluginManifest::from_dir(&dir.path().join("p")).unwrap();
        assert!(manifest.enabled);
        assert_eq!(PluginRegistry::scan(dir.path()).loaded(), 1);

        // Unknown ids are an error, and no temp file is left behind.
        assert!(registry.enable("nope").is_err());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("p"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn inspect_summarises_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "acme",
            r#"{"id":"acme","name":"acme","version":"1.2.3","description":"does things",
                "author":"acme","license":"MIT","entry":"main.sh",
                "components":{"tools":["run"],"skills":["s1"]},
                "dependencies":[{"name":"core","version_req":"^1.0.0"}],
                "permissions":["fs.write"]}"#,
        );
        std::fs::write(dir.path().join("acme/main.sh"), "#!/bin/sh\n").unwrap();
        let summary = PluginRegistry::scan(dir.path()).inspect("acme").unwrap();
        for needle in [
            "id: acme",
            "name: acme",
            "version: 1.2.3",
            "description: does things",
            "author: acme",
            "license: MIT",
            "entry: main.sh",
            "enabled: true",
            "components: skills: s1; tools: run",
            "dependencies: core ^1.0.0",
            "permissions: fs.write",
        ] {
            assert!(
                summary.contains(needle),
                "missing `{needle}` in:\n{summary}"
            );
        }
        assert!(PluginRegistry::scan(dir.path()).inspect("nope").is_err());
    }

    #[test]
    fn validate_reports_manifest_problems() {
        let installed = tempfile::tempdir().unwrap();
        write_plugin(
            installed.path(),
            "core",
            r#"{"name":"core","version":"1.0.0","entry":"main.sh"}"#,
        );
        std::fs::write(installed.path().join("core/main.sh"), "#!/bin/sh\n").unwrap();
        let registry = PluginRegistry::scan(installed.path());

        let source = tempfile::tempdir().unwrap();
        write_plugin(
            source.path(),
            "bad",
            r#"{"name":"bad","version":"1.0","entry":"missing.sh",
                "min_vikram_version":"9.9.9",
                "dependencies":[{"name":"ghost","version_req":"^1.0.0"}]}"#,
        );
        let issues = registry.validate(&source.path().join("bad")).unwrap();
        let joined = issues.join("\n");
        assert!(joined.contains("is not semver"), "{joined}");
        assert!(joined.contains("does not exist"), "{joined}");
        assert!(joined.contains("requires vikram >= 9.9.9"), "{joined}");
        assert!(
            joined.contains("dependency `ghost` is not installed"),
            "{joined}"
        );
        assert!(issues.iter().any(|i| i.starts_with("warning:")));
        assert!(!issues.is_empty());

        // A clean manifest against a satisfied dependency has nothing to report.
        write_plugin(
            source.path(),
            "good",
            r#"{"name":"good","version":"2.0.0","description":"d","entry":"main.sh",
                "min_vikram_version":"0.0.1",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
        );
        std::fs::write(source.path().join("good/main.sh"), "#!/bin/sh\n").unwrap();
        let clean = registry.validate(&source.path().join("good")).unwrap();
        assert!(clean.is_empty(), "{clean:?}");

        // A missing manifest is a hard error, not a warning list.
        assert!(registry.validate(&source.path().join("absent")).is_err());
    }

    #[test]
    fn lockfile_is_sorted_and_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(
            dir.path(),
            "zulu",
            r#"{"name":"zulu","version":"1.0.0","entry":"main.sh"}"#,
        );
        std::fs::write(dir.path().join("zulu/main.sh"), "#!/bin/sh\n").unwrap();
        write_plugin(
            dir.path(),
            "alpha",
            r#"{"id":"alpha","name":"alpha","version":"2.0.0","entry":"main.sh"}"#,
        );
        std::fs::write(dir.path().join("alpha/main.sh"), "#!/bin/sh\n").unwrap();
        let registry = PluginRegistry::scan(dir.path());
        let path = registry.lockfile().unwrap();
        assert_eq!(path, dir.path().join(".lock"));
        let first = std::fs::read_to_string(&path).unwrap();
        registry.lockfile().unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        let entries = value["plugins"].as_array().unwrap();
        assert_eq!(entries[0]["id"], "alpha");
        assert_eq!(entries[0]["version"], "2.0.0");
        assert_eq!(entries[1]["id"], "zulu");
        // The lockfile is not itself a plugin directory.
        assert_eq!(PluginRegistry::scan(dir.path()).installed(), 2);
    }

    // ---- sha256 ----------------------------------------------------------

    #[test]
    fn sha256_matches_the_published_vectors() {
        // NIST / RFC 6234 vectors, cross-checked with `sha256sum`.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"The quick brown fox jumps over the lazy dog"),
            "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
        );
        // Two-block message: padding spills into a second block.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // The 112-byte NIST vector: two blocks of message plus padding. The `\` continuation keeps
        // the literal free of the whitespace the source indentation would otherwise add.
        const LONG: &str = "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmn\
                            hijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
        assert_eq!(
            sha256_hex(LONG.as_bytes()),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
        // Padding boundaries: 55 bytes fits the length field in the same block, 56 needs a second.
        assert_eq!(
            sha256_hex(&[b'x'; 55]),
            "d5e285683cd4efc02d021a5c62014694958901005d6f71e89e0989fac77e4072"
        );
        assert_eq!(
            sha256_hex(&[b'x'; 56]),
            "04c26261370ee7541549d16dee320c723e3fd14671e66a099afe0a377c16888e"
        );
        assert_eq!(
            sha256_hex(&[b'x'; 64]),
            "7ce100971f64e7001e8fe5a51973ecdfe1ced42befe7ee8d5fd6219506b5393c"
        );
        // A megabyte of `a`: many blocks, and the length no longer fits a small integer.
        assert_eq!(
            sha256_hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        // The byte form and the hex form agree, and the digest is 32 bytes.
        assert_eq!(sha256(b"abc").len(), 32);
        assert_eq!(sha256(b"abc")[0], 0xba);
        assert_eq!(sha256_hex(b"abc").len(), 64);
    }

    #[test]
    fn sha256_is_sensitive_to_a_single_bit() {
        assert_ne!(sha256_hex(b"plugin-a"), sha256_hex(b"plugin-b"));
        assert_ne!(sha256_hex(b"abc"), sha256_hex(b"abd"));
        assert_eq!(sha256_hex(b"abc"), sha256_hex(b"abc"));
    }

    // ---- integrity -------------------------------------------------------

    /// Write `plugin.sha256` covering the `plugin.json` that is on disk right now.
    fn write_hash(dir: &Path) {
        let body = std::fs::read(dir.join("plugin.json")).unwrap();
        std::fs::write(
            dir.join(PLUGIN_HASH_FILE),
            format!("{}\n", sha256_hex(&body)),
        )
        .unwrap();
    }

    const SIGNED_MANIFEST: &str = r#"{"id":"signed","name":"signed","version":"1.0.0",
        "description":"d","entry":"main.sh"}"#;

    #[test]
    fn a_matching_hash_installs_and_validates_cleanly() {
        let source = tempfile::tempdir().unwrap();
        plugin_source(source.path(), "signed", SIGNED_MANIFEST);
        let dir = source.path().join("signed");
        write_hash(&dir);

        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());
        let issues = registry.validate(&dir).unwrap();
        assert!(issues.is_empty(), "{issues:?}");
        let integrity = registry.validate_integrity(&dir).unwrap();
        assert!(integrity.is_empty(), "{integrity:?}");
        assert_eq!(registry.install(&dir).unwrap(), "signed");
        // The hash file travels with the plugin so a later inspect can re-check it.
        assert!(installed.path().join("signed/plugin.sha256").is_file());
        let summary = PluginRegistry::scan(installed.path())
            .inspect("signed")
            .unwrap();
        assert!(
            summary.contains("integrity: plugin.sha256 matches"),
            "{summary}"
        );
    }

    #[test]
    fn a_tampered_manifest_is_refused_by_validate_and_install() {
        let source = tempfile::tempdir().unwrap();
        plugin_source(source.path(), "signed", SIGNED_MANIFEST);
        let dir = source.path().join("signed");
        write_hash(&dir);
        // Change the manifest after the hash was taken.
        std::fs::write(
            dir.join("plugin.json"),
            SIGNED_MANIFEST.replace(r#""version":"1.0.0""#, r#""version":"9.9.9""#),
        )
        .unwrap();

        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());

        let issues = registry.validate(&dir).unwrap();
        let joined = issues.join("\n");
        assert!(
            joined.contains(
                "integrity check failed for `signed`: plugin.json hash does not match plugin.sha256"
            ),
            "{joined}"
        );

        let err = registry.install(&dir).unwrap_err();
        assert!(
            err.contains(
                "integrity check failed for `signed`: plugin.json hash does not match plugin.sha256"
            ),
            "{err}"
        );
        // Nothing was copied: a failed check must not leave a half-installed plugin behind.
        assert!(!installed.path().join("signed").exists());

        // The integrity-only view reports the same failure as an error.
        let integrity = registry.validate_integrity(&dir).unwrap();
        assert!(
            integrity
                .iter()
                .any(|issue| issue.starts_with("error: integrity check failed")),
            "{integrity:?}"
        );
    }

    #[test]
    fn a_missing_hash_warns_without_blocking() {
        let source = tempfile::tempdir().unwrap();
        plugin_source(source.path(), "unsigned", SIGNED_MANIFEST);
        let dir = source.path().join("unsigned");
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());

        assert_eq!(
            registry.validate_integrity(&dir).unwrap(),
            vec!["warning: no plugin.sha256 present (integrity not verified)".to_string()]
        );
        // No hash is not an error: `validate` is quiet and `install` still works.
        assert!(registry.validate(&dir).unwrap().is_empty());
        assert_eq!(registry.install(&dir).unwrap(), "signed");
    }

    #[test]
    fn inspect_reports_signature_presence_without_claiming_verification() {
        let dir = tempfile::tempdir().unwrap();
        plugin_source(dir.path(), "sigged", SIGNED_MANIFEST);
        std::fs::write(
            dir.path().join("sigged/plugin.sig"),
            "-----BEGIN SIGNATURE-----\nnot-a-real-signature\n",
        )
        .unwrap();
        let summary = PluginRegistry::scan(dir.path()).inspect("sigged").unwrap();
        assert!(
            summary.contains("signature: present (not cryptographically verified)"),
            "{summary}"
        );
        assert!(
            summary.contains("integrity: not verified (no plugin.sha256)"),
            "{summary}"
        );
        assert!(!summary.contains("signature verified"), "{summary}");

        // No signature file, and an empty one, both read as absent.
        plugin_source(dir.path(), "bare", SIGNED_MANIFEST);
        let bare = PluginRegistry::scan(dir.path()).inspect("bare").unwrap();
        assert!(bare.contains("signature: (none)"), "{bare}");
        std::fs::write(dir.path().join("bare/plugin.sig"), "   \n").unwrap();
        let empty_sig = PluginRegistry::scan(dir.path()).inspect("bare").unwrap();
        assert!(empty_sig.contains("signature: (none)"), "{empty_sig}");
    }

    // ---- dependency auto-install -----------------------------------------

    #[test]
    fn installing_a_plugin_installs_its_dependencies_first() {
        let sources = tempfile::tempdir().unwrap();
        plugin_source(
            sources.path(),
            "core",
            r#"{"id":"core","name":"core","version":"1.2.0","entry":"main.sh"}"#,
        );
        plugin_source(
            sources.path(),
            "app",
            r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());

        let order = registry
            .install_with_dependencies(&sources.path().join("app"))
            .expect("dependencies resolve from the registry");
        assert_eq!(order, vec!["core".to_string(), "app".to_string()]);
        assert!(installed.path().join("core/plugin.json").is_file());
        assert!(installed.path().join("app/plugin.json").is_file());
        assert!(installed.path().join("core/main.sh").is_file());
        assert_eq!(PluginRegistry::scan(installed.path()).installed(), 2);
        // The dependency graph is now consistent.
        assert!(
            PluginRegistry::scan(installed.path())
                .resolve_dependencies()
                .is_empty()
        );
    }

    #[test]
    fn install_takes_the_dependency_path_when_dependencies_are_declared() {
        let sources = tempfile::tempdir().unwrap();
        plugin_source(
            sources.path(),
            "core",
            r#"{"id":"core","name":"core","version":"1.4.0","entry":"main.sh"}"#,
        );
        plugin_source(
            sources.path(),
            "app",
            r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());

        // `install` keeps its signature and still returns the plugin's own id.
        assert_eq!(
            registry.install(&sources.path().join("app")).unwrap(),
            "app"
        );
        assert!(installed.path().join("core/plugin.json").is_file());
        assert!(installed.path().join("app/plugin.json").is_file());
    }

    #[test]
    fn a_transitive_chain_installs_depth_first() {
        let sources = tempfile::tempdir().unwrap();
        plugin_source(
            sources.path(),
            "base",
            r#"{"id":"base","name":"base","version":"0.1.0","entry":"main.sh"}"#,
        );
        plugin_source(
            sources.path(),
            "mid",
            r#"{"id":"mid","name":"mid","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"base","version_req":"^0.1.0"}]}"#,
        );
        plugin_source(
            sources.path(),
            "app",
            r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"mid","version_req":"^1.0.0"}]}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());
        let order = registry
            .install_with_dependencies(&sources.path().join("app"))
            .unwrap();
        assert_eq!(
            order,
            vec!["base".to_string(), "mid".to_string(), "app".to_string()]
        );
    }

    #[test]
    fn a_shared_dependency_is_installed_once() {
        let sources = tempfile::tempdir().unwrap();
        plugin_source(
            sources.path(),
            "d",
            r#"{"id":"d","name":"d","version":"1.0.0","entry":"main.sh"}"#,
        );
        for leaf in ["b", "c"] {
            plugin_source(
                sources.path(),
                leaf,
                &format!(
                    r#"{{"id":"{leaf}","name":"{leaf}","version":"1.0.0","entry":"main.sh",
                        "dependencies":[{{"name":"d","version_req":"^1.0.0"}}]}}"#
                ),
            );
        }
        plugin_source(
            sources.path(),
            "a",
            r#"{"id":"a","name":"a","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"b","version_req":"^1.0.0"},
                                {"name":"c","version_req":"^1.0.0"}]}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());
        let order = registry
            .install_with_dependencies(&sources.path().join("a"))
            .unwrap();
        // `d` is reached twice but installed once, and never re-copied over itself.
        assert_eq!(
            order,
            vec![
                "d".to_string(),
                "b".to_string(),
                "c".to_string(),
                "a".to_string()
            ]
        );
    }

    #[test]
    fn a_sibling_directory_is_searched_before_the_registry() {
        let sources = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        plugin_source(
            sources.path(),
            "core",
            r#"{"id":"core","name":"core","version":"1.1.0","entry":"main.sh"}"#,
        );
        plugin_source(
            elsewhere.path(),
            "core",
            r#"{"id":"core","name":"core","version":"1.0.0","entry":"main.sh"}"#,
        );
        plugin_source(
            sources.path(),
            "app",
            r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path()).with_registry_dir(elsewhere.path());
        registry
            .install_with_dependencies(&sources.path().join("app"))
            .unwrap();
        // The sibling copy (1.1.0), not the registry copy (1.0.0), is what got installed.
        let rescanned = PluginRegistry::scan(installed.path());
        assert_eq!(
            rescanned.installed_version("core").as_deref(),
            Some("1.1.0")
        );
    }

    #[test]
    fn a_missing_dependency_names_itself_and_the_places_searched() {
        let sources = tempfile::tempdir().unwrap();
        let registry_dir = tempfile::tempdir().unwrap();
        plugin_source(
            sources.path(),
            "app",
            r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"ghost","version_req":"^1.0.0"}]}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry =
            PluginRegistry::scan(installed.path()).with_registry_dir(registry_dir.path());

        let err = registry
            .install_with_dependencies(&sources.path().join("app"))
            .unwrap_err();
        assert!(err.contains("missing dependency `ghost`"), "{err}");
        assert!(err.contains("^1.0.0"), "{err}");
        assert!(err.contains("`app`"), "{err}");
        // Both search locations are named, so the operator can put the plugin where it is looked
        // for instead of guessing.
        assert!(
            err.contains(&sources.path().join("ghost").display().to_string()),
            "{err}"
        );
        assert!(
            err.contains(&registry_dir.path().join("ghost").display().to_string()),
            "{err}"
        );
        // A failed resolution installs nothing at all.
        assert_eq!(PluginRegistry::scan(installed.path()).installed(), 0);
        assert!(!installed.path().join("app").exists());
    }

    #[test]
    fn a_dependency_cycle_is_refused() {
        let sources = tempfile::tempdir().unwrap();
        plugin_source(
            sources.path(),
            "alpha",
            r#"{"id":"alpha","name":"alpha","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"beta","version_req":"^1.0.0"}]}"#,
        );
        plugin_source(
            sources.path(),
            "beta",
            r#"{"id":"beta","name":"beta","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"alpha","version_req":"^1.0.0"}]}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());
        let err = registry
            .install_with_dependencies(&sources.path().join("alpha"))
            .unwrap_err();
        assert!(err.contains("dependency cycle detected"), "{err}");
        assert!(err.contains("`alpha` -> `beta` -> `alpha`"), "{err}");
        assert_eq!(PluginRegistry::scan(installed.path()).installed(), 0);
    }

    #[test]
    fn a_self_dependency_is_a_cycle() {
        let sources = tempfile::tempdir().unwrap();
        plugin_source(
            sources.path(),
            "navel",
            r#"{"id":"navel","name":"navel","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"navel","version_req":"^1.0.0"}]}"#,
        );
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());
        let err = registry
            .install_with_dependencies(&sources.path().join("navel"))
            .unwrap_err();
        assert!(err.contains("dependency cycle detected"), "{err}");
    }

    #[test]
    fn an_installed_but_unsatisfying_dependency_is_refused_not_replaced() {
        let sources = tempfile::tempdir().unwrap();
        let installed = tempfile::tempdir().unwrap();
        plugin_source(
            installed.path(),
            "core",
            r#"{"id":"core","name":"core","version":"2.0.0","entry":"main.sh"}"#,
        );
        plugin_source(
            sources.path(),
            "app",
            r#"{"id":"app","name":"app","version":"1.0.0","entry":"main.sh",
                "dependencies":[{"name":"core","version_req":"^1.0.0"}]}"#,
        );
        let registry = PluginRegistry::scan(installed.path()).with_registry_dir(sources.path());
        let err = registry
            .install_with_dependencies(&sources.path().join("app"))
            .unwrap_err();
        assert!(err.contains("is installed at `2.0.0`"), "{err}");
        assert!(err.contains("^1.0.0"), "{err}");
        // The installed plugin is left exactly as it was.
        assert_eq!(
            PluginRegistry::scan(installed.path())
                .installed_version("core")
                .as_deref(),
            Some("2.0.0")
        );
        assert!(!installed.path().join("app").exists());
    }

    // ---- registry --------------------------------------------------------

    #[test]
    fn registry_index_lists_local_plugins_sorted_by_id() {
        let marketplace = tempfile::tempdir().unwrap();
        plugin_source(
            marketplace.path(),
            "zeta",
            r#"{"id":"zeta","name":"zeta","version":"0.9.0","entry":"main.sh"}"#,
        );
        plugin_source(
            marketplace.path(),
            "acme",
            r#"{"id":"acme.tools","name":"acme","version":"2.0.0","entry":"main.sh"}"#,
        );
        // Neither of these is a plugin, and neither may appear in the index.
        std::fs::create_dir_all(marketplace.path().join("not-a-plugin")).unwrap();
        std::fs::write(marketplace.path().join("loose.json"), "{}").unwrap();
        std::fs::create_dir_all(marketplace.path().join("broken")).unwrap();
        std::fs::write(marketplace.path().join("broken/plugin.json"), "{oops").unwrap();

        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path()).with_registry_dir(marketplace.path());
        assert_eq!(
            registry.registry_index().unwrap(),
            vec![
                (
                    "acme.tools".to_string(),
                    "acme".to_string(),
                    "2.0.0".to_string()
                ),
                ("zeta".to_string(), "zeta".to_string(), "0.9.0".to_string()),
            ]
        );
    }

    #[test]
    fn a_missing_registry_is_an_empty_index() {
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path())
            .with_registry_dir(installed.path().join("no-such-registry"));
        assert!(registry.registry_index().unwrap().is_empty());
    }

    #[test]
    fn the_default_registry_lives_under_the_vikram_home() {
        let installed = tempfile::tempdir().unwrap();
        assert_eq!(
            PluginRegistry::scan(installed.path()).registry_root(),
            registry_dir()
        );
        assert!(
            registry_dir().ends_with("plugin-registry"),
            "{}",
            registry_dir().display()
        );
    }
}
