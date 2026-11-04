use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

impl PluginRegistry {
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
    pub(crate) fn plugin_dir(&self, id: &str) -> Option<PathBuf> {
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

use crate::plugins::*;
