//! Bundles: named groups of skills that belong together.
//!
//! A bundle is a file, never a copy. It lists members; installing the bundle installs the
//! members by name, so a bundle cannot drift out of sync with the skills it names.
//!
//! Membership is strict on purpose: `skills:` must exist and must be non-empty. An "empty
//! bundle" is always a mistake — a typo in the key, a truncated file — and accepting it would
//! make the command appear to succeed while activating nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::discovery::{Catalog, Diagnostic, DiagnosticLevel, DiscoveryOptions};
use crate::error::{Result, SkillError};
use crate::frontmatter::{self, Yaml};
use crate::paths;
use crate::skill::SkillId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    pub name: String,
    pub description: String,
    pub skills: Vec<SkillId>,
    pub optional: Vec<SkillId>,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct BundleIndex {
    pub bundles: BTreeMap<String, Bundle>,
    pub diagnostics: Vec<Diagnostic>,
}

impl BundleIndex {
    /// `<home>/bundles/*.{yaml,yml,json}` under the real home.
    pub fn load() -> BundleIndex {
        BundleIndex::load_in(&paths::bundles_dir())
    }

    /// A missing directory is an empty index, not an error: bundles are optional.
    pub fn load_in(dir: &Path) -> BundleIndex {
        let mut index = BundleIndex::default();
        if !dir.is_dir() {
            return index;
        }
        let entries = match paths::sorted_entries(dir) {
            Ok(entries) => entries,
            Err(err) => {
                index.diagnostics.push(Diagnostic::new(
                    DiagnosticLevel::Error,
                    dir,
                    err.to_string(),
                ));
                return index;
            }
        };
        for path in entries {
            if !path.is_file() || !is_bundle_file(&path) {
                continue;
            }
            match load_one(&path) {
                Ok(bundle) => {
                    if index.bundles.contains_key(&bundle.name) {
                        index.diagnostics.push(Diagnostic::new(
                            DiagnosticLevel::Warning,
                            &path,
                            format!(
                                "bundle `{}` is defined more than once; keeping the first definition",
                                bundle.name
                            ),
                        ));
                    } else {
                        index.bundles.insert(bundle.name.clone(), bundle);
                    }
                }
                Err(err) => index.diagnostics.push(Diagnostic::new(
                    DiagnosticLevel::Error,
                    &path,
                    err.to_string(),
                )),
            }
        }
        index
    }

    /// The members of `name` that can be activated right now: required ones must exist, optional
    /// ones are included only when they do.
    ///
    /// Discovery runs against the real environment here. Tests and callers that already hold a
    /// catalog should use [`BundleIndex::resolve_in`].
    pub fn resolve(&self, name: &str) -> Result<Vec<SkillId>> {
        let catalog = Catalog::discover(&DiscoveryOptions::default());
        self.resolve_in(name, &catalog)
    }

    pub fn resolve_in(&self, name: &str, catalog: &Catalog) -> Result<Vec<SkillId>> {
        let bundle = self.bundles.get(name).ok_or_else(|| SkillError::NotFound {
            name: name.to_string(),
            sources: self.bundles.len(),
        })?;
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for member in &bundle.skills {
            match catalog.resolve(member.as_str()) {
                None => {
                    return Err(SkillError::MissingDependency {
                        owner: bundle.name.clone(),
                        name: member.as_str().to_string(),
                    });
                }
                Some(summary) if !summary.enabled => {
                    return Err(SkillError::Disabled {
                        name: member.as_str().to_string(),
                    });
                }
                Some(_) => {}
            }
            if seen.insert(member.clone()) {
                out.push(member.clone());
            }
        }
        for member in &bundle.optional {
            if seen.contains(member) {
                continue;
            }
            if catalog.resolve(member.as_str()).is_some() && seen.insert(member.clone()) {
                out.push(member.clone());
            }
        }
        Ok(out)
    }
}

fn is_bundle_file(path: &Path) -> bool {
    path.extension()
        .map(|ext| {
            let ext = ext.to_string_lossy().to_ascii_lowercase();
            matches!(ext.as_str(), "yaml" | "yml" | "json")
        })
        .unwrap_or(false)
}

