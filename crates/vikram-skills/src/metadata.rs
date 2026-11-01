//! Skill metadata: the standard `name`/`description`/`license` block plus the `vikram:`
//! extension. Validation is deliberately strict about identity (the name becomes a directory)
//! and generous about everything else — unknown keys are carried, never dropped.

use std::path::PathBuf;

use crate::error::{Result, SkillError};
use crate::frontmatter::{Frontmatter, Yaml};

pub const NAME_MAX: usize = 64;
pub const DESCRIPTION_MAX: usize = 1024;
/// Long descriptions still load, but they are noise in a catalog listing.
pub const DESCRIPTION_WARN: usize = 400;

const KNOWN_KEYS: &[&str] = &[
    "name",
    "description",
    "license",
    "compatibility",
    "allowed-tools",
    "allowed_tools",
    "metadata",
    "vikram",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActivationPolicy {
    Auto,
    #[default]
    Suggested,
    Manual,
}

impl ActivationPolicy {
    pub fn label(self) -> &'static str {
        match self {
            ActivationPolicy::Auto => "auto",
            ActivationPolicy::Suggested => "suggested",
            ActivationPolicy::Manual => "manual",
        }
    }

    pub fn parse(value: &str) -> Option<ActivationPolicy> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "automatic" | "always" => Some(ActivationPolicy::Auto),
            "suggested" | "suggest" | "default" => Some(ActivationPolicy::Suggested),
            "manual" | "never" | "off" => Some(ActivationPolicy::Manual),
            _ => None,
        }
    }

    /// Only `Auto` may be pulled in without the selector being asked.
    pub fn is_automatic(self) -> bool {
        matches!(self, ActivationPolicy::Auto | ActivationPolicy::Suggested)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VikramExtension {
    pub version: Option<String>,
    pub min_vikram: Option<String>,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
    /// `"name"` or `"name@>=1.2"`.
    pub requires: Vec<String>,
    pub recommends: Vec<String>,
    pub permissions: Vec<String>,
    pub activation: ActivationPolicy,
    pub author: Option<String>,
    pub source: Option<String>,
    pub tests: Vec<String>,
}

impl VikramExtension {
    /// The skill name inside a requirement spec, ignoring any version constraint.
    pub fn dependency_name(spec: &str) -> &str {
        match spec.split_once('@') {
            Some((name, _)) => name.trim(),
            None => spec.trim(),
        }
    }

    pub fn required_names(&self) -> Vec<String> {
        self.requires
            .iter()
            .map(|r| Self::dependency_name(r).to_string())
            .filter(|n| !n.is_empty())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.version.is_none()
            && self.min_vikram.is_none()
            && self.categories.is_empty()
            && self.tags.is_empty()
            && self.requires.is_empty()
            && self.recommends.is_empty()
            && self.permissions.is_empty()
            && self.activation == ActivationPolicy::Suggested
            && self.author.is_none()
            && self.source.is_none()
            && self.tests.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Vec<String>,
    /// The standard `metadata:` map, flattened to string pairs.
    pub metadata: Vec<(String, String)>,
    pub vikram: VikramExtension,
    /// Keys this crate does not interpret. Preserved verbatim so an edit never loses them.
    pub unknown: Vec<(String, Yaml)>,
}

impl SkillMetadata {
    pub fn new(name: &str, description: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: description.to_string(),
            ..SkillMetadata::default()
        }
    }

    /// `dir_name` is the identity of the skill: frontmatter `name` must match it, and stands
    /// in for it when the frontmatter omits `name` (a warning, not a failure).
    pub fn from_frontmatter(
        fm: &Frontmatter,
        dir_name: &str,
    ) -> Result<(SkillMetadata, Vec<String>)> {
        let mut warnings = Vec::new();

        let declared = fm.get("name").and_then(Yaml::as_str).map(str::trim);
        let name = match declared.filter(|n| !n.is_empty()) {
            Some(n) => {
                SkillMetadata::validate_name(n)?;
                n.to_string()
            }
            None => {
                if fm.get("name").is_some() && declared.is_none() {
                    warnings.push("`name` is not a string; using the directory name".to_string());
                }
                if SkillMetadata::validate_name(dir_name).is_err() {
                    return Err(SkillError::InvalidName {
                        name: dir_name.to_string(),
                        reason: "frontmatter has no usable `name` and the directory name is not a valid skill name"
                            .to_string(),
                    });
                }
                warnings.push(format!(
                    "missing `name`; using the directory name `{dir_name}`"
                ));
                dir_name.to_string()
            }
        };
        if name != dir_name {
            warnings.push(format!(
                "name `{name}` does not match its directory `{dir_name}`"
            ));
        }

        let description = fm
            .get("description")
            .map(|v| v.strings().join(" ").trim().to_string())
            .unwrap_or_default();
        if description.is_empty() {
            return Err(SkillError::MissingField {
                path: PathBuf::new(),
                field: "description",
            });
        }
        if description.len() > DESCRIPTION_MAX {
            return Err(SkillError::TooLarge {
                what: "description".to_string(),
                size: description.len(),
                limit: DESCRIPTION_MAX,
            });
        }
        if description.len() > DESCRIPTION_WARN {
            warnings.push(format!(
                "description is {} bytes; catalog listings get noisy over {DESCRIPTION_WARN}",
                description.len()
            ));
        }

        let license = fm
            .get("license")
            .and_then(Yaml::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let compatibility = fm
            .get("compatibility")
            .and_then(Yaml::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let allowed_tools = fm
            .get("allowed-tools")
            .or_else(|| fm.get("allowed_tools"))
            .map(Yaml::strings)
            .map(|v| v.into_iter().filter(|s| !s.trim().is_empty()).collect())
            .unwrap_or_default();

        let mut metadata = Vec::new();
        if let Some(map) = fm.get("metadata").and_then(Yaml::as_map) {
            flatten_map(map, "", &mut metadata);
        }

        let vikram = resolve_extension(fm, &metadata, &mut warnings);

        let unknown = fm
            .entries
            .iter()
            .filter(|(k, _)| !KNOWN_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let meta = SkillMetadata {
            name,
            description,
            license,
            compatibility,
            allowed_tools,
            metadata,
            vikram,
            unknown,
        };
        Ok((meta, warnings))
    }

    /// Lowercase ASCII alphanumerics and `-`, no leading/trailing `-`, no `--`, at most
    /// [`NAME_MAX`] bytes. Strict because the name is joined onto a filesystem root.
    pub fn validate_name(name: &str) -> Result<()> {
        let invalid = |reason: String| SkillError::InvalidName {
            name: name.to_string(),
            reason,
        };
        if name.is_empty() {
            return Err(invalid("name is empty".to_string()));
        }
        if name.len() > NAME_MAX {
            return Err(invalid(format!("longer than {NAME_MAX} bytes")));
        }
        let first = name.chars().next().unwrap_or(' ');
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(invalid(
                "must start with a lowercase ASCII letter or a digit".to_string(),
            ));
        }
        if name.ends_with('-') {
            return Err(invalid("must not end with '-'".to_string()));
        }
        if name.contains("--") {
            return Err(invalid("must not contain '--'".to_string()));
        }
        for c in name.chars() {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                return Err(invalid(format!(
                    "character `{c}` is not allowed; use lowercase letters, digits and '-'"
                )));
            }
        }
        Ok(())
    }

    pub fn to_frontmatter(&self, body: &str) -> Frontmatter {
        let mut entries: Vec<(String, Yaml)> = Vec::new();
        entries.push(("name".to_string(), Yaml::Str(self.name.clone())));
        entries.push((
            "description".to_string(),
            Yaml::Str(self.description.clone()),
        ));
        if let Some(license) = &self.license {
            entries.push(("license".to_string(), Yaml::Str(license.clone())));
        }
        if let Some(compat) = &self.compatibility {
            entries.push(("compatibility".to_string(), Yaml::Str(compat.clone())));
        }
        if !self.allowed_tools.is_empty() {
            entries.push((
                "allowed-tools".to_string(),
                Yaml::List(
                    self.allowed_tools
                        .iter()
                        .map(|t| Yaml::Str(t.clone()))
                        .collect(),
                ),
            ));
        }
        if !self.metadata.is_empty() {
            entries.push((
                "metadata".to_string(),
                Yaml::Map(
                    self.metadata
                        .iter()
                        .map(|(k, v)| (k.clone(), Yaml::Str(v.clone())))
                        .collect(),
                ),
            ));
        }
        let extension = extension_entries(&self.vikram);
        if !extension.is_empty() {
            entries.push(("vikram".to_string(), Yaml::Map(extension)));
        }
        entries.extend(self.unknown.iter().cloned());

        let fm = Frontmatter {
            entries,
            body: body.to_string(),
            raw: String::new(),
        };
        let raw = fm.to_yaml();
        Frontmatter { raw, ..fm }
    }
}

fn push_list(out: &mut Vec<(String, Yaml)>, key: &str, values: &[String]) {
    if !values.is_empty() {
        out.push((
            key.to_string(),
            Yaml::List(values.iter().map(|v| Yaml::Str(v.clone())).collect()),
        ));
    }
}

fn extension_entries(v: &VikramExtension) -> Vec<(String, Yaml)> {
    let mut out = Vec::new();
    if let Some(version) = &v.version {
        out.push(("version".to_string(), Yaml::Str(version.clone())));
    }
    if let Some(min) = &v.min_vikram {
        out.push(("min_vikram".to_string(), Yaml::Str(min.clone())));
    }
    push_list(&mut out, "categories", &v.categories);
    push_list(&mut out, "tags", &v.tags);
    push_list(&mut out, "requires", &v.requires);
    push_list(&mut out, "recommends", &v.recommends);
    push_list(&mut out, "permissions", &v.permissions);
    if v.activation != ActivationPolicy::Suggested {
        out.push((
            "activation".to_string(),
            Yaml::Str(v.activation.label().to_string()),
        ));
    }
    if let Some(author) = &v.author {
        out.push(("author".to_string(), Yaml::Str(author.clone())));
    }
    if let Some(source) = &v.source {
        out.push(("source".to_string(), Yaml::Str(source.clone())));
    }
    push_list(&mut out, "tests", &v.tests);
    out
}

fn flatten_map(map: &[(String, Yaml)], prefix: &str, out: &mut Vec<(String, String)>) {
    for (key, value) in map {
        let full = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            Yaml::Map(nested) => flatten_map(nested, &full, out),
            other => out.push((full, other.strings().join(", "))),
        }
    }
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn resolve_extension(
    fm: &Frontmatter,
    metadata: &[(String, String)],
    warnings: &mut Vec<String>,
) -> VikramExtension {
    let direct = fm.get("vikram").and_then(Yaml::as_map);
    let direct_get = |key: &str| -> Option<&Yaml> {
        direct.and_then(|entries| entries.iter().find(|(k, _)| k == key).map(|(_, v)| v))
    };
    let meta_get = |key: &str| -> Option<&str> {
        metadata
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };

    let own = |key: &str| -> Vec<String> { direct_get(key).map(Yaml::strings).unwrap_or_default() };
    let own_string = |key: &str| -> Option<String> {
        direct_get(key)
            .and_then(Yaml::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let mut vikram = VikramExtension {
        version: own_string("version")
            .or_else(|| meta_get("vikram.version").map(str::to_string))
            .or_else(|| meta_get("version").map(str::to_string)),
        min_vikram: own_string("min_vikram").or_else(|| own_string("min-vikram")),
        categories: {
            let own = own("categories");
            if !own.is_empty() {
                own
            } else if let Some(raw) = meta_get("category") {
                split_list(raw)
            } else if let Some(raw) = meta_get("categories") {
                split_list(raw)
            } else {
                Vec::new()
            }
        },
        tags: {
            let own = own("tags");
            if !own.is_empty() {
                own
            } else if let Some(raw) = meta_get("vikram.tags").or_else(|| meta_get("tags")) {
                split_list(raw)
            } else {
                Vec::new()
            }
        },
        requires: own("requires"),
        recommends: own("recommends"),
        permissions: own("permissions"),
        activation: ActivationPolicy::Suggested,
        author: own_string("author").or_else(|| meta_get("author").map(str::to_string)),
        source: own_string("source"),
        tests: own("tests"),
    };

    if let Some(raw) = direct_get("activation").and_then(Yaml::as_str) {
        match ActivationPolicy::parse(raw) {
            Some(policy) => vikram.activation = policy,
            None => warnings.push(format!(
                "unknown activation policy `{raw}`; using `suggested`"
            )),
        }
    } else if let Some(raw) = meta_get("vikram.activation") {
        match ActivationPolicy::parse(raw) {
            Some(policy) => vikram.activation = policy,
            None => warnings.push(format!(
                "unknown activation policy `{raw}`; using `suggested`"
            )),
        }
    }

    vikram
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Frontmatter {
        Frontmatter::parse(text).expect("frontmatter")
    }

    fn meta(text: &str, dir: &str) -> (SkillMetadata, Vec<String>) {
        SkillMetadata::from_frontmatter(&parse(text), dir).expect("metadata")
    }

    #[test]
    fn accepts_valid_names() {
        for name in ["a", "abc", "a1", "code-review", "v2-tools", "a-b-c-d"] {
            assert!(SkillMetadata::validate_name(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn names_are_limited_to_64_bytes_of_lowercase_alnum_and_dashes() {
        for bad in [
            "",
            "Code",
            "code/review",
            "-lead",
            "trail-",
            "double--dash",
            "space name",
            "under_score",
            "dot.dot",
            "ünïcode",
        ] {
            let err = SkillMetadata::validate_name(bad).expect_err(bad);
            assert!(matches!(err, SkillError::InvalidName { .. }), "{bad}");
        }
        let long = "a".repeat(NAME_MAX + 1);
        assert!(SkillMetadata::validate_name(&long).is_err());
        let edge = "a".repeat(NAME_MAX);
        assert!(SkillMetadata::validate_name(&edge).is_ok());
    }

    #[test]
    fn reads_the_standard_fields() {
        let (m, warnings) = meta(
            "---\nname: check-deps\ndescription: Verify dependencies\nlicense: MIT\ncompatibility: vikram>=0.1\nallowed-tools: [fs.read, shell.run]\n---\n",
            "check-deps",
        );
        assert_eq!(m.name, "check-deps");
        assert_eq!(m.description, "Verify dependencies");
        assert_eq!(m.license.as_deref(), Some("MIT"));
        assert_eq!(m.compatibility.as_deref(), Some("vikram>=0.1"));
        assert_eq!(m.allowed_tools, vec!["fs.read", "shell.run"]);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn missing_name_falls_back_to_the_directory_with_a_warning() {
        let (m, warnings) = meta("---\ndescription: d\n---\n", "from-dir");
        assert_eq!(m.name, "from-dir");
        assert!(warnings.iter().any(|w| w.contains("missing `name`")));
    }

    #[test]
    fn missing_name_with_an_invalid_directory_is_an_error() {
        let err = SkillMetadata::from_frontmatter(&parse("---\ndescription: d\n---\n"), "Bad Dir")
            .expect_err("no usable name");
        assert!(matches!(err, SkillError::InvalidName { .. }));
    }

    #[test]
    fn name_directory_mismatch_warns_but_loads() {
        let (m, warnings) = meta("---\nname: alpha\ndescription: d\n---\n", "beta");
        assert_eq!(m.name, "alpha");
        assert!(warnings.iter().any(|w| w.contains("does not match")));
    }

    #[test]
    fn description_is_required_and_bounded() {
        let missing =
            SkillMetadata::from_frontmatter(&parse("---\nname: a\n---\n"), "a").expect_err("none");
        assert!(matches!(
            missing,
            SkillError::MissingField {
                field: "description",
                ..
            }
        ));

        let empty = SkillMetadata::from_frontmatter(
            &parse("---\nname: a\ndescription: \"   \"\n---\n"),
            "a",
        )
        .expect_err("empty");
        assert!(matches!(empty, SkillError::MissingField { .. }));

        let long = format!(
            "---\nname: a\ndescription: {}\n---\n",
            "x".repeat(DESCRIPTION_MAX + 10)
        );
        let over = SkillMetadata::from_frontmatter(&parse(&long), "a").expect_err("too long");
        assert!(matches!(over, SkillError::TooLarge { .. }));
    }

    #[test]
    fn a_long_but_legal_description_only_warns() {
        let text = format!(
            "---\nname: a\ndescription: {}\n---\n",
            "x".repeat(DESCRIPTION_WARN + 1)
        );
        let (_, warnings) = meta(&text, "a");
        assert!(warnings.iter().any(|w| w.contains("noisy")));
    }

    #[test]
    fn reads_the_vikram_extension() {
        let (m, _) = meta(
            "---\nname: a\ndescription: d\nvikram:\n  version: 1.2.0\n  min_vikram: 0.1\n  categories: [dev, ci]\n  tags: [rust, cli]\n  requires: [\"base@>=1.0\"]\n  recommends: [docs]\n  permissions: [\"fs.write:workspace\", net.http]\n  activation: auto\n  author: someone\n  source: https://example.com\n  tests: [\"cargo test\"]\n---\n",
            "a",
        );
        let v = &m.vikram;
        assert_eq!(v.version.as_deref(), Some("1.2.0"));
        assert_eq!(v.min_vikram.as_deref(), Some("0.1"));
        assert_eq!(v.categories, vec!["dev", "ci"]);
        assert_eq!(v.tags, vec!["rust", "cli"]);
        assert_eq!(v.requires, vec!["base@>=1.0"]);
        assert_eq!(v.recommends, vec!["docs"]);
        assert_eq!(v.permissions, vec!["fs.write:workspace", "net.http"]);
        assert_eq!(v.activation, ActivationPolicy::Auto);
        assert_eq!(v.author.as_deref(), Some("someone"));
        assert_eq!(v.source.as_deref(), Some("https://example.com"));
        assert_eq!(v.tests, vec!["cargo test"]);
    }

    #[test]
    fn dotted_metadata_keys_fill_the_extension() {
        let (m, _) = meta(
            "---\nname: a\ndescription: d\nmetadata:\n  vikram.version: 0.9\n  vikram.tags: rust, cli\n  category: dev\n  author: someone\n---\n",
            "a",
        );
        assert_eq!(m.vikram.version.as_deref(), Some("0.9"));
        assert_eq!(m.vikram.tags, vec!["rust", "cli"]);
        assert_eq!(m.vikram.categories, vec!["dev"]);
        assert_eq!(m.vikram.author.as_deref(), Some("someone"));
        assert_eq!(m.metadata.len(), 4);
        assert_eq!(
            m.metadata[0],
            ("vikram.version".to_string(), "0.9".to_string())
        );
    }

    #[test]
    fn an_explicit_extension_wins_over_dotted_metadata() {
        let (m, _) = meta(
            "---\nname: a\ndescription: d\nmetadata:\n  tags: from-meta\nvikram:\n  tags: [from-extension]\n---\n",
            "a",
        );
        assert_eq!(m.vikram.tags, vec!["from-extension"]);
    }

    #[test]
    fn activation_defaults_to_suggested_and_unknown_values_warn() {
        let (m, _) = meta("---\nname: a\ndescription: d\n---\n", "a");
        assert_eq!(m.vikram.activation, ActivationPolicy::Suggested);
        assert!(m.vikram.is_empty());

        let (m, warnings) = meta(
            "---\nname: a\ndescription: d\nvikram:\n  activation: whenever\n---\n",
            "a",
        );
        assert_eq!(m.vikram.activation, ActivationPolicy::Suggested);
        assert!(warnings.iter().any(|w| w.contains("unknown activation")));
    }

    #[test]
    fn unknown_keys_survive_to_frontmatter() {
        let (m, _) = meta(
            "---\nname: a\ndescription: d\nx-custom:\n  nested: [1, 2]\n---\n",
            "a",
        );
        assert_eq!(m.unknown.len(), 1);
        assert_eq!(m.unknown[0].0, "x-custom");
        let fm = m.to_frontmatter("# Body\n");
        assert!(fm.get("x-custom").is_some());
        let (again, warnings) =
            SkillMetadata::from_frontmatter(&fm, "a").expect("round trip metadata");
        assert!(warnings.is_empty());
        assert_eq!(again.unknown, m.unknown);
        assert_eq!(fm.body, "# Body\n");
    }

    #[test]
    fn to_frontmatter_round_trips_every_field() {
        let (m, _) = meta(
            "---\nname: a\ndescription: d\nlicense: MIT\nallowed-tools: [fs.read]\nmetadata:\n  owner: team\nvikram:\n  version: 1.0\n  categories: [dev]\n  tags: [rust]\n  requires: [base]\n  permissions: [\"fs.read:workspace\"]\n  activation: manual\n  author: someone\n---\n",
            "a",
        );
        let fm = m.to_frontmatter("body");
        let (again, warnings) = SkillMetadata::from_frontmatter(&fm, "a").expect("round trip");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(again, m);
    }

    #[test]
    fn dependency_names_ignore_version_constraints() {
        assert_eq!(VikramExtension::dependency_name("base"), "base");
        assert_eq!(VikramExtension::dependency_name("base@>=1.2"), "base");
        let ext = VikramExtension {
            requires: vec!["base@>=1.2".to_string(), "docs".to_string()],
            ..VikramExtension::default()
        };
        assert_eq!(
            ext.required_names(),
            vec!["base".to_string(), "docs".to_string()]
        );
    }

    #[test]
    fn activation_policies_parse_from_their_spellings() {
        assert_eq!(
            ActivationPolicy::parse("auto"),
            Some(ActivationPolicy::Auto)
        );
        assert_eq!(
            ActivationPolicy::parse("Never"),
            Some(ActivationPolicy::Manual)
        );
        assert_eq!(ActivationPolicy::parse("bogus"), None);
        assert_eq!(ActivationPolicy::default(), ActivationPolicy::Suggested);
        assert!(ActivationPolicy::Suggested.is_automatic());
        assert!(!ActivationPolicy::Manual.is_automatic());
    }
}
