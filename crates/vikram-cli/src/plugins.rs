//! `vikram plugins` — install, inspect, and toggle plugins under `~/.vikram/plugins`.
//!
//! Everything here is local filesystem work on the same `PluginRegistry` the runtime scans: no
//! network, no marketplace fetch. The source registry at `~/.vikram/plugin-registry` is listed by
//! `plugins registry` and searched for dependencies at install time.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use vikram_runtime::plugins::{PluginManifest, PluginRegistry, compare_versions};

#[derive(Debug, Subcommand)]
pub enum PluginsAction {
    /// List installed plugins (default).
    List,
    /// Show one plugin's manifest in full.
    Show { id: String },
    /// Validate a plugin directory before installing it.
    Validate { path: PathBuf },
    /// Install a plugin from a local directory or a `.tar.gz` from `vikram module pack`.
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
    /// Reinstall an installed plugin from a local source, keeping the old copy for rollback.
    Update {
        /// Plugin id to update.
        id: Option<String>,
        /// Update every installed plugin that has a recorded source or a copy in the local
        /// source registry.
        #[arg(long)]
        all: bool,
        /// Source plugin directory or `.tar.gz`; without it the recorded install source is
        /// used, then the local source registry.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Restore the version an update replaced.
    Rollback {
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
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
        PluginsAction::Update { id, all, from, yes } => {
            update(&registry, id.as_deref(), all, from.as_deref(), yes)
        }
        PluginsAction::Rollback { id, yes } => {
            if !yes {
                confirm(&format!(
                    "roll back plugin `{id}` to its previous version? [y/N] "
                ))?;
            }
            let version = registry.rollback(&id)?;
            if version.trim().is_empty() {
                println!("rolled back {id}");
            } else {
                println!("rolled back {id} to {version}");
            }
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
    // A `.tar.gz` from `vikram module pack` extracts to a temp dir and then takes the exact
    // same directory path; the temp dir is removed when `extracted` drops.
    if crate::archive::looks_like_archive(path) {
        let extracted = crate::archive::extract_module_dir(path)?;
        return install_dir(registry, extracted.path(), yes, path);
    }
    install_dir(registry, path, yes, path)
}

/// Where each installed plugin came from, so `plugins update` can reinstall without
/// `--from`. The runtime registry keeps no source path, so the CLI records one per plugin
/// in `$VIKRAM_HOME/plugin-sources.json`: plugin id -> the directory or `.tar.gz` path
/// `plugins install` was given.
fn sources_path() -> PathBuf {
    vikram_runtime::config::vikram_home().join("plugin-sources.json")
}

fn load_sources() -> Result<BTreeMap<String, String>, String> {
    let path = sources_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => return Err(format!("could not read {}: {err}", path.display())),
    };
    serde_json::from_str(&raw).map_err(|err| format!("could not parse {}: {err}", path.display()))
}

fn save_sources(sources: &BTreeMap<String, String>) -> Result<(), String> {
    let path = sources_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(sources).expect("plugin sources are serializable");
    std::fs::write(&path, format!("{body}\n"))
        .map_err(|err| format!("could not write {}: {err}", path.display()))
}

/// Record the canonical source path so a later `plugins update` finds it again. Relative
/// paths are made absolute because the update may run from another directory.
fn record_source(id: &str, source: &Path) -> Result<(), String> {
    let mut sources = load_sources()?;
    let canonical = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    sources.insert(id.to_string(), canonical.to_string_lossy().into_owned());
    save_sources(&sources)
}

fn forget_source(id: &str) -> Result<(), String> {
    let mut sources = load_sources()?;
    if sources.remove(id).is_some() {
        save_sources(&sources)?;
    }
    Ok(())
}

/// Resolve a recorded source path to a plugin directory. An archive is extracted fresh —
/// the temp dir is parked in `extractions` so it outlives the update that reads it.
fn recorded_source(
    recorded: &Path,
    id: &str,
    extractions: &mut Vec<crate::archive::ExtractedModule>,
) -> Result<PathBuf, String> {
    if crate::archive::looks_like_archive(recorded) {
        let extracted = crate::archive::extract_module_dir(recorded)?;
        let root = extracted.path().to_path_buf();
        extractions.push(extracted);
        return Ok(root);
    }
    if recorded.join("plugin.json").is_file() {
        return Ok(recorded.to_path_buf());
    }
    if recorded.exists() {
        return Err(format!(
            "recorded source {} for `{id}` is not a plugin directory; pass --from <dir>",
            recorded.display()
        ));
    }
    Err(format!(
        "recorded source {} for `{id}` no longer exists; pass --from <dir> to update from a local copy",
        recorded.display()
    ))
}

fn install_dir(
    registry: &PluginRegistry,
    path: &Path,
    yes: bool,
    shown: &Path,
) -> Result<(), String> {
    let manifest =
        PluginManifest::from_dir(path).map_err(|err| format!("not a plugin directory: {err}"))?;
    let id = manifest.id();
    if !yes {
        confirm(&format!(
            "install plugin `{id}` from {}? [y/N] ",
            shown.display()
        ))?;
    }
    let installed = registry.install(path)?;
    record_source(id, shown)?;
    println!("installed {installed}");
    Ok(())
}

/// Reinstall plugins from a local source. `plugins install` records where each plugin came
/// from in `plugin-sources.json`, so without `--from` the recorded source is tried first and
/// the local registry at `~/.vikram/plugin-registry/<id>` second; a plugin with neither is
/// reported and skipped rather than silently treated as up to date.
fn update(
    registry: &PluginRegistry,
    id: Option<&str>,
    all: bool,
    from: Option<&Path>,
    yes: bool,
) -> Result<(), String> {
    if all && (id.is_some() || from.is_some()) {
        return Err("pass either a plugin id (with optional --from) or --all, not both".to_owned());
    }
    if !all && id.is_none() {
        return Err("name a plugin id, or pass --all".to_owned());
    }
    let sources = load_sources()?;
    let mut extractions: Vec<crate::archive::ExtractedModule> = Vec::new();
    let mut targets: Vec<(String, PathBuf)> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    if let Some(id) = id {
        let source = match from {
            Some(dir) => {
                let direct = dir.to_path_buf();
                if direct.join("plugin.json").is_file() {
                    direct
                } else {
                    let nested = dir.join(id);
                    if nested.join("plugin.json").is_file() {
                        nested
                    } else {
                        direct
                    }
                }
            }
            None => match sources.get(id) {
                Some(recorded) => recorded_source(Path::new(recorded), id, &mut extractions)?,
                None => {
                    let candidate = registry.registry_root().join(id);
                    if candidate.join("plugin.json").is_file() {
                        candidate
                    } else {
                        return Err(format!(
                            "no recorded source for `{id}` and {} has no `{id}` folder; \
                             pass --from <dir> to update from a local copy",
                            registry.registry_root().display()
                        ));
                    }
                }
            },
        };
        targets.push((id.to_owned(), source));
    } else {
        for plugin in registry.plugins() {
            if !plugin.installed {
                continue;
            }
            let name = plugin
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| plugin.name.clone());
            let id = plugin.id();
            if let Some(recorded) = sources.get(&id).or_else(|| sources.get(&name)) {
                match recorded_source(Path::new(recorded), &id, &mut extractions) {
                    Ok(source) => {
                        targets.push((id, source));
                        continue;
                    }
                    Err(reason) => {
                        missing.push(format!("{name}: {reason}"));
                        continue;
                    }
                }
            }
            let candidate = registry.registry_root().join(&name);
            if candidate.join("plugin.json").is_file() {
                targets.push((name, candidate));
            } else {
                missing.push(format!(
                    "{name}: no copy in {}",
                    registry.registry_root().display()
                ));
            }
        }
    }
    for entry in &missing {
        println!("skipped {entry}");
    }
    if targets.is_empty() {
        if missing.is_empty() {
            println!("nothing to update");
        }
        return Ok(());
    }
    if !yes {
        let list = targets
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        confirm(&format!("update plugin(s) {list}? [y/N] "))?;
    }
    let mut failures: Vec<String> = Vec::new();
    for (id, source) in &targets {
        let before = registry.installed_version(id).unwrap_or_default();
        match registry.update(source, id) {
            Ok(updated) => {
                let after = PluginManifest::from_dir(source)
                    .map(|manifest| manifest.version)
                    .unwrap_or_default();
                let direction = match compare_versions(&before, &after) {
                    Some(std::cmp::Ordering::Greater) => " (downgrade)",
                    _ => "",
                };
                if before.is_empty() {
                    println!("updated {updated}: {after}{direction}");
                } else {
                    println!("updated {updated}: {before} -> {after}{direction}");
                }
            }
            Err(error) => failures.push(format!("{id}: {error}")),
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "{} updated, {} not: {}",
            targets.len() - failures.len(),
            failures.len(),
            failures.join("; ")
        ));
    }
    println!("{} plugin(s) updated", targets.len());
    Ok(())
}

