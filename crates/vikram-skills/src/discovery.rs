//! Discovery: turn a set of source roots into a catalog.
//!
//! Discovery is read-only and tolerant. A directory that looks like a skill but has broken
//! frontmatter is kept, marked `valid: false`, and explained with a diagnostic — silently
//! dropping it would leave the user with a skill that never loads and no reason why.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::builtin;
use crate::error::Result;
use crate::frontmatter::Frontmatter;
use crate::metadata::{ActivationPolicy, SkillMetadata};
use crate::paths;
use crate::skill::{Compat, PersonaKind, Scope, SkillId, SkillSource, SkillSummary};
use crate::trust::TrustPolicy;

/// A persona whose own `skills/` directory should be discovered ahead of the shared roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonaSource {
    pub home: PathBuf,
    pub kind: PersonaKind,
    pub slug: String,
}

impl PersonaSource {
    pub fn new(home: impl Into<PathBuf>, kind: PersonaKind, slug: impl Into<String>) -> Self {
        PersonaSource {
            home: home.into(),
            kind,
            slug: slug.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveryOptions {
    pub workspace: Option<PathBuf>,
    pub include_user: bool,
    pub include_builtin: bool,
    pub include_compat: bool,
    pub extra: Vec<(Scope, Compat, PathBuf)>,
    /// How many directory levels below a source root may hold a skill. `0` means unlimited.
    pub max_depth: usize,
    pub follow_symlinks: bool,
    /// Directory names never descended into.
    pub exclude: Vec<String>,
    /// Overrides `<state_dir>` for the disabled list. `None` uses [`crate::paths::state_dir`],
    /// or the home root when one is given.
    pub state_dir: Option<PathBuf>,
    /// Overrides the trust policy read from `<state_dir>/trust.json`.
    pub trust: Option<TrustPolicy>,
    /// Overrides `$VIKRAM_HOME` for the user roots, the state directory and the trust file.
    pub home: Option<PathBuf>,
    /// A persona's own roots, discovered before every shared root.
    pub persona: Option<PersonaSource>,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        DiscoveryOptions {
            workspace: None,
            include_user: true,
            include_builtin: true,
            include_compat: true,
            extra: Vec::new(),
            max_depth: 3,
            follow_symlinks: false,
            exclude: paths::default_excludes(),
            state_dir: None,
            trust: None,
            home: None,
            persona: None,
        }
    }
}

impl DiscoveryOptions {
    /// Everything off except the roots the caller passes: what tests and one-off scans want.
    pub fn roots_only() -> DiscoveryOptions {
        DiscoveryOptions {
            include_user: false,
            include_builtin: false,
            ..DiscoveryOptions::default()
        }
    }

    pub fn with_root(mut self, scope: Scope, compat: Compat, root: PathBuf) -> Self {
        self.extra.push((scope, compat, root));
        self
    }

    pub fn with_state_dir(mut self, dir: PathBuf) -> Self {
        self.state_dir = Some(dir);
        self
    }

    pub fn with_trust(mut self, policy: TrustPolicy) -> Self {
        self.trust = Some(policy);
        self
    }

    pub fn with_workspace(mut self, workspace: PathBuf) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    pub fn with_persona(mut self, persona: PersonaSource) -> Self {
        self.persona = Some(persona);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

impl DiagnosticLevel {
    pub fn label(self) -> &'static str {
        match self {
            DiagnosticLevel::Info => "info",
            DiagnosticLevel::Warning => "warning",
            DiagnosticLevel::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub path: PathBuf,
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn new(level: DiagnosticLevel, path: &Path, message: impl Into<String>) -> Self {
        Diagnostic {
            level,
            path: path.to_path_buf(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatalogStats {
    pub sources: usize,
    pub dirs_scanned: usize,
    pub skills: usize,
    pub duplicates: usize,
    pub invalid: usize,
    pub disabled: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    /// Every candidate found per name, best first. The first entry is what `resolve` returns.
    pub entries: BTreeMap<SkillId, Vec<SkillSummary>>,
    pub diagnostics: Vec<Diagnostic>,
    pub stats: CatalogStats,
    pub(crate) policies: BTreeMap<SkillId, ActivationPolicy>,
    pub(crate) disabled_names: BTreeSet<SkillId>,
}

impl Catalog {
    pub fn discover(opts: &DiscoveryOptions) -> Catalog {
        let state = opts
            .state_dir
            .clone()
            .or_else(|| opts.home.as_deref().map(paths::state_dir_at))
            .unwrap_or_else(paths::state_dir);
        let trust = match &opts.trust {
            Some(policy) => policy.clone(),
            None => match opts.home.as_deref() {
                Some(home) => TrustPolicy::load_in(&paths::state_dir_at(home)),
                None => TrustPolicy::load(),
            },
        };
        let mut catalog = Catalog::default();
        let (disabled, disabled_unknown) = match read_disabled(&state) {
            Ok(disabled) => (disabled, false),
            // Fail closed: an unreadable disabled list must not silently re-enable every
            // skill the user turned off, so all of them report as disabled.
            Err(reason) => {
                catalog.diagnostics.push(Diagnostic::new(
                    DiagnosticLevel::Error,
                    &state.join("disabled.json"),
                    format!(
                        "cannot read disabled list; treating every skill as disabled: {reason}"
                    ),
                ));
                (BTreeSet::new(), true)
            }
        };
        catalog.disabled_names = disabled.clone();
        let mut policies: BTreeMap<SkillId, ActivationPolicy> = BTreeMap::new();

        if opts.include_builtin {
            catalog.stats.sources += 1;
            for skill in builtin::catalog() {
                let dir = builtin::root().join(skill.name);
                if let Some((summary, policy)) = summarize(
                    &dir,
                    Ok(skill.content.to_string()),
                    SkillSource::Builtin,
                    Compat::Native,
                    &trust,
                    &disabled,
                    disabled_unknown,
                    &mut catalog.diagnostics,
                ) {
                    record(&mut catalog, summary, policy, &mut policies);
                }
            }
        }

        for SourceRoot {
            scope,
            compat,
            root,
            source,
        } in collect_sources(opts)
        {
            if !root.is_dir() {
                continue;
            }
            catalog.stats.sources += 1;
            let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
            let mut exclude = opts.exclude.clone();
            if scope == Scope::User
                && compat == Compat::Native
                && same_path(&root, &user_skills_root(opts))
            {
                exclude.extend(paths::compat_subdirs());
            }
            let mut found = Vec::new();
            let mut visited = BTreeSet::new();
            walk(
                &root,
                &canonical_root,
                1,
                opts,
                &exclude,
                &mut visited,
                &mut found,
                &mut catalog.stats,
                &mut catalog.diagnostics,
            );
            found.sort();
            for dir in found {
                let file = match find_skill_file(&dir) {
                    Some(file) => file,
                    None => continue,
                };
                let text = fs::read_to_string(&file)
                    .map_err(|err| crate::error::SkillError::io(&file, &err));
                if let Some((summary, policy)) = summarize(
                    &dir,
                    text,
                    source.clone(),
                    compat,
                    &trust,
                    &disabled,
                    disabled_unknown,
                    &mut catalog.diagnostics,
                ) {
                    record(&mut catalog, summary, policy, &mut policies);
                }
            }
        }

        for list in catalog.entries.values_mut() {
            // Stable sort: equal precedence keeps the source order from `collect_sources`.
            list.sort_by_key(|a| std::cmp::Reverse(a.scope.precedence()));
            if list.len() > 1 {
                catalog.stats.duplicates += list.len() - 1;
                let names: Vec<String> = list
                    .iter()
                    .map(|s| format!("{} ({})", s.dir.display(), s.source))
                    .collect();
                let path = list[0].dir.clone();
                catalog.diagnostics.push(Diagnostic::new(
                    DiagnosticLevel::Info,
                    &path,
                    format!(
                        "`{}` is provided by {} sources: {}",
                        list[0].name,
                        list.len(),
                        names.join(", ")
                    ),
                ));
            }
        }
        catalog.policies = policies;
        catalog
    }

    /// Build a catalog for `workspace_root` with an optional persona tier ahead of the shared
    /// roots. With `persona: None` the result is the same set `SkillRegistry::for_workspace`
    /// scans: user roots, then project roots, no builtins.
    pub fn for_workspace_and_persona(
        workspace: &Path,
        home: &Path,
        persona: Option<(&str, PersonaKind)>,
    ) -> Catalog {
        let mut opts = DiscoveryOptions {
            include_user: true,
            ..DiscoveryOptions::roots_only()
        }
        .with_home(home.to_path_buf())
        .with_workspace(workspace.to_path_buf());
        if let Some((slug, kind)) = persona {
            opts = opts.with_persona(PersonaSource::new(home, kind, slug));
        }
        Catalog::discover(&opts)
    }

    /// The precedence winner for a name.
    pub fn resolve(&self, name: &str) -> Option<&SkillSummary> {
        self.entries.get(name).and_then(|list| list.first())
    }

    /// Every name offered by more than one source, with all its candidates.
    pub fn conflicts(&self) -> Vec<(&SkillId, &[SkillSummary])> {
        self.entries
            .iter()
            .filter(|(_, list)| list.len() > 1)
            .map(|(name, list)| (name, list.as_slice()))
            .collect()
    }

    /// The resolved winner for every name, sorted by name.
    pub fn list(&self) -> Vec<&SkillSummary> {
        self.entries
            .values()
            .filter_map(|list| list.first())
            .collect()
    }

    /// Case-insensitive substring search; every whitespace-separated term must match the name,
    /// the description, a tag or a category.
    pub fn search(&self, query: &str) -> Vec<&SkillSummary> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        if terms.is_empty() {
            return Vec::new();
        }
        self.list()
            .into_iter()
            .filter(|summary| {
                let haystack = format!(
                    "{} {} {} {}",
                    summary.name.as_str(),
                    summary.description,
                    summary.tags.join(" "),
                    summary.categories.join(" ")
                )
                .to_ascii_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .collect()
    }

    /// Names whose resolved candidate is disabled, plus any name recorded as disabled in the
    /// state file that is not installed right now.
    pub fn disabled(&self) -> BTreeSet<SkillId> {
        let mut set = self.disabled_names.clone();
        for (name, list) in &self.entries {
            if list.first().map(|s| !s.enabled).unwrap_or(false) {
                set.insert(name.clone());
            }
        }
        set
    }

    pub fn refresh(&mut self, opts: &DiscoveryOptions) {
        *self = Catalog::discover(opts);
    }

    /// The activation policy that belongs to the resolved candidate. Skills the catalog never
    /// saw default to `Suggested`.
    pub(crate) fn activation_policy(&self, name: &SkillId) -> ActivationPolicy {
        self.policies
            .get(name)
            .copied()
            .unwrap_or(ActivationPolicy::Suggested)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Diagnostics at or above `level`.
    pub fn diagnostics_at(&self, level: DiagnosticLevel) -> Vec<&Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.level >= level)
            .collect()
    }
}

fn record(
    catalog: &mut Catalog,
    summary: SkillSummary,
    policy: ActivationPolicy,
    policies: &mut BTreeMap<SkillId, ActivationPolicy>,
) {
    catalog.stats.skills += 1;
    if !summary.valid {
        catalog.stats.invalid += 1;
    }
    if !summary.enabled {
        catalog.stats.disabled += 1;
    }
    let name = summary.name.clone();
    catalog
        .entries
        .entry(name.clone())
        .or_default()
        .push(summary);
    policies.entry(name).or_insert(policy);
}

/// One root to walk plus the origin recorded on every skill found under it.
#[derive(Debug)]
struct SourceRoot {
    scope: Scope,
    compat: Compat,
    root: PathBuf,
    source: SkillSource,
}

/// A root that claims `Scope::Persona` without a persona configured cannot be attributed, so it
/// is skipped rather than mislabelled.
fn source_root(
    scope: Scope,
    compat: Compat,
    root: PathBuf,
    persona: Option<&PersonaSource>,
) -> Option<SourceRoot> {
    let source = match scope {
        Scope::Builtin => SkillSource::Builtin,
        Scope::User => SkillSource::User,
        Scope::Project => SkillSource::Project,
        Scope::Imported => SkillSource::Imported,
        Scope::External => SkillSource::External,
        Scope::Persona => SkillSource::Persona {
            kind: persona?.kind,
            slug: persona?.slug.clone(),
        },
    };
    Some(SourceRoot {
        scope,
        compat,
        root,
        source,
    })
}

fn push_roots(
    out: &mut Vec<SourceRoot>,
    roots: Vec<(Scope, Compat, PathBuf)>,
    persona: Option<&PersonaSource>,
) {
    for (scope, compat, root) in roots {
        if let Some(source) = source_root(scope, compat, root, persona) {
            out.push(source);
        }
    }
}

fn collect_sources(opts: &DiscoveryOptions) -> Vec<SourceRoot> {
    let persona = opts.persona.as_ref();
    let mut sources = Vec::new();
    if let Some(persona) = persona {
        push_roots(
            &mut sources,
            paths::persona_skill_dirs(&persona.home, &persona.slug, persona.kind),
            Some(persona),
        );
    }
    if let Some(workspace) = &opts.workspace {
        push_roots(&mut sources, paths::project_skill_dirs(workspace), persona);
    }
    if opts.include_user {
        push_roots(&mut sources, user_skill_dirs(opts), persona);
    }
    push_roots(&mut sources, opts.extra.clone(), persona);
    if !opts.include_compat {
        sources.retain(|source| source.compat == Compat::Native);
    }
    sources
}

fn user_skill_dirs(opts: &DiscoveryOptions) -> Vec<(Scope, Compat, PathBuf)> {
    match &opts.home {
        Some(home) => paths::user_skill_dirs_at(home),
        None => paths::user_skill_dirs(),
    }
}

fn user_skills_root(opts: &DiscoveryOptions) -> PathBuf {
    match &opts.home {
        Some(home) => paths::user_skills_dir_at(home),
        None => paths::user_skills_dir(),
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// `SKILL.md`, with the lowercase spelling accepted for case-sensitive filesystems.
pub(crate) fn find_skill_file(dir: &Path) -> Option<PathBuf> {
    for candidate in ["SKILL.md", "skill.md"] {
        let path = dir.join(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn walk(
    dir: &Path,
    canonical_root: &Path,
    depth: usize,
    opts: &DiscoveryOptions,
    exclude: &[String],
    visited: &mut BTreeSet<PathBuf>,
    found: &mut Vec<PathBuf>,
    stats: &mut CatalogStats,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // A followed symlink may point at an ancestor of the root, which would otherwise
    // recurse forever when max_depth is unlimited. Canonical dirs are walked once.
    if opts.follow_symlinks {
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if !visited.insert(canonical) {
            return;
        }
    }
    stats.dirs_scanned += 1;
    let entries = match paths::sorted_entries(dir) {
        Ok(entries) => entries,
        Err(err) => {
            diagnostics.push(Diagnostic::new(
                DiagnosticLevel::Warning,
                dir,
                format!("cannot read directory: {err}"),
            ));
            return;
        }
    };
    for path in entries {
        let name = match path.file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => continue,
        };
        if exclude.iter().any(|excluded| excluded == &name) {
            continue;
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) => {
                diagnostics.push(Diagnostic::new(
                    DiagnosticLevel::Warning,
                    &path,
                    format!("cannot stat entry: {err}"),
                ));
                continue;
            }
        };
        let mut is_dir = meta.is_dir();
        if meta.file_type().is_symlink() {
            if !opts.follow_symlinks {
                diagnostics.push(Diagnostic::new(
                    DiagnosticLevel::Info,
                    &path,
                    "symlink skipped; enable follow_symlinks to scan it",
                ));
                continue;
            }
            match path.canonicalize() {
                Ok(target) => {
                    if !target.starts_with(canonical_root) {
                        diagnostics.push(Diagnostic::new(
                            DiagnosticLevel::Warning,
                            &path,
                            "symlink resolves outside its source root; refused",
                        ));
                        continue;
                    }
                    is_dir = target.is_dir();
                }
                Err(err) => {
                    diagnostics.push(Diagnostic::new(
                        DiagnosticLevel::Warning,
                        &path,
                        format!("cannot resolve symlink: {err}"),
                    ));
                    continue;
                }
            }
        }
        if !is_dir {
            continue;
        }
        match find_skill_file(&path) {
            Some(file) => {
                if !opts.follow_symlinks
                    && let Ok(file_meta) = fs::symlink_metadata(&file)
                    && file_meta.file_type().is_symlink()
                {
                    diagnostics.push(Diagnostic::new(
                        DiagnosticLevel::Warning,
                        &file,
                        "SKILL.md is a symlink; refused",
                    ));
                    continue;
                }
                found.push(path);
            }
            None => {
                let below_limit = opts.max_depth == 0 || depth < opts.max_depth;
                if below_limit {
                    walk(
                        &path,
                        canonical_root,
                        depth + 1,
                        opts,
                        exclude,
                        visited,
                        found,
                        stats,
                        diagnostics,
                    );
                }
            }
        }
    }
}

fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    line.chars().take(160).collect()
}

#[allow(clippy::too_many_arguments)]
fn summarize(
    dir: &Path,
    text: Result<String>,
    source: SkillSource,
    compat: Compat,
    trust: &TrustPolicy,
    disabled: &BTreeSet<SkillId>,
    disabled_unknown: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<(SkillSummary, ActivationPolicy)> {
    let dir_name = dir.file_name()?.to_string_lossy().into_owned();
    let name = match SkillId::new(&dir_name) {
        Ok(name) => name,
        Err(err) => {
            diagnostics.push(Diagnostic::new(
                DiagnosticLevel::Error,
                dir,
                err.to_string(),
            ));
            return None;
        }
    };
    let scope = source.scope();
    let mut summary = SkillSummary {
        enabled: !disabled_unknown && !disabled.contains(&name),
        name,
        description: String::new(),
        dir: dir.to_path_buf(),
        scope,
        compat,
        trust: trust.level_for(scope, compat, dir),
        valid: false,
        version: None,
        categories: Vec::new(),
        tags: Vec::new(),
        source,
    };
    let mut activation = ActivationPolicy::Suggested;

    let text = match text {
        Ok(text) => text,
        Err(err) => {
            diagnostics.push(Diagnostic::new(
                DiagnosticLevel::Error,
                dir,
                err.to_string(),
            ));
            return Some((summary, activation));
        }
    };
    let skill_file = dir.join("SKILL.md");
    let fm = match Frontmatter::parse_at(&text, &skill_file) {
        Ok(fm) => fm,
        Err(err) => {
            diagnostics.push(Diagnostic::new(
                DiagnosticLevel::Error,
                dir,
                err.to_string(),
            ));
            summary.description = first_line(&text);
            return Some((summary, activation));
        }
    };

    match SkillMetadata::from_frontmatter(&fm, &dir_name) {
        Ok((metadata, warnings)) => {
            for warning in warnings {
                diagnostics.push(Diagnostic::new(DiagnosticLevel::Warning, dir, warning));
            }
            summary.description = metadata.description;
            summary.version = metadata.vikram.version.clone();
            summary.categories = metadata.vikram.categories.clone();
            summary.tags = metadata.vikram.tags.clone();
            summary.valid = true;
            activation = metadata.vikram.activation;
        }
        Err(err) => {
            diagnostics.push(Diagnostic::new(
                DiagnosticLevel::Error,
                dir,
                err.to_string(),
            ));
            summary.description = first_line(&fm.body);
        }
    }
    Some((summary, activation))
}

fn read_disabled(state_dir: &Path) -> std::result::Result<BTreeSet<SkillId>, String> {
    let path = state_dir.join("disabled.json");
    let names: Vec<String> = match paths::read_json_strict(&path) {
        Ok(Some(names)) => names,
        Ok(None) => return Ok(BTreeSet::new()),
        Err(reason) => return Err(reason),
    };
    Ok(names
        .iter()
        .filter_map(|name| SkillId::new(name).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::{Budget, Loader};
    use crate::skill::TrustLevel;

    fn write_skill(root: &Path, name: &str, body: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("create dir");
        fs::write(dir.join("SKILL.md"), body).expect("write skill");
    }

    fn skill_doc(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\nBody.\n")
    }

    fn opts(root: &Path, state: &Path, scope: Scope) -> DiscoveryOptions {
        DiscoveryOptions::roots_only()
            .with_root(scope, Compat::Native, root.to_path_buf())
            .with_state_dir(state.to_path_buf())
            .with_trust(TrustPolicy::default())
    }

    #[test]
    fn finds_a_skill_directory_by_its_skill_file() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        write_skill(&root, "alpha", &skill_doc("alpha", "Does alpha"));
        let catalog = Catalog::discover(&opts(&root, tmp.path(), Scope::User));
        assert_eq!(catalog.len(), 1);
        let summary = catalog.resolve("alpha").expect("found");
        assert_eq!(summary.description, "Does alpha");
        assert!(summary.valid);
        assert!(summary.enabled);
        assert_eq!(summary.scope, Scope::User);
        assert!(summary.dir.ends_with("alpha"));
        assert_eq!(catalog.stats.skills, 1);
    }

    #[test]
    fn accepts_a_lowercase_skill_file_name() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        let dir = root.join("alpha");
        fs::create_dir_all(&dir).expect("dirs");
        fs::write(dir.join("skill.md"), skill_doc("alpha", "lowercase")).expect("write");
        let catalog = Catalog::discover(&opts(&root, tmp.path(), Scope::User));
        assert!(catalog.resolve("alpha").is_some());
    }

    #[test]
    fn missing_roots_are_ignored_without_diagnostics() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("does-not-exist");
        let catalog = Catalog::discover(&opts(&root, tmp.path(), Scope::User));
        assert!(catalog.is_empty());
        assert_eq!(catalog.stats.sources, 0);
        assert!(catalog.diagnostics.is_empty());
    }

    #[test]
    fn max_depth_bounds_the_walk_and_zero_means_unlimited() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        let nested = root.join("group/inner");
        fs::create_dir_all(&nested).expect("dirs");
        fs::write(nested.join("SKILL.md"), skill_doc("inner", "nested")).expect("write");

        let mut shallow = opts(&root, tmp.path(), Scope::User);
        shallow.max_depth = 1;
        assert!(Catalog::discover(&shallow).is_empty());

        let mut deep = opts(&root, tmp.path(), Scope::User);
        deep.max_depth = 3;
        assert!(Catalog::discover(&deep).resolve("inner").is_some());

        let mut unlimited = opts(&root, tmp.path(), Scope::User);
        unlimited.max_depth = 0;
        assert!(Catalog::discover(&unlimited).resolve("inner").is_some());
    }

    #[test]
    fn invalid_skills_are_kept_and_explained() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        write_skill(
            &root,
            "broken",
            "---\nname: broken\n---\n# No description\n",
        );
        let catalog = Catalog::discover(&opts(&root, tmp.path(), Scope::User));
        let summary = catalog.resolve("broken").expect("kept");
        assert!(!summary.valid);
        assert_eq!(summary.description, "# No description");
        assert_eq!(catalog.stats.invalid, 1);
        assert!(
            catalog
                .diagnostics_at(DiagnosticLevel::Error)
                .iter()
                .any(|d| d.message.contains("description"))
        );
    }

    #[test]
    fn malformed_yaml_is_a_diagnostic_not_a_drop() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        write_skill(&root, "bad", "---\nname: a\nname: b\n---\n");
        let catalog = Catalog::discover(&opts(&root, tmp.path(), Scope::User));
        let summary = catalog.resolve("bad").expect("kept");
        assert!(!summary.valid);
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|d| d.message.contains("duplicate key"))
        );
    }

    #[test]
    fn a_directory_with_an_unusable_name_is_reported() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        write_skill(&root, "Bad_Name", &skill_doc("bad-name", "d"));
        let catalog = Catalog::discover(&opts(&root, tmp.path(), Scope::User));
        assert!(catalog.is_empty());
        assert!(
            catalog
                .diagnostics_at(DiagnosticLevel::Error)
                .iter()
                .any(|d| d.message.contains("invalid skill name"))
        );
    }

    #[test]
    fn higher_precedence_scope_wins_a_duplicate() {
        let tmp = tempfile::tempdir().expect("temp");
        let project = tmp.path().join("ws/skills");
        let user = tmp.path().join("home/skills");
        write_skill(&project, "alpha", &skill_doc("alpha", "from project"));
        write_skill(&user, "alpha", &skill_doc("alpha", "from user"));

        let options = DiscoveryOptions::roots_only()
            .with_root(Scope::Project, Compat::Native, project)
            .with_root(Scope::User, Compat::Native, user)
            .with_state_dir(tmp.path().to_path_buf())
            .with_trust(TrustPolicy::default());
        let catalog = Catalog::discover(&options);

        let winner = catalog.resolve("alpha").expect("winner");
        assert_eq!(winner.scope, Scope::User);
        assert_eq!(winner.description, "from user");
        assert_eq!(catalog.entries.len(), 1);
        assert_eq!(catalog.entries["alpha"].len(), 2);
        assert_eq!(catalog.entries["alpha"][1].scope, Scope::Project);
        assert_eq!(catalog.stats.duplicates, 1);
        assert_eq!(catalog.conflicts().len(), 1);
    }

    #[test]
    fn disabled_names_come_from_the_state_file() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).expect("state");
        fs::write(state.join("disabled.json"), "[\"alpha\"]").expect("write");
        write_skill(&root, "alpha", &skill_doc("alpha", "d"));
        write_skill(&root, "beta", &skill_doc("beta", "d"));

        let catalog = Catalog::discover(&opts(&root, &state, Scope::User));
        assert!(!catalog.resolve("alpha").expect("alpha").enabled);
        assert!(catalog.resolve("beta").expect("beta").enabled);
        assert_eq!(catalog.stats.disabled, 1);
        assert_eq!(catalog.disabled().len(), 1);
        assert!(
            catalog
                .disabled()
                .contains(&SkillId::new("alpha").expect("id"))
        );
        assert!(!catalog.resolve("alpha").expect("alpha").enabled);
    }

    #[test]
    fn a_corrupt_disabled_file_does_not_re_enable_skills() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).expect("state");
        fs::write(state.join("disabled.json"), "{not json").expect("write");
        write_skill(&root, "alpha", &skill_doc("alpha", "d"));

        let catalog = Catalog::discover(&opts(&root, &state, Scope::User));
        let summary = catalog.resolve("alpha").expect("kept");
        assert!(!summary.enabled);
        assert!(
            catalog
                .diagnostics_at(DiagnosticLevel::Error)
                .iter()
                .any(|d| d.message.contains("disabled list"))
        );
        assert!(catalog.disabled().contains(&summary.name));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_back_to_an_ancestor_terminates() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        let nested = root.join("group/inner");
        fs::create_dir_all(&nested).expect("dirs");
        write_skill(&nested, "alpha", &skill_doc("alpha", "d"));
        std::os::unix::fs::symlink(&root, root.join("loop")).expect("symlink");
        std::os::unix::fs::symlink(tmp.path(), root.join("group/up")).expect("symlink");

        let mut options = opts(&root, tmp.path(), Scope::User);
        options.follow_symlinks = true;
        options.max_depth = 0;
        let catalog = Catalog::discover(&options);
        assert!(catalog.resolve("alpha").is_some());
        assert_eq!(catalog.stats.dirs_scanned, 3);
    }

    #[test]
    fn compat_roots_are_skipped_when_disabled() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("claude-skills");
        write_skill(&root, "alpha", &skill_doc("alpha", "d"));

        let mut options = DiscoveryOptions::roots_only()
            .with_root(Scope::User, Compat::Claude, root)
            .with_state_dir(tmp.path().to_path_buf())
            .with_trust(TrustPolicy::default());
        assert!(Catalog::discover(&options).resolve("alpha").is_some());

        options.include_compat = false;
        assert!(Catalog::discover(&options).resolve("alpha").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_skipped_unless_asked() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).expect("root");
        let outside = tmp.path().join("outside/alpha");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("SKILL.md"), skill_doc("alpha", "d")).expect("write");
        std::os::unix::fs::symlink(&outside, root.join("alpha")).expect("symlink");

