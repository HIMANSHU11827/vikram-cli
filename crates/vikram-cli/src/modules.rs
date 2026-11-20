//! `vikram modules` — browse and install the shipped hook-module catalog.
//!
//! The catalog is a directory of ordinary plugin folders (`modules/` in the source tree, or
//! whatever `--from`/`VIKRAM_MODULES_DIR` names). Installing one copies it into the plugin
//! directory; the runtime picks up its hooks on the next plugin reload.

use std::io::IsTerminal;
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
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
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
        ModulesAction::Install { id, all, yes, from } => {
            install(&catalog_dir(from), id.as_deref(), all, yes)
        }
        ModulesAction::Hooks => hooks(),
    }
}

fn catalog_dir(from: Option<PathBuf>) -> PathBuf {
    from.or_else(|| std::env::var_os("VIKRAM_MODULES_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("modules"))
}

/// A catalog entry: the directory the manifest was read from, paired with the manifest itself.
/// Installing uses this path rather than re-joining `dir` with the manifest id, so a hostile
/// `id` like `../x` cannot redirect the copy outside the catalog.
type ModuleEntry = (PathBuf, PluginManifest);

fn modules(dir: &Path) -> Result<Vec<ModuleEntry>, String> {
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
        found.push((path, manifest));
    }
    found.sort_by(|a, b| a.1.id().cmp(b.1.id()));
    Ok(found)
}

fn list(dir: &Path) -> Result<(), String> {
    let found = modules(dir)?;
    if found.is_empty() {
        return Err(format!("no modules found in {}", dir.display()));
    }
    println!("{} module(s) in {}", found.len(), dir.display());
    for (_, manifest) in found {
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

fn install(dir: &Path, id: Option<&str>, all: bool, yes: bool) -> Result<(), String> {
    let found = modules(dir)?;
    let selected: Vec<ModuleEntry> = match (id, all) {
        (Some(id), false) => {
            let entry = found
                .into_iter()
                .find(|(_, manifest)| manifest.id() == id)
                .ok_or_else(|| format!("no module `{id}` in {}", dir.display()))?;
            vec![entry]
        }
        (None, true) => found,
        (Some(_), true) => {
            return Err("pass either a module id or --all, not both".to_owned());
        }
        (None, false) => {
            return Err("name a module id, or pass --all".to_owned());
        }
    };
    if !yes {
        confirm_install(dir, &selected)?;
    }
    let registry = PluginRegistry::from_default_dir();
    let mut failures: Vec<String> = Vec::new();
    for (path, manifest) in &selected {
        let id = manifest.id();
        match registry.install(path) {
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

fn confirm_install(dir: &Path, selected: &[ModuleEntry]) -> Result<(), String> {
    use std::io::Write;
    let mut question = format!(
        "install {} module(s) from {}? [y/N] ",
        selected.len(),
        dir.display()
    );
    for (_, manifest) in selected {
        let points = manifest
            .components
            .as_ref()
            .and_then(|components| components.get("hooks"))
            .map(|points| points.join(", "))
            .unwrap_or_else(|| "no hooks".to_owned());
        let permissions = if manifest.permissions.is_empty() {
            "no permissions".to_owned()
        } else {
            manifest.permissions.join(", ")
        };
        question.push_str(&format!(
            "\n  {} — {} — permissions: {}",
            manifest.id(),
            points,
            permissions
        ));
    }
    question.push('\n');
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_module(dir: &Path, name: &str, id: &str) {
        let module = dir.join(name);
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(
            module.join("plugin.json"),
            format!(r#"{{"id": "{id}", "name": "{name}", "version": "1.0.0"}}"#),
        )
        .unwrap();
    }

    /// A manifest id must never steer the install source outside the catalog: the copy uses the
    /// directory the manifest was discovered in, and the registry refuses an unusable id anyway.
    #[test]
    fn install_ignores_a_traversal_manifest_id() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        write_module(&catalog, "evil", "../escaped");
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let err = install(&catalog, Some("../escaped"), false, true).unwrap_err();
        assert!(err.contains("not a usable plugin id"), "{err}");
        assert!(!tmp.path().join("escaped").exists());
    }

    /// Without `--yes` a non-terminal stdin refuses before anything is copied. On an interactive
    /// terminal the prompt would block on input, so the assertion only runs headless.
    #[test]
    fn install_without_yes_prompts_first() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        write_module(&catalog, "good-mod", "good-mod");
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let err = install(&catalog, Some("good-mod"), false, false).unwrap_err();
        assert!(err.contains("--yes"), "{err}");
        assert!(
            !vikram_runtime::config::plugins_dir()
                .join("good-mod")
                .exists()
        );
    }
}
