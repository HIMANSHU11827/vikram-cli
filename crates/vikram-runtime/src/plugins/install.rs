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
        if let Some(entry) = manifest.entry_point()
            && !source.join(entry).exists()
        {
            return Err(format!(
                "declared entry point `{entry}` is missing from {}",
                source.display()
            ));
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
}

use crate::plugins::*;
