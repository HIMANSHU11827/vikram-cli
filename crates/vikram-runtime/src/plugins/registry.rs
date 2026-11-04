use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::plugins_dir;

#[derive(Clone, Debug, Default)]
pub struct PluginRegistry {
    pub(crate) plugins: Vec<PluginState>,
    /// Directory the registry was scanned from; lifecycle operations target it. Falling back to
    /// [`plugins_dir`] keeps `PluginRegistry::default()` usable.
    pub(crate) dir: Option<PathBuf>,
    /// Source registry searched for dependencies. `None` means [`registry_dir`]; tests and callers
    /// that keep their own marketplace point it somewhere else with [`Self::with_registry_dir`].
    pub(crate) registry: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub(crate) struct LockEntry {
    pub(crate) id: String,
    pub(crate) version: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct LockBody {
    pub(crate) plugins: Vec<LockEntry>,
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
            // Prefer python3, fall back to python (Windows often only has `python`).
            let interpreter = if crate::computer::which("python3").is_some() {
                "python3"
            } else {
                "python"
            };
            let mut c = std::process::Command::new(interpreter);
            c.arg(&entry_path);
            c
        } else if entry.ends_with(".sh") {
            // Shell scripts need a POSIX shell; on Windows try bash (Git Bash/MSYS) then sh.
            let interpreter = if cfg!(windows) {
                if crate::computer::which("bash").is_some() {
                    "bash"
                } else {
                    "sh"
                }
            } else {
                "sh"
            };
            let mut c = std::process::Command::new(interpreter);
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
}

use crate::plugins::*;
