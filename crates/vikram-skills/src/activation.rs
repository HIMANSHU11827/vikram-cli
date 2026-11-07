//! Activation: what is loaded right now, and which skills *should* be loaded for a task.
//!
//! The selector is a small, inspectable scorer. It never calls a model and never embeds
//! anything, so the same task against the same catalog always yields the same list, and every
//! entry can say exactly which terms earned its score.

use crate::discovery::Catalog;
use crate::metadata::ActivationPolicy;
use crate::paths;
use crate::skill::{Scope, SkillId, TrustLevel};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationSource {
    /// The user asked for this skill by name.
    Explicit,
    /// The selector chose it.
    Automatic,
    /// A bundle brought it in.
    Bundle(String),
    /// The agent's role requires it.
    Role(String),
    /// Configured to always be present.
    Pinned,
}

impl ActivationSource {
    pub fn label(&self) -> String {
        match self {
            ActivationSource::Explicit => "explicit".to_string(),
            ActivationSource::Automatic => "automatic".to_string(),
            ActivationSource::Bundle(name) => format!("bundle:{name}"),
            ActivationSource::Role(role) => format!("role:{role}"),
            ActivationSource::Pinned => "pinned".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activation {
    pub name: SkillId,
    pub source: ActivationSource,
    /// Why this skill is active, in the same vocabulary the selector uses.
    pub reason: String,
    pub at_ms: u64,
    pub agent: Option<String>,
    pub scope: Scope,
    pub loaded_resources: Vec<String>,
}

impl Activation {
    pub fn new(name: SkillId, source: ActivationSource, reason: impl Into<String>) -> Activation {
        Activation {
            name,
            source,
            reason: reason.into(),
            at_ms: paths::now_ms(),
            agent: None,
            scope: Scope::User,
            loaded_resources: Vec::new(),
        }
    }

    pub fn for_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }

    pub fn in_scope(mut self, scope: Scope) -> Self {
        self.scope = scope;
        self
    }

    pub fn with_resources(mut self, resources: Vec<String>) -> Self {
        self.loaded_resources = resources;
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct ActivationSet {
    pub active: Vec<Activation>,
}

impl ActivationSet {
    /// `false` when the skill is already active: activation is idempotent, and the first
    /// reason wins so the record stays explainable.
    pub fn activate(&mut self, activation: Activation) -> bool {
        if self.contains(&activation.name) {
            return false;
        }
        self.active.push(activation);
        true
    }

    pub fn deactivate(&mut self, name: &SkillId) -> bool {
        let before = self.active.len();
        self.active.retain(|a| a.name != *name);
        before != self.active.len()
    }

    pub fn contains(&self, name: &SkillId) -> bool {
        self.active.iter().any(|a| a.name == *name)
    }

    pub fn names(&self) -> Vec<&SkillId> {
        self.active.iter().map(|a| &a.name).collect()
    }

    pub fn get(&self, name: &SkillId) -> Option<&Activation> {
        self.active.iter().find(|a| a.name == *name)
    }

    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    pub fn clear(&mut self) {
        self.active.clear();
    }
}

pub struct SelectionInput<'a> {
    pub task: &'a str,
    pub project: Option<&'a str>,
    pub agent_role: Option<&'a str>,
    pub pinned: Vec<SkillId>,
    pub exclude: Vec<SkillId>,
    /// At most this many skills; `0` means the default of 3.
    pub limit: usize,
}

impl<'a> SelectionInput<'a> {
    pub fn new(task: &'a str) -> SelectionInput<'a> {
        SelectionInput {
            task,
            project: None,
            agent_role: None,
            pinned: Vec::new(),
            exclude: Vec::new(),
            limit: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    pub name: SkillId,
    pub score: f32,
    /// The terms that produced the score, in a stable order.
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Selector {
    pub min_score: f32,
}

impl Default for Selector {
    fn default() -> Self {
        Selector { min_score: 1.0 }
    }
}

const DEFAULT_LIMIT: usize = 3;

impl Selector {
    pub fn new(min_score: f32) -> Selector {
        Selector { min_score }
    }

    pub fn select(&self, catalog: &Catalog, input: &SelectionInput<'_>) -> Vec<Selection> {
        let task_lower = input.task.to_ascii_lowercase();
        let task_tokens = tokens(input.task);
        let mut out: Vec<Selection> = Vec::new();

        for summary in catalog.list() {
            if input.exclude.iter().any(|e| e == &summary.name) {
                continue;
            }
            if !summary.enabled || !summary.valid {
                continue;
            }
            if summary.trust == TrustLevel::Untrusted {
                continue;
            }
            let pinned = input.pinned.iter().any(|p| p == &summary.name);
            let policy = catalog.activation_policy(&summary.name);
            if policy == ActivationPolicy::Manual && !pinned {
                continue;
            }

            let mut score = 0.0_f32;
            let mut reasons: Vec<String> = Vec::new();

            let name_tokens = tokens(summary.name.as_str());
            if contains_phrase(&task_lower, summary.name.as_str()) {
                // The whole name is stronger evidence than any of its words; counting both
                // would let one mention score twice for the same match.
                score += 10.0;
                reasons.push("name".to_string());
            } else {
                for token in &name_tokens {
                    if task_tokens.contains(token) {
                        score += 3.0;
                        reasons.push(format!("name-token:{token}"));
                    }
                }
            }
            for token in tokens(&summary.description) {
                if name_tokens.contains(&token) {
                    continue;
                }
                if task_tokens.contains(&token) {
                    score += 1.0;
                    reasons.push(format!("description:{token}"));
                }
            }
            for label in summary.tags.iter().chain(summary.categories.iter()) {
                let label = label.to_ascii_lowercase();
                if contains_phrase(&task_lower, &label) {
                    score += 2.0;
                    reasons.push(format!("tag:{label}"));
                }
            }
            if let Some(project) = input.project
                && let (Ok(dir), Ok(root)) = (
                    summary.dir.canonicalize(),
                    std::path::Path::new(project).canonicalize(),
                )
                && dir.starts_with(&root)
            {
                score += 2.0;
                reasons.push("project".to_string());
            }
            if let Some(role) = input.agent_role {
                let role = role.to_ascii_lowercase();
                let matches_role = summary
                    .tags
                    .iter()
                    .chain(summary.categories.iter())
                    .any(|label| label.to_ascii_lowercase() == role)
                    || name_tokens.contains(&role);
                if matches_role {
                    score += 2.0;
                    reasons.push(format!("role:{role}"));
                }
            }
            if pinned {
                score += 100.0;
                reasons.push("pinned".to_string());
            }

            if score < self.min_score {
                continue;
            }
            out.push(Selection {
                name: summary.name.clone(),
                score,
                reason: if reasons.is_empty() {
                    "listed".to_string()
                } else {
                    reasons.join(", ")
                },
            });
        }

        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.name.cmp(&b.name))
        });
        let limit = if input.limit == 0 {
            DEFAULT_LIMIT
        } else {
            input.limit
        };
        out.truncate(limit);
        out
    }
}

/// Lowercase alphanumeric runs of at least two characters, deduplicated in first-seen order.
fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let push = |current: &mut String, out: &mut Vec<String>| {
        if current.chars().count() >= 2 && !out.iter().any(|t| t == current) {
            out.push(current.clone());
        }
        current.clear();
    };
    for c in text.chars() {
        if c.is_alphanumeric() {
            current.extend(c.to_lowercase());
        } else {
            push(&mut current, &mut out);
        }
    }
    push(&mut current, &mut out);
    out
}

fn contains_phrase(haystack_lower: &str, phrase: &str) -> bool {
    if phrase.is_empty() || haystack_lower.len() < phrase.len() {
        return false;
    }
    for (index, _) in haystack_lower.match_indices(phrase) {
        let before = haystack_lower.as_bytes().get(index.wrapping_sub(1));
        let end = index + phrase.len();
        let after = haystack_lower.as_bytes().get(end);
        let boundary = |byte: Option<&u8>| match byte {
            Some(b) => !b.is_ascii_alphanumeric(),
            None => true,
        };
        let before_ok = index == 0 || boundary(before);
        if before_ok && boundary(after) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{Catalog, DiscoveryOptions};
    use crate::trust::TrustPolicy;
    use std::fs;
    use std::path::Path;

    fn id(name: &str) -> SkillId {
        SkillId::new(name).expect("valid name")
    }

    fn write_skill(root: &Path, name: &str, description: &str, extra: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("dirs");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n{extra}---\n# {name}\n"),
        )
        .expect("write");
    }

    fn catalog(tmp: &Path, skills: &[(&str, &str, &str)]) -> Catalog {
        let root = tmp.join("skills");
        for (name, description, extra) in skills {
            write_skill(&root, name, description, extra);
        }
        Catalog::discover(
            &DiscoveryOptions::roots_only()
                .with_root(Scope::User, crate::skill::Compat::Native, root)
                .with_state_dir(tmp.join("state"))
                .with_trust(TrustPolicy::default()),
        )
    }

    #[test]
    fn activation_is_idempotent_and_deactivatable() {
        let mut set = ActivationSet::default();
        assert!(set.activate(Activation::new(
            id("alpha"),
            ActivationSource::Explicit,
            "user asked"
        )));
        assert!(!set.activate(Activation::new(
            id("alpha"),
            ActivationSource::Automatic,
            "second reason"
        )));
        assert!(set.contains(&id("alpha")));
        assert_eq!(set.names(), vec![&id("alpha")]);
        assert_eq!(
            set.get(&id("alpha")).map(|a| a.reason.as_str()),
            Some("user asked")
        );
        assert!(set.deactivate(&id("alpha")));
        assert!(!set.deactivate(&id("alpha")));
        assert!(set.is_empty());
    }

    #[test]
    fn activation_sources_render_for_a_status_line() {
        assert_eq!(ActivationSource::Automatic.label(), "automatic");
        assert_eq!(
            ActivationSource::Bundle("core".into()).label(),
            "bundle:core"
        );
        assert_eq!(
            ActivationSource::Role("reviewer".into()).label(),
            "role:reviewer"
        );
    }

    #[test]
    fn an_exact_name_mention_scores_ten() {
        let tmp = tempfile::tempdir().expect("temp");
        let catalog = catalog(tmp.path(), &[("code-review", "Review a diff", "")]);
        let selector = Selector::default();
        let picked = selector.select(&catalog, &SelectionInput::new("please run code-review now"));
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].score, 10.0);
        assert!(picked[0].reason.contains("name"));
        assert!(!picked[0].reason.contains("name-token"));
    }

    #[test]
    fn name_token_overlap_scores_three_per_token() {
        let tmp = tempfile::tempdir().expect("temp");
        let catalog = catalog(tmp.path(), &[("code-review", "Review a diff", "")]);
        let picked = Selector::default().select(&catalog, &SelectionInput::new("review this code"));
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].score, 6.0);
        assert!(picked[0].reason.contains("name-token:code"));
        assert!(picked[0].reason.contains("name-token:review"));
    }

    #[test]
    fn description_and_tags_add_their_weights() {
        let tmp = tempfile::tempdir().expect("temp");
        let catalog = catalog(
            tmp.path(),
            &[("lint", "Lint Rust sources", "vikram:\n  tags: [ci]\n")],
        );
        let picked =
            Selector::default().select(&catalog, &SelectionInput::new("lint rust with ci"));
        assert_eq!(picked.len(), 1);
        // name 10 + description rust 1 + tag ci 2
        assert_eq!(picked[0].score, 13.0);
        assert!(picked[0].reason.contains("description:rust"));
        assert!(picked[0].reason.contains("tag:ci"));
    }

    #[test]
    fn pinned_outranks_everything_and_is_never_manual_excluded() {
        let tmp = tempfile::tempdir().expect("temp");
        let catalog = catalog(
            tmp.path(),
            &[
                ("code-review", "Review a diff", ""),
                (
                    "house-style",
                    "Team conventions",
                    "vikram:\n  activation: manual\n",
                ),
            ],
        );
        let mut input = SelectionInput::new("unrelated task");
        input.pinned = vec![id("house-style")];
        let picked = Selector::default().select(&catalog, &input);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].name, id("house-style"));
        assert_eq!(picked[0].score, 100.0);
        assert!(picked[0].reason.contains("pinned"));
    }

    #[test]
    fn manual_skills_are_excluded_from_automatic_selection() {
        let tmp = tempfile::tempdir().expect("temp");
        let catalog = catalog(
            tmp.path(),
            &[(
                "house-style",
                "Team conventions",
                "vikram:\n  activation: manual\n",
            )],
        );
        assert!(
            Selector::default()
                .select(&catalog, &SelectionInput::new("house style conventions"))
                .is_empty()
        );
    }

    #[test]
    fn excluded_disabled_and_untrusted_skills_are_never_selected() {
        let tmp = tempfile::tempdir().expect("temp");
        let catalog = catalog(
            tmp.path(),
            &[
                ("alpha", "Alpha thing", ""),
                ("beta", "Beta thing", ""),
                ("gamma", "Gamma thing", ""),
            ],
        );
        let mut input = SelectionInput::new("alpha beta gamma");
        input.exclude = vec![id("beta")];
        let picked = Selector::default().select(&catalog, &input);
        let names: Vec<String> = picked.iter().map(|s| s.name.to_string()).collect();
        assert!(names.contains(&"alpha".to_string()));
        assert!(!names.contains(&"beta".to_string()));
    }

    #[test]
    fn min_score_filters_weak_matches() {
        let tmp = tempfile::tempdir().expect("temp");
        let catalog = catalog(tmp.path(), &[("lint", "Lint Rust sources", "")]);
        let weak = Selector::new(5.0).select(&catalog, &SelectionInput::new("rust"));
        assert!(weak.is_empty());
        let strong = Selector::new(1.0).select(&catalog, &SelectionInput::new("rust"));
        assert_eq!(strong.len(), 1);
    }

    #[test]
    fn the_default_limit_is_three_and_zero_means_default() {
        let tmp = tempfile::tempdir().expect("temp");
        let catalog = catalog(
            tmp.path(),
            &[
                ("alpha", "one", ""),
                ("beta", "two", ""),
                ("gamma", "three", ""),
                ("delta", "four", ""),
            ],
        );
        let mut input = SelectionInput::new("alpha beta gamma delta");
        assert_eq!(Selector::default().select(&catalog, &input).len(), 3);
        input.limit = 0;
        assert_eq!(Selector::default().select(&catalog, &input).len(), 3);
        input.limit = 2;
        assert_eq!(Selector::default().select(&catalog, &input).len(), 2);
    }

    #[test]
    fn selection_is_deterministic() {
        let tmp = tempfile::tempdir().expect("temp");
        let catalog = catalog(tmp.path(), &[("alpha", "one", ""), ("beta", "two", "")]);
        let selector = Selector::default();
        let a = selector.select(&catalog, &SelectionInput::new("alpha beta"));
        let b = selector.select(&catalog, &SelectionInput::new("alpha beta"));
        assert_eq!(a, b);
        assert_eq!(a[0].name, id("alpha"));
    }

    #[test]
    fn project_and_role_contributions_are_named_in_the_reason() {
        let tmp = tempfile::tempdir().expect("temp");
        let workspace = tmp.path().join("ws");
        let root = workspace.join("skills");
        write_skill(
            &root,
            "deploy",
            "Deploy the service",
            "vikram:\n  tags: [release]\n",
        );
        let catalog = Catalog::discover(
            &DiscoveryOptions::roots_only()
                .with_root(Scope::Project, crate::skill::Compat::Native, root)
                .with_state_dir(tmp.path().join("state"))
                .with_trust(TrustPolicy::default()),
        );
        let mut input = SelectionInput::new("deploy");
        input.project = Some(workspace.to_str().expect("utf8"));
        input.agent_role = Some("release");
        let picked = Selector::default().select(&catalog, &input);
        assert_eq!(picked.len(), 1);
        assert!(picked[0].reason.contains("project"));
        assert!(picked[0].reason.contains("role:release"));
        assert_eq!(picked[0].score, 10.0 + 2.0 + 2.0);
    }

    #[test]
    fn phrase_matching_respects_word_boundaries() {
        assert!(contains_phrase("run code-review now", "code-review"));
        assert!(!contains_phrase("run code-reviewer now", "code-review"));
        assert!(contains_phrase("code-review", "code-review"));
        assert!(!contains_phrase("x", "code-review"));
    }

    #[test]
    fn tokens_are_lowercase_unique_and_ignore_single_characters() {
        assert_eq!(tokens("Rust and CI, rust a"), vec!["rust", "and", "ci"]);
        assert!(tokens("a b c").is_empty());
    }
}
