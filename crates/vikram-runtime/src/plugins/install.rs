use std::path::{Path, PathBuf};

impl PluginRegistry {
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
        if dir.join("plugin.json").is_file()
            && let Ok(manifest) = PluginManifest::from_dir(&dir)
        {
            return Some(manifest.version);
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
            let resolved = entry_path(source, entry).map_err(|e| format!("plugin `{id}`: {e}"))?;
            if !resolved.exists() {
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

    /// Reinstall the installed plugin `id` from `source`, keeping the replaced copy for
    /// [`Self::rollback`].
    ///
    /// The source manifest must pass the same checks [`Self::install`] applies and must declare
    /// the id the plugin is installed under. Declared dependencies are resolved first from the
    /// local sources, exactly as at install time. The current directory is moved to
    /// `<root>/.backups/<id>` before the new copy lands; if the copy fails the previous install
    /// is moved back. As with install there is no cross-plugin rollback: a dependency this
    /// update pulled in stays installed.
    pub fn update(&self, source: &Path, id: &str) -> Result<String, String> {
        let manifest = self.candidate_manifest(source)?;
        let new_id = manifest.id().trim().to_owned();
        let dir = self
            .plugin_dir(id)
            .ok_or_else(|| format!("plugin not found: {id}"))?;
        let installed_id = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if new_id != installed_id {
            return Err(format!(
                "source is plugin `{new_id}` but `{id}` is installed as `{installed_id}`"
            ));
        }
        if manifest
            .dependencies
            .iter()
            .any(|dep| dep.name.trim() == new_id)
        {
            return Err(format!(
                "dependency cycle detected: {}",
                quote_chain(&[new_id.clone(), new_id.clone()])
            ));
        }
        let mut chain = vec![new_id.clone()];
        self.install_dependencies_of(source, &manifest, &new_id, &mut chain)?;
        warn_active_transforms(&dir, &new_id);
        let backup = self.backup_dir(&installed_id);
        if backup.exists() {
            std::fs::remove_dir_all(&backup)
                .map_err(|e| format!("cannot replace backup {}: {e}", backup.display()))?;
        }
        std::fs::create_dir_all(self.root().join(".backups"))
            .map_err(|e| format!("cannot create {}: {e}", self.root().display()))?;
        std::fs::rename(&dir, &backup)
            .map_err(|e| format!("cannot move {} aside for update: {e}", dir.display()))?;
        match copy_dir(source, &dir) {
            Ok(()) => Ok(new_id),
            Err(err) => {
                let _ = std::fs::remove_dir_all(&dir);
                if std::fs::rename(&backup, &dir).is_ok() {
                    Err(format!("cannot install {}: {err}", source.display()))
                } else {
                    Err(format!(
                        "cannot install {}: {err}; the previous version is preserved at {}",
                        source.display(),
                        backup.display()
                    ))
                }
            }
        }
    }

    /// Restore the copy [`Self::update`] parked under `.backups`, replacing the current install.
    ///
    /// The parked copy is kept, so rollback is repeatable; the next [`Self::update`] overwrites
    /// it. `Err` when the plugin is unknown or no backup exists — a plugin that was only ever
    /// installed has nothing to roll back to. Returns the restored manifest's version.
    pub fn rollback(&self, id: &str) -> Result<String, String> {
        let dir = self
            .plugin_dir(id)
            .ok_or_else(|| format!("plugin not found: {id}"))?;
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let backup = self.backup_dir(&name);
        if !backup.join("plugin.json").is_file() {
            return Err(format!("no previous version recorded for `{id}`"));
        }
        warn_active_transforms(&dir, &name);
        let aside = self.backup_dir(&format!(".{name}.pre-rollback"));
        if aside.exists() {
            std::fs::remove_dir_all(&aside)
                .map_err(|e| format!("cannot clear {}: {e}", aside.display()))?;
        }
        std::fs::rename(&dir, &aside)
            .map_err(|e| format!("cannot move {} aside for rollback: {e}", dir.display()))?;
        match copy_dir(&backup, &dir) {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(&aside);
                PluginManifest::from_dir(&dir)
                    .map(|manifest| manifest.version)
                    .map_err(|e| {
                        format!(
                            "restored {} but cannot read its manifest: {e}",
                            dir.display()
                        )
                    })
            }
            Err(err) => {
                let _ = std::fs::remove_dir_all(&dir);
                if std::fs::rename(&aside, &dir).is_ok() {
                    Err(format!("cannot restore {}: {err}", backup.display()))
                } else {
                    Err(format!(
                        "cannot restore {}: {err}; the current version is preserved at {}",
                        backup.display(),
                        aside.display()
                    ))
                }
            }
        }
    }

    /// Where [`Self::update`] parks the replaced plugin directory for [`Self::rollback`].
    ///
    /// `.backups` sits one level below the plugins root and holds no `plugin.json` of its own,
    /// so the one-level-deep scanner never lists it as a plugin.
    fn backup_dir(&self, id: &str) -> PathBuf {
        self.root().join(".backups").join(id)
    }

    /// Remove an installed plugin's directory.
    pub fn uninstall(&self, id: &str) -> Result<(), String> {
        let dir = self
            .plugin_dir(id)
            .ok_or_else(|| format!("plugin not found: {id}"))?;
        warn_active_transforms(&dir, id);
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
        write_atomic(&manifest_path, &body)?;
        if !enabled {
            warn_active_transforms(&dir, id);
        }
        Ok(())
    }
}

/// Warn on stderr when the plugin has applied file transforms recorded in `transforms.json`.
///
/// The state file is owned by the CLI's transform manager; this is a read-only peek so disabling
/// or uninstalling a plugin never silently orphans files it wrote under the managed roots. The
/// manifest id is preferred over the directory name because that is the key transforms.json uses.
fn warn_active_transforms(dir: &Path, fallback_id: &str) {
    let id = PluginManifest::from_dir(dir)
        .map(|manifest| manifest.id().to_owned())
        .unwrap_or_else(|_| fallback_id.to_owned());
    let path = crate::config::vikram_home().join("transforms.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(state) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let applied = state
        .get("plugins")
        .and_then(|plugins| plugins.get(&id))
        .and_then(|record| record.get("ops"))
        .and_then(|ops| ops.as_array())
        .map(|ops| !ops.is_empty())
        .unwrap_or(false);
    if applied {
        eprintln!(
            "warning: plugin `{id}` has applied file transforms; \
             run `vikram transforms rollback {id}` to revert them"
        );
    }
}

use crate::plugins::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn plugin_source(root: &Path, name: &str, version: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            format!(r#"{{"id":"{name}","name":"{name}","version":"{version}","entry":"main.sh"}}"#),
        )
        .unwrap();
        std::fs::write(dir.join("main.sh"), "#!/bin/sh\necho hi\n").unwrap();
    }

    fn installed_version(dir: &Path, id: &str) -> String {
        PluginManifest::from_dir(&dir.join(id)).unwrap().version
    }

    #[test]
    fn update_swaps_the_plugin_and_rollback_restores_the_previous_version() {
        let source = tempfile::tempdir().unwrap();
        plugin_source(source.path(), "acme", "1.0.0");
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());
        registry.install(&source.path().join("acme")).unwrap();
        assert_eq!(installed_version(installed.path(), "acme"), "1.0.0");

        // A second source revision with an extra file.
        plugin_source(source.path(), "acme", "2.0.0");
        std::fs::write(source.path().join("acme/new.txt"), "new").unwrap();
        let updated = registry
            .update(&source.path().join("acme"), "acme")
            .expect("update works");
        assert_eq!(updated, "acme");
        assert_eq!(installed_version(installed.path(), "acme"), "2.0.0");
        assert!(installed.path().join("acme/new.txt").is_file());
        // The parked copy still holds the old manifest, and the scanner ignores `.backups`.
        assert_eq!(
            installed_version(&installed.path().join(".backups"), "acme"),
            "1.0.0"
        );
        assert_eq!(PluginRegistry::scan(installed.path()).installed(), 1);

        let restored = PluginRegistry::scan(installed.path());
        let version = restored.rollback("acme").expect("rollback works");
        assert_eq!(version, "1.0.0");
        assert_eq!(installed_version(installed.path(), "acme"), "1.0.0");
        assert!(!installed.path().join("acme/new.txt").exists());
        // The backup is kept, so rollback stays repeatable.
        assert_eq!(restored.rollback("acme").unwrap(), "1.0.0");
    }