fn uninstall(registry: &PluginRegistry, id: &str, yes: bool) -> Result<(), String> {
    if !yes {
        confirm(&format!("uninstall plugin `{id}`? [y/N] "))?;
    }
    registry.uninstall(id)?;
    forget_source(id)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_plugin(dir: &Path, name: &str, version: &str) {
        let plugin = dir.join(name);
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("plugin.json"),
            format!(r#"{{"id": "{name}", "name": "{name}", "version": "{version}"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn update_requires_an_id_or_all() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(tmp.path());
        assert!(
            update(&registry, None, false, None, true)
                .unwrap_err()
                .contains("--all")
        );
        let source = tmp.path().join("src");
        assert!(
            update(&registry, Some("acme"), true, Some(&source), true)
                .unwrap_err()
                .contains("not both")
        );
    }

    /// Without `--from` the only source is the local registry; an installed plugin with no
    /// copy there is refused honestly instead of reported as updated.
    #[test]
    fn update_without_a_recorded_source_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let installed = tmp.path().join("plugins");
        write_plugin(&installed, "acme", "1.0.0");
        let registry = PluginRegistry::scan(&installed);
        let err = update(&registry, Some("acme"), false, None, true).unwrap_err();
        assert!(err.contains("no recorded source"), "{err}");
        assert!(err.contains("--from"), "{err}");
    }

    #[test]
    fn update_from_a_source_directory_swaps_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let installed = tmp.path().join("plugins");
        write_plugin(&installed, "acme", "1.0.0");
        let source = tmp.path().join("src").join("acme");
        write_plugin(&tmp.path().join("src"), "acme", "2.0.0");
        let registry = PluginRegistry::scan(&installed);
        update(&registry, Some("acme"), false, Some(source.as_path()), true).unwrap();
        let manifest = PluginManifest::from_dir(&installed.join("acme")).unwrap();
        assert_eq!(manifest.version, "2.0.0");
        // The replaced copy is parked for `plugins rollback`.
        assert!(installed.join(".backups/acme/plugin.json").is_file());
    }

    #[test]
    fn update_reports_an_unknown_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(tmp.path());
        let err = update(&registry, Some("ghost"), false, None, true).unwrap_err();
        assert!(err.contains("no recorded source"), "{err}");
    }

    fn tar_dir(dir: &Path, out: &Path) {
        let name = dir.file_name().unwrap();
        let parent = dir.parent().unwrap();
        let status = std::process::Command::new("tar")
            .args(["-czf"])
            .arg(out)
            .arg("-C")
            .arg(parent)
            .arg(name)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// `plugins install module.tar.gz` extracts and then runs the unchanged directory path.
    #[test]
    fn install_accepts_a_packed_archive() {
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
        let source = tmp.path().join("src");
        write_plugin(&source, "acme", "1.0.0");
        let archive = tmp.path().join("acme.tar.gz");
        tar_dir(&source.join("acme"), &archive);
        let installed = tmp.path().join("installed");
        let registry = PluginRegistry::scan(&installed);
        install(&registry, &archive, true).unwrap();
        assert!(installed.join("acme/plugin.json").is_file());
    }

    /// `install` records the source path; a later `update` without `--from` finds it.
    #[test]
    fn install_records_the_source_and_update_uses_it() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let src = tmp.path().join("src");
        write_plugin(&src, "acme", "1.0.0");
        let installed = tmp.path().join("plugins");
        let registry = PluginRegistry::scan(&installed);
        install(&registry, &src.join("acme"), true).unwrap();
        let sources = load_sources().unwrap();
        assert_eq!(
            sources.get("acme").map(String::as_str),
            Some(src.join("acme").canonicalize().unwrap().to_str().unwrap())
        );

        write_plugin(&src, "acme", "2.0.0");
        let registry = PluginRegistry::scan(&installed);
        update(&registry, Some("acme"), false, None, true).unwrap();
        let manifest = PluginManifest::from_dir(&installed.join("acme")).unwrap();
        assert_eq!(manifest.version, "2.0.0");
    }

    /// A recorded source that vanished is an honest error naming `--from`, not a silent skip.
    #[test]
    fn update_with_a_deleted_recorded_source_fails_honestly() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let src = tmp.path().join("src");
        write_plugin(&src, "acme", "1.0.0");
        let installed = tmp.path().join("plugins");
        let registry = PluginRegistry::scan(&installed);
        install(&registry, &src.join("acme"), true).unwrap();
        std::fs::remove_dir_all(&src).unwrap();

        let registry = PluginRegistry::scan(&installed);
        let err = update(&registry, Some("acme"), false, None, true).unwrap_err();
        assert!(err.contains("no longer exists"), "{err}");
        assert!(err.contains("--from"), "{err}");
    }

    /// An archive install records the `.tar.gz` path; `update` re-extracts it.
    #[test]
    fn update_reextracts_a_recorded_archive_source() {
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
        let src = tmp.path().join("src");
        write_plugin(&src, "acme", "1.0.0");
        let archive = tmp.path().join("acme.tar.gz");
        tar_dir(&src.join("acme"), &archive);
        let installed = tmp.path().join("plugins");
        let registry = PluginRegistry::scan(&installed);
        install(&registry, &archive, true).unwrap();
        assert!(load_sources().unwrap().contains_key("acme"));

        write_plugin(&src, "acme", "2.0.0");
        tar_dir(&src.join("acme"), &archive);
        let registry = PluginRegistry::scan(&installed);
        update(&registry, Some("acme"), false, None, true).unwrap();
        let manifest = PluginManifest::from_dir(&installed.join("acme")).unwrap();
        assert_eq!(manifest.version, "2.0.0");
    }

    #[test]
    fn uninstall_forgets_the_recorded_source() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let src = tmp.path().join("src");
        write_plugin(&src, "acme", "1.0.0");
        let installed = tmp.path().join("plugins");
        let registry = PluginRegistry::scan(&installed);
        install(&registry, &src.join("acme"), true).unwrap();
        uninstall(&registry, "acme", true).unwrap();
        assert!(!load_sources().unwrap().contains_key("acme"));
    }

    #[test]
    fn install_refuses_a_corrupt_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("bad.tar.gz");
        std::fs::write(&archive, b"not a tarball").unwrap();
        let registry = PluginRegistry::scan(&tmp.path().join("installed"));
        let err = install(&registry, &archive, true).unwrap_err();
        assert!(err.contains("cannot extract"), "{err}");
    }

    /// Validation is not skipped for archives: a tarball whose contents are not a plugin
    /// is refused exactly like the directory would be.
    #[test]
    fn install_validates_the_extracted_directory() {
        if std::process::Command::new("tar")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("not-a-plugin");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), "hello").unwrap();
        let archive = tmp.path().join("module.tar.gz");
        tar_dir(&source, &archive);
        let registry = PluginRegistry::scan(&tmp.path().join("installed"));
        let err = install(&registry, &archive, true).unwrap_err();
        assert!(err.contains("not a plugin directory"), "{err}");
    }
}
