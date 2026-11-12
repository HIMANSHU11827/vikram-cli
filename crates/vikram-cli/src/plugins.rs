//! `vikram plugins` — install, inspect, and toggle plugins under `~/.vikram/plugins`.
//!
//! Everything here is local filesystem work on the same `PluginRegistry` the runtime scans: no
//! network, no marketplace fetch. The source registry at `~/.vikram/plugin-registry` is listed by
//! `plugins registry` and searched for dependencies at install time.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use vikram_runtime::plugins::{PluginManifest, PluginRegistry};

#[derive(Debug, Subcommand)]
pub enum PluginsAction {
    /// List installed plugins (default).
    List,
    /// Show one plugin's manifest in full.
    Show { id: String },
    /// Validate a plugin directory before installing it.
    Validate { path: PathBuf },
    /// Install a plugin from a local directory.
    Install {
        path: PathBuf,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Remove an installed plugin.
    Uninstall {
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Enable a plugin.
    Enable { id: String },
    /// Disable a plugin without removing it.
    Disable { id: String },
    /// List the permissions a plugin declares.
    Permissions { id: String },
    /// Run a plugin's entry point with arguments.
    Run {
        id: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List the local source registry: plugin folders dependencies resolve against.
    Registry,
}

pub fn run(action: PluginsAction) -> Result<(), String> {
    let registry = PluginRegistry::from_default_dir();
    match action {
        PluginsAction::List => list(&registry),
        PluginsAction::Show { id } => {
            println!("{}", registry.inspect(&id)?);
            Ok(())
        }
        PluginsAction::Validate { path } => {
            let issues = registry.validate(&path)?;
            if issues.is_empty() {
                println!("{}: no problems", path.display());
            } else {
                for issue in &issues {
                    println!("{issue}");
                }
            }
            Ok(())
        }
        PluginsAction::Install { path, yes } => install(&registry, &path, yes),
        PluginsAction::Uninstall { id, yes } => uninstall(&registry, &id, yes),
        PluginsAction::Enable { id } => {
            registry.enable(&id)?;
            println!("enabled {id}");
            Ok(())
        }
        PluginsAction::Disable { id } => {
            registry.disable(&id)?;
            println!("disabled {id}");
            Ok(())
        }
        PluginsAction::Permissions { id } => {
            let permissions = registry.permissions(&id);
            if permissions.is_empty() {
                println!("{id}: declares no permissions");
            } else {
                for permission in permissions {
                    println!("{}", permission.as_str());
                }
            }
            Ok(())
        }
        PluginsAction::Run { id, args } => {
            println!("{}", registry.execute(&id, args)?);
            Ok(())
        }
        PluginsAction::Registry => {
            let index = registry.registry_index()?;
            if index.is_empty() {
                println!(
                    "source registry is empty ({})",
                    registry.registry_root().display()
                );
                return Ok(());
            }
            for (id, name, version) in index {
                println!("{id:<24} {name:<24} {version}");
            }
            Ok(())
        }
    }
}

fn list(registry: &PluginRegistry) -> Result<(), String> {
    let plugins = registry.plugins();
    if plugins.is_empty() {
        println!(
            "no plugins installed ({})",
            vikram_runtime::config::plugins_dir().display()
        );
        return Ok(());
    }
    println!("{:<20} {:<10} {:<9} ENTRY", "NAME", "VERSION", "STATE");
    for plugin in plugins {
        let state = if let Some(err) = &plugin.error {
            format!("error: {err}")
        } else if !plugin.loaded {
            // Not loaded with no recorded error means the manifest is disabled.
            match PluginManifest::from_dir(&plugin.path) {
                Ok(manifest) if !manifest.enabled => "disabled".to_owned(),
                _ => "not loaded".to_owned(),
            }
        } else {
            "loaded".to_owned()
        };
        println!(
            "{:<20} {:<10} {:<9} {}",
            plugin.name,
            plugin.version.clone().unwrap_or_else(|| "-".to_owned()),
            state,
            plugin.entry.clone().unwrap_or_default()
        );
    }
    Ok(())
}

fn install(registry: &PluginRegistry, path: &Path, yes: bool) -> Result<(), String> {
    let manifest =
        PluginManifest::from_dir(path).map_err(|err| format!("not a plugin directory: {err}"))?;
    let id = manifest.id();
    if !yes {
        confirm(&format!(
            "install plugin `{id}` from {}? [y/N] ",
            path.display()
        ))?;
    }
    let installed = registry.install(path)?;
    println!("installed {installed}");
    Ok(())
}

fn uninstall(registry: &PluginRegistry, id: &str, yes: bool) -> Result<(), String> {
    if !yes {
        confirm(&format!("uninstall plugin `{id}`? [y/N] "))?;
    }
    registry.uninstall(id)?;
    println!("uninstalled {id}");
    Ok(())
}

fn confirm(question: &str) -> Result<(), String> {
    use std::io::Write;
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return Err("refusing to continue without --yes (stdin is not a terminal)".to_owned());
    }
    eprint!("{question}");
    std::io::stderr().flush().map_err(|err| err.to_string())?;
    let mut answer = String::new();
    stdin
        .read_line(&mut answer)
        .map_err(|err| format!("cannot read the answer: {err}"))?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err("aborted".to_owned()),
    }
}