    #[test]
    fn rollback_without_a_previous_version_is_refused() {
        let source = tempfile::tempdir().unwrap();
        plugin_source(source.path(), "acme", "1.0.0");
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());
        registry.install(&source.path().join("acme")).unwrap();
        let err = registry.rollback("acme").unwrap_err();
        assert!(err.contains("no previous version"), "{err}");
        assert_eq!(installed_version(installed.path(), "acme"), "1.0.0");
    }

    #[test]
    fn update_and_rollback_report_unknown_plugins() {
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());
        let source = tempfile::tempdir().unwrap();
        plugin_source(source.path(), "acme", "1.0.0");
        assert!(
            registry
                .update(&source.path().join("acme"), "ghost")
                .unwrap_err()
                .contains("plugin not found")
        );
        assert!(
            registry
                .rollback("ghost")
                .unwrap_err()
                .contains("plugin not found")
        );
    }

    #[test]
    fn update_rejects_a_source_declaring_a_different_id() {
        let source = tempfile::tempdir().unwrap();
        plugin_source(source.path(), "acme", "1.0.0");
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());
        registry.install(&source.path().join("acme")).unwrap();
        plugin_source(source.path(), "other", "9.9.9");
        let err = registry
            .update(&source.path().join("other"), "acme")
            .unwrap_err();
        assert!(err.contains("installed as `acme`"), "{err}");
        assert_eq!(installed_version(installed.path(), "acme"), "1.0.0");
    }

    #[test]
    fn a_failed_update_restores_the_previous_install() {
        let source = tempfile::tempdir().unwrap();
        plugin_source(source.path(), "acme", "1.0.0");
        let installed = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::scan(installed.path());
        registry.install(&source.path().join("acme")).unwrap();
        // v2 declares an entry point that does not exist, so the candidate check fails
        // before anything on disk is touched.
        std::fs::write(
            source.path().join("acme/plugin.json"),
            r#"{"id":"acme","name":"acme","version":"2.0.0","entry":"missing.sh"}"#,
        )
        .unwrap();
        let err = registry
            .update(&source.path().join("acme"), "acme")
            .unwrap_err();
        assert!(err.contains("entry point"), "{err}");
        assert_eq!(installed_version(installed.path(), "acme"), "1.0.0");
    }
}