fn load_one(path: &Path) -> Result<Bundle> {
    let text = fs::read_to_string(path).map_err(|err| SkillError::io(path, &err))?;
    let is_json = path
        .extension()
        .map(|ext| ext.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if is_json {
        from_json(path, &text)
    } else {
        from_yaml(path, &text)
    }
}

fn from_yaml(path: &Path, text: &str) -> Result<Bundle> {
    let doc = frontmatter::parse_document(text, path)?;
    let map = match doc {
        Yaml::Map(entries) => entries,
        Yaml::Null => {
            return Err(SkillError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "the file is empty".to_string(),
            });
        }
        _ => {
            return Err(SkillError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "a bundle must be a mapping with a `skills` list".to_string(),
            });
        }
    };
    let get = |key: &str| {
        map.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    };
    let skills = match get("skills") {
        Some(value) => value.strings(),
        None => {
            return Err(SkillError::InvalidBundle {
                path: path.to_path_buf(),
                reason: "missing `skills`".to_string(),
            });
        }
    };
    build(
        path,
        get("name").and_then(Yaml::as_str),
        get("description").and_then(Yaml::as_str).unwrap_or(""),
        skills,
        get("optional").map(Yaml::strings).unwrap_or_default(),
    )
}

fn from_json(path: &Path, text: &str) -> Result<Bundle> {
    let invalid = |reason: String| SkillError::InvalidBundle {
        path: path.to_path_buf(),
        reason,
    };
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| invalid(err.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("a bundle must be a JSON object".to_string()))?;
    let skills = match object.get("skills") {
        Some(serde_json::Value::String(one)) => vec![one.clone()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| invalid("`skills` must contain strings".to_string()))
            })
            .collect::<Result<Vec<String>>>()?,
        Some(_) => return Err(invalid("`skills` must be a list of strings".to_string())),
        None => return Err(invalid("missing `skills`".to_string())),
    };
    let optional = match object.get("optional") {
        None => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| invalid("`optional` must contain strings".to_string()))
            })
            .collect::<Result<Vec<String>>>()?,
        Some(_) => return Err(invalid("`optional` must be a list of strings".to_string())),
    };
    build(
        path,
        object.get("name").and_then(|v| v.as_str()),
        object
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        skills,
        optional,
    )
}

fn build(
    path: &Path,
    name: Option<&str>,
    description: &str,
    skills: Vec<String>,
    optional: Vec<String>,
) -> Result<Bundle> {
    let invalid = |reason: String| SkillError::InvalidBundle {
        path: path.to_path_buf(),
        reason,
    };
    let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_string(),
        None => path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .ok_or_else(|| {
                invalid("the file has no name and the bundle declares none".to_string())
            })?,
    };
    let skills = members(path, "skills", &skills)?;
    let optional = members(path, "optional", &optional)?;
    if skills.is_empty() {
        return Err(invalid("`skills` must list at least one skill".to_string()));
    }
    Ok(Bundle {
        name,
        description: description.trim().to_string(),
        skills,
        optional,
        path: path.to_path_buf(),
    })
}

