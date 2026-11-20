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
    /// Install one module, or all of them. The id may also name a `.tar.gz` archive.
    Install {
        /// Module id to install, or a path to a `.tar.gz` archive.
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
    /// Reinstall installed modules from the catalog, keeping the old copy for rollback.
    Update {
        /// Module id to update.
        id: Option<String>,
        /// Update every installed module found in the catalog.
        #[arg(long)]
        all: bool,
        /// Catalog directory (defaults to $VIKRAM_MODULES_DIR, then ./modules).
        #[arg(long)]
        from: Option<PathBuf>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Show the hooks the installed plugins contribute, in execution order.
    Hooks,
}

pub fn run(action: ModulesAction) -> Result<(), String> {
    match action {
        ModulesAction::List { from } => list(&catalog_dir(from)),
        ModulesAction::Install { id, all, yes, from } => {
            // `id` doubles as an archive path: `vikram modules install module.tar.gz`.
            if let Some(id) = id.as_deref()
                && !all
                && crate::archive::looks_like_archive(Path::new(id))
            {
                return install_archive(Path::new(id), yes);
            }
            install(&catalog_dir(from), id.as_deref(), all, yes)
        }
        ModulesAction::Update { id, all, from, yes } => {
            update(&catalog_dir(from), id.as_deref(), all, yes)
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
    install_selected(dir, selected, yes)
}

/// Install already-selected catalog entries through the same registry path a directory
/// install uses.
fn install_selected(dir: &Path, selected: Vec<ModuleEntry>, yes: bool) -> Result<(), String> {
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

/// `vikram modules install <file.tar.gz>`: the archive holds one module directory (or a
/// directory of them, like a catalog); each plugin inside installs through the same
/// registry path. The extracted temp dir is removed when `extracted` drops.
fn install_archive(archive: &Path, yes: bool) -> Result<(), String> {
    let extracted = crate::archive::extract_module_dir(archive)?;
    let root = extracted.path();
    let selected = if root.join("plugin.json").is_file() {
        let manifest =
            PluginManifest::from_dir(root).map_err(|e| format!("{}: {e}", root.display()))?;
        vec![(root.to_path_buf(), manifest)]
    } else {
        let found = modules(root)?;
        if found.is_empty() {
            return Err(format!(
                "{} contains no plugin module (no plugin.json found)",
                archive.display()
            ));
        }
        found
    };
    install_selected(archive, selected, yes)
}

/// Reinstall modules that are already installed; the catalog is the only source, so catalog
/// entries that were never installed are not touched.
fn update(dir: &Path, id: Option<&str>, all: bool, yes: bool) -> Result<(), String> {
    if all && id.is_some() {
        return Err("pass either a module id or --all, not both".to_owned());
    }
    if !all && id.is_none() {
        return Err("name a module id, or pass --all".to_owned());
    }
    let found = modules(dir)?;
    let registry = PluginRegistry::from_default_dir();
    let selected: Vec<ModuleEntry> = match (id, all) {
        (Some(id), false) => {
            if registry.installed_version(id).is_none() {
                return Err(format!("module `{id}` is not installed"));
            }
            let entry = found
                .into_iter()
                .find(|(_, manifest)| manifest.id() == id)
                .ok_or_else(|| format!("no module `{id}` in {}", dir.display()))?;
            vec![entry]
        }
        (None, true) => found
            .into_iter()
            .filter(|(_, manifest)| registry.installed_version(manifest.id()).is_some())
            .collect(),
        _ => unreachable!(),
    };
    if selected.is_empty() {
        println!("no installed modules found in {}", dir.display());
        return Ok(());
    }
    if !yes {
        confirm_install(dir, &selected)?;
    }
    let mut failures: Vec<String> = Vec::new();
    for (path, manifest) in &selected {
        let id = manifest.id();
        let before = registry.installed_version(id).unwrap_or_default();
        match registry.update(path, id) {
            Ok(_) => {
                if before.is_empty() || before == manifest.version {
                    println!("updated {id}");
                } else {
                    println!("updated {id}: {before} -> {}", manifest.version);
                }
            }
            Err(error) => failures.push(format!("{id}: {error}")),
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "{} updated, {} not: {}",
            selected.len() - failures.len(),
            failures.len(),
            failures.join("; ")
        ));
    }
    println!("{} module(s) updated", selected.len());
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

    /// `modules install module.tar.gz` extracts and installs through the same registry path.
    #[test]
    fn install_archive_extracts_and_installs() {
        if std::process::Command::new("tar")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let source = tmp.path().join("packed");
        write_module(&source, "packed-mod", "packed-mod");
        let archive = tmp.path().join("packed.tar.gz");
        let status = std::process::Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&source)
            .arg("packed-mod")
            .status()
            .unwrap();
        assert!(status.success());
        install_archive(&archive, true).unwrap();
        assert!(
            vikram_runtime::config::plugins_dir()
                .join("packed-mod")
                .join("plugin.json")
                .is_file()
        );
    }

    #[test]
    fn install_archive_refuses_contents_without_a_plugin() {
        if std::process::Command::new("tar")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let source = tmp.path().join("empty-mod");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), "hi").unwrap();
        let archive = tmp.path().join("empty.tar.gz");
        let status = std::process::Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(tmp.path())
            .arg("empty-mod")
            .status()
            .unwrap();
        assert!(status.success());
        let err = install_archive(&archive, true).unwrap_err();
        assert!(err.contains("no plugin module"), "{err}");
    }
}