        let catalog = Catalog::discover(&opts(&root, tmp.path(), Scope::User));
        assert!(catalog.resolve("alpha").is_none());
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|d| d.message.contains("symlink skipped"))
        );

        let mut following = opts(&root, tmp.path(), Scope::User);
        following.follow_symlinks = true;
        let catalog = Catalog::discover(&following);
        assert!(catalog.resolve("alpha").is_none());
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|d| d.message.contains("outside its source root"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_root_is_followed_when_allowed() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        let real = root.join("real-alpha");
        fs::create_dir_all(&real).expect("real");
        fs::write(real.join("SKILL.md"), skill_doc("alpha", "d")).expect("write");
        std::os::unix::fs::symlink(&real, root.join("alpha")).expect("symlink");

        let mut options = opts(&root, tmp.path(), Scope::User);
        options.follow_symlinks = true;
        let catalog = Catalog::discover(&options);
        // Identity is the directory name, so the symlink and its target are two entries that
        // point at the same SKILL.md.
        assert!(catalog.resolve("alpha").is_some());
        assert!(catalog.resolve("real-alpha").is_some());
        assert_eq!(catalog.len(), 2);
    }

    #[test]
    fn search_requires_every_term_across_name_description_tags_and_categories() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        write_skill(
            &root,
            "rust-lint",
            "---\nname: rust-lint\ndescription: Lint Rust code\nvikram:\n  tags: [ci]\n  categories: [dev]\n---\n",
        );
        write_skill(&root, "docs", &skill_doc("docs", "Write prose"));
        let catalog = Catalog::discover(&opts(&root, tmp.path(), Scope::User));

        assert_eq!(catalog.search("rust").len(), 1);
        assert_eq!(catalog.search("RUST lint").len(), 1);
        assert_eq!(catalog.search("ci dev").len(), 1);
        assert_eq!(catalog.search("rust docs").len(), 0);
        assert!(catalog.search("   ").is_empty());
    }

    #[test]
    fn builtins_are_discovered_with_their_own_scope_and_trust() {
        let tmp = tempfile::tempdir().expect("temp");
        let options = DiscoveryOptions {
            include_user: false,
            include_builtin: true,
            state_dir: Some(tmp.path().to_path_buf()),
            trust: Some(TrustPolicy::default()),
            ..DiscoveryOptions::default()
        };
        let catalog = Catalog::discover(&options);
        let author = catalog.resolve("skill-author").expect("builtin present");
        assert_eq!(author.scope, Scope::Builtin);
        assert_eq!(author.trust, TrustLevel::Builtin);
        assert!(author.valid);
        assert_eq!(author.dir, builtin::root().join("skill-author"));
        assert!(!author.tags.is_empty());
    }

    #[test]
    fn refresh_rebuilds_the_catalog() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        write_skill(&root, "alpha", &skill_doc("alpha", "d"));
        let options = opts(&root, tmp.path(), Scope::User);
        let mut catalog = Catalog::discover(&options);
        assert_eq!(catalog.len(), 1);
        write_skill(&root, "beta", &skill_doc("beta", "d"));
        catalog.refresh(&options);
        assert_eq!(catalog.len(), 2);
        assert!(catalog.resolve("beta").is_some());
    }

    #[test]
    fn excluded_directories_are_not_descended_into() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        write_skill(&root, "node_modules/pkg", &skill_doc("hidden", "d"));
        let catalog = Catalog::discover(&opts(&root, tmp.path(), Scope::User));
        assert!(catalog.resolve("hidden").is_none());
    }

    #[test]
    fn list_is_sorted_and_carries_one_entry_per_name() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("skills");
        write_skill(&root, "zeta", &skill_doc("zeta", "d"));
        write_skill(&root, "alpha", &skill_doc("alpha", "d"));
        let catalog = Catalog::discover(&opts(&root, tmp.path(), Scope::User));
        let names: Vec<&str> = catalog.list().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn trust_policy_lowers_a_third_party_layout() {
        let tmp = tempfile::tempdir().expect("temp");
        let root = tmp.path().join("opencode/skills");
        write_skill(&root, "alpha", &skill_doc("alpha", "d"));
        let options = DiscoveryOptions::roots_only()
            .with_root(Scope::User, Compat::OpenCode, root)
            .with_state_dir(tmp.path().to_path_buf())
            .with_trust(TrustPolicy::default());
        let catalog = Catalog::discover(&options);
        assert_eq!(
            catalog.resolve("alpha").expect("found").trust,
            TrustLevel::Community
        );
    }

    fn persona_options(
        workspace: &Path,
        home: &Path,
        persona: Option<(&str, PersonaKind)>,
    ) -> DiscoveryOptions {
        let mut options = DiscoveryOptions {
            include_user: true,
            ..DiscoveryOptions::roots_only()
        }
        .with_home(home.to_path_buf())
        .with_workspace(workspace.to_path_buf());
        if let Some((slug, kind)) = persona {
            options = options.with_persona(PersonaSource::new(home, kind, slug));
        }
        options
    }

    fn snapshot(catalog: &Catalog) -> Vec<(String, PathBuf, Scope)> {
        catalog
            .list()
            .iter()
            .map(|summary| {
                (
                    summary.name.as_str().to_string(),
                    summary.dir.clone(),
                    summary.scope,
                )
            })
            .collect()
    }

    #[test]
    fn a_persona_skill_is_discovered_before_a_user_skill() {
        let tmp = tempfile::tempdir().expect("temp");
        let workspace = tmp.path().join("ws");
        let home = tmp.path().join("home");
        let persona_root = home.join("agents/reviewer/skills");
        let user_root = home.join("skills");
        write_skill(
            &persona_root,
            "persona-only",
            &skill_doc("persona-only", "from the persona"),
        );
        write_skill(
            &user_root,
            "user-only",
            &skill_doc("user-only", "from the user"),
        );

        let options = persona_options(&workspace, &home, Some(("reviewer", PersonaKind::Agent)));
        let sources = collect_sources(&options);
        assert_eq!(sources[0].root, persona_root);
        assert_eq!(sources[0].scope, Scope::Persona);
        let user_index = sources
            .iter()
            .position(|source| source.root == user_root)
            .expect("user root");
        assert!(user_index > 0, "{sources:?}");

        let catalog = Catalog::discover(&options);
        let persona = catalog.resolve("persona-only").expect("persona skill");
        assert_eq!(
            persona.source,
            SkillSource::Persona {
                kind: PersonaKind::Agent,
                slug: "reviewer".to_string(),
            }
        );
        assert_eq!(persona.scope, Scope::Persona);
        assert_eq!(
            catalog.resolve("user-only").expect("user skill").source,
            SkillSource::User
        );
    }

    #[test]
    fn a_persona_skill_shadows_a_user_skill_with_the_same_name() {
        let tmp = tempfile::tempdir().expect("temp");
        let workspace = tmp.path().join("ws");
        let home = tmp.path().join("home");
        let persona_dir = home.join("agents/reviewer/skills/alpha");
        let user_dir = home.join("skills/alpha");
        write_skill(
            &home.join("agents/reviewer/skills"),
            "alpha",
            "---\nname: alpha\ndescription: persona alpha\n---\npersona body\n",
        );
        write_skill(
            &home.join("skills"),
            "alpha",
            "---\nname: alpha\ndescription: user alpha\n---\nuser body\n",
        );

        let catalog = Catalog::for_workspace_and_persona(
            &workspace,
            &home,
            Some(("reviewer", PersonaKind::Agent)),
        );

        let alpha: Vec<&SkillSummary> = catalog
            .list()
            .into_iter()
            .filter(|summary| summary.name.as_str() == "alpha")
            .collect();
        assert_eq!(alpha.len(), 1, "the catalogue lists one entry per name");
        assert_eq!(alpha[0].dir, persona_dir);
        assert_eq!(alpha[0].description, "persona alpha");
        assert_eq!(alpha[0].scope, Scope::Persona);
        assert_eq!(catalog.stats.duplicates, 1);
        assert_eq!(
            catalog.resolve("alpha").expect("winner").dir,
            persona_dir,
            "the persona copy shadows the user copy"
        );

        let winner = catalog.resolve("alpha").expect("winner");
        let loaded = Loader::new(Budget::default()).load(winner).expect("load");
        assert!(loaded.instructions.contains("persona body"));
        assert!(!loaded.instructions.contains("user body"));
        assert_ne!(winner.dir, user_dir);
    }

    #[test]
    fn a_registry_without_a_persona_finds_exactly_what_it_found_before() {
        let tmp = tempfile::tempdir().expect("temp");
        let workspace = tmp.path().join("ws");
        let home = tmp.path().join("home");
        write_skill(
            &workspace.join("skills"),
            "alpha",
            &skill_doc("alpha", "from the project"),
        );
        write_skill(
            &home.join("skills"),
            "beta",
            &skill_doc("beta", "from the user"),
        );
        write_skill(
            &home.join("agents/reviewer/skills"),
            "gamma",
            &skill_doc("gamma", "persona-only"),
        );

        let mut legacy = DiscoveryOptions::roots_only()
            .with_state_dir(home.join("skills/.vikram"))
            .with_trust(TrustPolicy::default());
        for (scope, compat, root) in paths::user_skill_dirs_at(&home) {
            legacy = legacy.with_root(scope, compat, root);
        }
        for (scope, compat, root) in paths::project_skill_dirs(&workspace) {
            legacy = legacy.with_root(scope, compat, root);
        }

        let without_persona = Catalog::for_workspace_and_persona(&workspace, &home, None);

        assert_eq!(
            snapshot(&without_persona),
            snapshot(&Catalog::discover(&legacy))
        );
        assert_eq!(
            snapshot(&without_persona),
            vec![
                (
                    "alpha".to_string(),
                    workspace.join("skills/alpha"),
                    Scope::Project
                ),
                ("beta".to_string(), home.join("skills/beta"), Scope::User),
            ]
        );
        assert!(
            without_persona
                .list()
                .iter()
                .all(|summary| summary.source.persona().is_none())
        );
        assert!(without_persona.resolve("gamma").is_none());
    }

    #[test]
    fn a_missing_persona_directory_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("temp");
        let workspace = tmp.path().join("ws");
        let home = tmp.path().join("home");
        fs::create_dir_all(&workspace).expect("workspace");
        write_skill(
            &home.join("skills"),
            "shared",
            &skill_doc("shared", "from the user"),
        );

        let catalog = Catalog::for_workspace_and_persona(
            &workspace,
            &home,
            Some(("absent", PersonaKind::Bot)),
        );

        assert!(catalog.diagnostics_at(DiagnosticLevel::Error).is_empty());
        assert_eq!(
            catalog.resolve("shared").expect("shared").scope,
            Scope::User
        );
        assert!(
            catalog
                .list()
                .iter()
                .all(|summary| summary.source.persona().is_none())
        );
        assert_eq!(catalog.stats.sources, 1);
        assert!(catalog.resolve("missing").is_none());
    }
}