fn members(path: &Path, key: &str, raw: &[String]) -> Result<Vec<SkillId>> {
    let mut out: Vec<SkillId> = Vec::new();
    for entry in raw {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let id = SkillId::new(entry).map_err(|err| SkillError::InvalidBundle {
            path: path.to_path_buf(),
            reason: format!("`{key}` contains `{entry}`, which is not a valid skill name: {err}"),
        })?;
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::Compat;
    use crate::skill::Scope;
    use crate::trust::TrustPolicy;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("dirs");
        }
        fs::write(&path, text).expect("write");
        path
    }

    fn skill_doc(name: &str) -> String {
        format!("---\nname: {name}\ndescription: {name} skill\n---\nbody\n")
    }

    fn catalog_with(root: &Path, state: &Path, names: &[&str]) -> Catalog {
        for name in names {
            let dir = root.join(name);
            fs::create_dir_all(&dir).expect("dirs");
            fs::write(dir.join("SKILL.md"), skill_doc(name)).expect("write");
        }
        let opts = DiscoveryOptions::roots_only()
            .with_root(Scope::User, Compat::Native, root.to_path_buf())
            .with_state_dir(state.to_path_buf())
            .with_trust(TrustPolicy::default());
        Catalog::discover(&opts)
    }

    #[test]
    fn a_yaml_bundle_loads_all_its_fields() {
        let tmp = tempfile::tempdir().expect("temp");
        let path = write(
            tmp.path(),
            "web.yaml",
            "name: web-work\ndescription: Build and test websites\nskills:\n  - alpha\n  - beta\noptional:\n  - gamma\n",
        );
        let index = BundleIndex::load_in(tmp.path());
        assert!(index.diagnostics.is_empty(), "{:?}", index.diagnostics);
        let bundle = index.bundles.get("web-work").expect("bundle");
        assert_eq!(bundle.description, "Build and test websites");
        assert_eq!(
            bundle
                .skills
                .iter()
                .map(SkillId::as_str)
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert_eq!(
            bundle
                .optional
                .iter()
                .map(SkillId::as_str)
                .collect::<Vec<_>>(),
            vec!["gamma"]
        );
        assert_eq!(bundle.path, path);
    }

    #[test]
    fn the_file_stem_names_a_bundle_without_a_name_key() {
        let tmp = tempfile::tempdir().expect("temp");
        write(tmp.path(), "quick.json", r#"{"skills": ["alpha"]}"#);
        let index = BundleIndex::load_in(tmp.path());
        assert!(
            index.bundles.contains_key("quick"),
            "{:?}",
            index.diagnostics
        );
    }

    #[test]
    fn a_json_bundle_loads() {
        let tmp = tempfile::tempdir().expect("temp");
        write(
            tmp.path(),
            "ops.json",
            r#"{"name": "ops", "description": "Ops work", "skills": ["alpha"], "optional": ["beta"]}"#,
        );
        let index = BundleIndex::load_in(tmp.path());
        let bundle = index.bundles.get("ops").expect("bundle");
        assert_eq!(bundle.description, "Ops work");
        assert_eq!(bundle.optional.len(), 1);
    }

    #[test]
    fn a_yaml_bundle_without_skills_is_rejected() {
        let tmp = tempfile::tempdir().expect("temp");
        write(
            tmp.path(),
            "broken.yaml",
            "name: broken\ndescription: none\n",
        );
        let index = BundleIndex::load_in(tmp.path());
        assert!(index.bundles.is_empty());
        assert_eq!(index.diagnostics.len(), 1);
        assert_eq!(index.diagnostics[0].level, DiagnosticLevel::Error);
        assert!(index.diagnostics[0].message.contains("missing `skills`"));
    }

    #[test]
    fn an_empty_skills_list_is_rejected() {
        let tmp = tempfile::tempdir().expect("temp");
        write(tmp.path(), "empty.yaml", "name: empty\nskills: []\n");
        let index = BundleIndex::load_in(tmp.path());
        assert!(index.bundles.is_empty());
        assert!(index.diagnostics[0].message.contains("at least one"));
    }

    #[test]
    fn an_invalid_member_name_is_rejected_with_the_name_in_the_message() {
        let tmp = tempfile::tempdir().expect("temp");
        write(
            tmp.path(),
            "bad.yaml",
            "name: bad\nskills:\n  - Not A Skill\n",
        );
        let index = BundleIndex::load_in(tmp.path());
        assert!(index.bundles.is_empty());
        assert!(index.diagnostics[0].message.contains("Not A Skill"));
    }

    #[test]
    fn duplicate_bundle_names_keep_the_first_and_warn() {
        let tmp = tempfile::tempdir().expect("temp");
        write(tmp.path(), "a.yaml", "name: shared\nskills: [alpha]\n");
        write(tmp.path(), "b.yaml", "name: shared\nskills: [beta]\n");
        let index = BundleIndex::load_in(tmp.path());
        assert_eq!(index.bundles.len(), 1);
        assert_eq!(
            index.bundles["shared"]
                .skills
                .iter()
                .map(SkillId::as_str)
                .collect::<Vec<_>>(),
            vec!["alpha"]
        );
        assert!(
            index.diagnostics.iter().any(
                |d| d.level == DiagnosticLevel::Warning && d.message.contains("more than once")
            )
        );
    }

    #[test]
    fn a_missing_bundle_directory_is_an_empty_index() {
        let tmp = tempfile::tempdir().expect("temp");
        let index = BundleIndex::load_in(&tmp.path().join("nope"));
        assert!(index.bundles.is_empty());
        assert!(index.diagnostics.is_empty());
    }

    #[test]
    fn non_bundle_files_are_ignored() {
        let tmp = tempfile::tempdir().expect("temp");
        write(tmp.path(), "notes.txt", "not a bundle");
        let index = BundleIndex::load_in(tmp.path());
        assert!(index.bundles.is_empty());
        assert!(index.diagnostics.is_empty());
    }

    #[test]
    fn resolve_returns_required_members_and_present_optional_ones() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        let state = tmp.path().join("state");
        let catalog = catalog_with(&root, &state, &["alpha", "beta", "gamma"]);
        write(
            tmp.path(),
            "bundles/web.yaml",
            "name: web\nskills:\n  - alpha\n  - beta\noptional:\n  - gamma\n  - delta\n",
        );
        let index = BundleIndex::load_in(&tmp.path().join("bundles"));
        let members = index.resolve_in("web", &catalog).expect("resolve");
        assert_eq!(
            members.iter().map(SkillId::as_str).collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );
    }

    #[test]
    fn resolve_reports_a_missing_required_member() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        let state = tmp.path().join("state");
        let catalog = catalog_with(&root, &state, &["alpha"]);
        write(
            tmp.path(),
            "bundles/web.yaml",
            "name: web\nskills: [alpha, ghost]\n",
        );
        let index = BundleIndex::load_in(&tmp.path().join("bundles"));
        let err = index
            .resolve_in("web", &catalog)
            .expect_err("missing member");
        match err {
            SkillError::MissingDependency { owner, name } => {
                assert_eq!(owner, "web");
                assert_eq!(name, "ghost");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn resolve_reports_a_disabled_member_rather_than_silently_skipping_it() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).expect("dirs");
        fs::write(state.join("disabled.json"), r#"["beta"]"#).expect("write");
        let catalog = catalog_with(&root, &state, &["alpha", "beta"]);
        write(
            tmp.path(),
            "bundles/web.yaml",
            "name: web\nskills: [alpha, beta]\n",
        );
        let index = BundleIndex::load_in(&tmp.path().join("bundles"));
        let err = index
            .resolve_in("web", &catalog)
            .expect_err("disabled member");
        assert!(matches!(err, SkillError::Disabled { .. }));
    }

    #[test]
    fn resolve_of_an_unknown_bundle_is_not_found() {
        let tmp = tempfile::tempdir().expect("temp");
        let index = BundleIndex::load_in(tmp.path());
        let err = index.resolve("nope").expect_err("unknown");
        match err {
            SkillError::NotFound { name, sources } => {
                assert_eq!(name, "nope");
                assert_eq!(sources, 0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn duplicate_members_are_collapsed() {
        let tmp = tempfile::tempdir().expect("temp");
        write(tmp.path(), "b.yaml", "name: b\nskills: [alpha, alpha]\n");
        let index = BundleIndex::load_in(tmp.path());
        assert_eq!(index.bundles["b"].skills.len(), 1);
    }

    #[test]
    fn a_broken_yaml_file_is_a_parse_diagnostic() {
        let tmp = tempfile::tempdir().expect("temp");
        write(tmp.path(), "bad.yaml", "skills:\n  - a\n\t- b\n");
        let index = BundleIndex::load_in(tmp.path());
        assert!(index.bundles.is_empty());
        assert_eq!(index.diagnostics[0].level, DiagnosticLevel::Error);
    }
}
