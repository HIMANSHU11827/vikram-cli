//! `vikram modules` — browse and install the shipped hook-module catalog.
//!
//! The catalog is a directory of ordinary plugin folders (`modules/` in the source tree, or
//! whatever `--from`/`VIKRAM_MODULES_DIR` names). Installing one copies it into the plugin
//! directory; the runtime picks up its hooks on the next plugin reload.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use vikram_runtime::hooks::HookRegistry;
use vikram_runtime::plugins::{PluginManifest, PluginRegistry};

#[derive(Debug, Subcommand)]
pub enum ModulesAction {
    /// List the catalog (default).
    List {
        /// Catalog directory (defaults to $VIKRAM_MODULES_DIR, then ./modules).
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Install one module, or all of them.
    Install {
        /// Module id to install.
        id: Option<String>,
        /// Install every module in the catalog.
        #[arg(long)]
        all: bool,
        /// Catalog directory (defaults to $VIKRAM_MODULES_DIR, then ./modules).
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Show the hooks the installed plugins contribute, in execution order.
    Hooks,
}

pub fn run(action: ModulesAction) -> Result<(), String> {
    match action {
        ModulesAction::List { from } => list(&catalog_dir(from)),
        ModulesAction::Install { id, all, from } => install(&catalog_dir(from), id.as_deref(), all),
        ModulesAction::Hooks => hooks(),
    }
}

fn catalog_dir(from: Option<PathBuf>) -> PathBuf {
    from.or_else(|| std::env::var_os("VIKRAM_MODULES_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("modules"))
}

fn modules(dir: &Path) -> Result<Vec<PluginManifest>, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read catalog at {}: {e}", dir.display()))?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || !path.join("plugin.json").is_file() {
            continue;
        }
        let manifest =
            PluginManifest::from_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        found.push(manifest);
    }
    found.sort_by(|a, b| a.id().cmp(b.id()));
    Ok(found)
}

fn list(dir: &Path) -> Result<(), String> {
    let found = modules(dir)?;
    if found.is_empty() {
        return Err(format!("no modules found in {}", dir.display()));
    }
    println!("{} module(s) in {}", found.len(), dir.display());
    for manifest in found {
        let points = manifest
            .components
            .as_ref()
            .and_then(|components| components.get("hooks"))
            .map(|points| points.join(", "))
            .unwrap_or_else(|| "—".to_owned());
        println!(
            "  {:<32} {:<18} {}",
            manifest.id(),
            points,
            manifest.description
        );
    }
    Ok(())
}

fn install(dir: &Path, id: Option<&str>, all: bool) -> Result<(), String> {
    let found = modules(dir)?;
    let selected: Vec<PluginManifest> = match (id, all) {
        (Some(id), false) => {
            let manifest = found
                .into_iter()
                .find(|manifest| manifest.id() == id)
                .ok_or_else(|| format!("no module `{id}` in {}", dir.display()))?;
            vec![manifest]
        }
        (None, true) => found,
        (Some(_), true) => {
            return Err("pass either a module id or --all, not both".to_owned());
        }
        (None, false) => {
            return Err("name a module id, or pass --all".to_owned());
        }
    };
    let registry = PluginRegistry::from_default_dir();
    let mut failures: Vec<String> = Vec::new();
    for manifest in &selected {
        let id = manifest.id();
        match registry.install(&dir.join(id)) {
            Ok(installed) => println!("installed {installed}"),
            Err(error) => failures.push(format!("{id}: {error}")),
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "{} installed, {} not: {}",
            selected.len() - failures.len(),
            failures.len(),
            failures.join("; ")
        ));
    }
    println!("{} module(s) installed", selected.len());
    Ok(())
}

fn hooks() -> Result<(), String> {
    let registry = PluginRegistry::from_default_dir();
    let hooks = HookRegistry::from_plugins(&registry);
    if hooks.hooks().is_empty() {
        println!(
            "no plugin hooks are active; install the catalog with `vikram modules install --all`"
        );
    }
    for info in hooks.entries() {
        println!(
            "  {:<32} {:<8} priority {:<4} {:<24} {}",
            info.plugin,
            info.kind,
            info.priority,
            info.points
                .iter()
                .map(|point| point.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            info.description
        );
    }
    for rejection in hooks.rejected() {
        println!("  refused {}: {}", rejection.plugin, rejection.reason);
    }
    Ok(())
}
