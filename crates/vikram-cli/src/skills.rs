//! `vikram skills` — the operator surface for skill discovery, authoring, and installs.
//!
//! Every operation delegates to `vikram_skills`; nothing here re-implements discovery,
//! parsing, or validation. Operations that cannot be undone ask before they run.

use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use vikram_runtime::config::Config;
use vikram_skills::{
    ActivationPolicy, Budget, BundleIndex, Catalog, Diagnostic, DiagnosticLevel, DiscoveryOptions,
    History, InstallSource, Installer, Loader, Manager, Manifest, ResourceKind, Scope,
    SkillSummary, TrustPolicy, bundles_dir, home, project_skill_dirs, state_dir, user_skills_dir,
};

const NAME_WIDTH: usize = 26;
const SCOPE_WIDTH: usize = 9;
const TRUST_WIDTH: usize = 15;
const VERSION_WIDTH: usize = 9;
const ENABLED_WIDTH: usize = 7;
const DESCRIPTION_WIDTH: usize = 40;
const BUNDLE_DESCRIPTION_WIDTH: usize = 48;

#[derive(Debug, Subcommand)]
pub enum SkillsAction {
    /// List the resolved catalog (default).
    List(ListArgs),
    /// Search names, descriptions, tags, and categories.
    Search {
        /// Terms that must all match.
        query: String,
    },
    /// Show one skill in full.
    Show {
        name: String,
        /// Instruction body lines to print; 0 prints all.
        #[arg(long, default_value_t = 40)]
        lines: usize,
    },
    /// Validate one skill directory or the whole catalog.
    Validate {
        /// Skill directory. Omit to validate the catalog.
        path: Option<PathBuf>,
    },
    /// Create a skill.
    Create {
        name: String,
        /// One-line description for the frontmatter.
        #[arg(long)]
        description: String,
        /// Copy an existing skill directory instead of starting empty.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Replace a skill's instruction body, via $EDITOR or a file.
    Edit {
        name: String,
        /// Read the new body from this file instead of opening $EDITOR.
        #[arg(long)]
        body: Option<PathBuf>,
    },
    /// Delete a skill (it moves to the trash directory).
    Delete {
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Enable a skill.
    Enable { name: String },
    /// Disable a skill without deleting it.
    Disable { name: String },
    /// Copy a skill under a new name.
    Duplicate {
        name: String,
        #[arg(value_name = "AS")]
        as_name: String,
    },
    /// Export a skill directory to a destination.
    Export { name: String, dest: PathBuf },
    /// Import a skill directory into the user skills directory.
    Import { path: PathBuf },
    /// Install a skill from a local directory or a git repository.
    Install {
        /// Local directory, or the repository URL with --git.
        source: String,
        /// Treat the source as a git URL.
        #[arg(long)]
        git: bool,
        /// Git revision to check out.
        #[arg(long)]
        rev: Option<String>,
        /// Subdirectory inside the repository that holds the skill.
        #[arg(long)]
        subdir: Option<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// List bundles.
    Bundles,
    /// Inspect one bundle.
    Bundle {
        #[command(subcommand)]
        action: BundleAction,
    },
    /// Show the recorded provenance of a skill.
    History { name: String },
    /// Restore the previous recorded version of a skill.
    Rollback {
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Report where skills live and whether the catalog is healthy.
    Doctor,
}

#[derive(Debug, Subcommand)]
pub enum BundleAction {
    /// Show a bundle's members and any missing ones.
    Show { name: String },
}

#[derive(Debug, Args, Default)]
pub struct ListArgs {
    /// Emit JSON instead of a table.
    #[arg(long)]
    json: bool,
    /// Include every candidate, not just the winning copy of each name.
    #[arg(long)]
    all: bool,
    /// Restrict to one scope.
    #[arg(long, value_enum)]
    scope: Option<ScopeArg>,
    /// Filter by name, description, tag, or category.
    #[arg(long)]
    search: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScopeArg {
    Builtin,
    User,
    Project,
    Imported,
    External,
}

impl ScopeArg {
    fn to_scope(self) -> Scope {
        match self {
            ScopeArg::Builtin => Scope::Builtin,
            ScopeArg::User => Scope::User,
            ScopeArg::Project => Scope::Project,
            ScopeArg::Imported => Scope::Imported,
            ScopeArg::External => Scope::External,
        }
    }
}

/// One row of the catalog table, and the body of `skills list --json`.
#[derive(Debug, Serialize)]
pub struct SkillRow {
    pub name: String,
    pub scope: String,
    pub trust: String,
    pub compat: String,
    pub version: Option<String>,
    pub enabled: bool,
    pub valid: bool,
    pub description: String,
    pub dir: String,
}

/// The plain fields a row needs, so the row can be built without a `SkillSummary`.
struct SkillFacts<'a> {
    name: &'a str,
    description: &'a str,
    scope: &'a str,
    trust: &'a str,
    compat: &'a str,
    version: Option<&'a str>,
    enabled: bool,
    valid: bool,
    dir: &'a str,
}

impl SkillRow {
    fn from_facts(facts: SkillFacts<'_>) -> SkillRow {
        SkillRow {
            name: facts.name.to_owned(),
            scope: facts.scope.to_owned(),
            trust: facts.trust.to_owned(),
            compat: facts.compat.to_owned(),
            version: facts.version.map(|value| value.to_owned()),
            enabled: facts.enabled,
            valid: facts.valid,
            description: facts.description.to_owned(),
            dir: facts.dir.to_owned(),
        }
    }

    fn from_summary(summary: &SkillSummary) -> SkillRow {
        let name = summary.name.to_string();
        let dir = summary.dir.display().to_string();
        SkillRow::from_facts(SkillFacts {
            name: &name,
            description: &summary.description,
            scope: summary.scope.label(),
            trust: summary.trust.label(),
            compat: summary.compat.label(),
            version: summary.version.as_deref(),
            enabled: summary.enabled,
            valid: summary.valid,
            dir: &dir,
        })
    }
}

pub fn run(action: SkillsAction, config: &Config) -> Result<(), String> {
    match action {
        SkillsAction::List(args) => list(args, config),
        SkillsAction::Search { query } => search(&query, config),
        SkillsAction::Show { name, lines } => show(&name, lines, config),
        SkillsAction::Validate { path } => validate(path, config),
        SkillsAction::Create {
            name,
            description,
            from,
        } => create(&name, &description, from),
        SkillsAction::Edit { name, body } => edit(&name, body, config),
        SkillsAction::Delete { name, yes } => delete(&name, yes, config),
        SkillsAction::Enable { name } => set_enabled(&name, true),
        SkillsAction::Disable { name } => set_enabled(&name, false),
        SkillsAction::Duplicate { name, as_name } => duplicate(&name, &as_name),
        SkillsAction::Export { name, dest } => export(&name, &dest),
        SkillsAction::Import { path } => import(&path),
        SkillsAction::Install {
            source,
            git,
            rev,
            subdir,
            yes,
        } => install(source, git, rev, subdir, yes),
        SkillsAction::Bundles => bundles(),
        SkillsAction::Bundle { action } => bundle(action, config),
        SkillsAction::History { name } => history(&name, config),
        SkillsAction::Rollback { name, yes } => rollback(&name, yes, config),
        SkillsAction::Doctor => doctor(config),
    }
}

fn list(args: ListArgs, config: &Config) -> Result<(), String> {
    let catalog = Catalog::discover(&discovery_options(config));
    let mut candidates = candidates(&catalog, args.all);

    if let Some(scope) = args.scope {
        let wanted = scope.to_scope().label();
        candidates.retain(|summary| summary.scope.label() == wanted);
    }
    if let Some(query) = args.search.as_deref() {
        let matched: Vec<String> = catalog
            .search(query)
            .into_iter()
            .map(|summary| summary.name.to_string())
            .collect();
        candidates.retain(|summary| matched.contains(&summary.name.to_string()));
    }

    let rows: Vec<SkillRow> = candidates.into_iter().map(SkillRow::from_summary).collect();
    if args.json {
        let text = serde_json::to_string_pretty(&rows)
            .map_err(|err| format!("cannot render json: {err}"))?;
        println!("{text}");
    } else {
        print_rows(&rows);
    }
    Ok(())
}

fn search(query: &str, config: &Config) -> Result<(), String> {
    let catalog = Catalog::discover(&discovery_options(config));
    let rows: Vec<SkillRow> = catalog
        .search(query)
        .into_iter()
        .map(SkillRow::from_summary)
        .collect();
    print_rows(&rows);
    Ok(())
}

fn show(name: &str, lines: usize, config: &Config) -> Result<(), String> {
    let catalog = Catalog::discover(&discovery_options(config));
    let summary = find(&catalog, name)?;
    let loader = Loader::new(budget());
    let loaded = loader.load(summary).map_err(|err| err.to_string())?;
    let skill = &loaded.skill;
    let meta = &skill.metadata;

    println!("name:          {}", meta.name);
    println!("description:   {}", meta.description);
    println!(
        "version:       {}",
        optional(meta.vikram.version.as_deref())
    );
    println!("scope:         {}", skill.summary.scope.label());
    println!("compat:        {}", skill.summary.compat.label());
    println!("trust:         {}", skill.summary.trust.label());
    println!("enabled:       {}", yes_no(skill.summary.enabled));
    println!("valid:         {}", yes_no(skill.summary.valid));
    println!("source:        {}", skill.summary.dir.display());
    println!("license:       {}", optional(meta.license.as_deref()));
    println!("compatibility: {}", optional(meta.compatibility.as_deref()));
    println!(
        "min vikram:    {}",
        optional(meta.vikram.min_vikram.as_deref())
    );
    println!("author:        {}", optional(meta.vikram.author.as_deref()));
    println!(
        "activation:    {}",
        activation_label(&meta.vikram.activation)
    );
    print_list("allowed tools", &meta.allowed_tools);
    print_list("requires", &meta.vikram.requires);
    print_list("recommends", &meta.vikram.recommends);
    print_list("permissions", &meta.vikram.permissions);
    print_list("categories", &meta.vikram.categories);
    print_list("tags", &meta.vikram.tags);
    if !meta.metadata.is_empty() {
        println!("labels:");
        for (key, value) in &meta.metadata {
            println!("  {key}: {value}");
        }
    }
    if !meta.unknown.is_empty() {
        let keys: Vec<&str> = meta.unknown.iter().map(|(key, _)| key.as_str()).collect();
        println!("unknown keys:  {}", keys.join(", "));
    }

    if skill.resources.is_empty() {
        println!("resources:     none");
    } else {
        println!("resources:");
        for resource in &skill.resources {
            println!(
                "  {:<12} {:>8} B  {}",
                resource_label(&resource.kind),
                resource.bytes,
                resource.rel
            );
        }
    }

    let (body, total) = first_lines(&loaded.instructions, lines);
    if total == 0 {
        println!("instructions:  empty");
        return Ok(());
    }
    if lines != 0 && total > lines {
        println!("instructions (first {lines} of {total} lines):");
    } else {
        println!("instructions ({total} lines):");
    }
    for line in body.lines() {
        println!("  {line}");
    }
    if lines != 0 && total > lines {
        println!("  ... ({} more lines, --lines 0 prints all)", total - lines);
    }
    if loaded.truncated {
        println!("note: the loader budget truncated the instruction body");
    }
    Ok(())
}

fn validate(path: Option<PathBuf>, config: &Config) -> Result<(), String> {
    let diagnostics = match path {
        Some(path) => Manager::new()
            .validate(&path)
            .map_err(|err| err.to_string())?,
        None => Catalog::discover(&discovery_options(config)).diagnostics,
    };
    let errors = print_diagnostics(&diagnostics);
    if errors > 0 {
        return Err(format!("{errors} error-level diagnostic(s)"));
    }
    Ok(())
}

fn create(name: &str, description: &str, from: Option<PathBuf>) -> Result<(), String> {
    let manager = Manager::new();
    let path = match from {
        Some(source) => manager.create_from(name, &source),
        None => manager.create(name, description, ""),
    }
    .map_err(|err| err.to_string())?;
    println!("created {name} at {}", path.display());
    Ok(())
}

fn edit(name: &str, body: Option<PathBuf>, config: &Config) -> Result<(), String> {
    let catalog = Catalog::discover(&discovery_options(config));
    let summary = find(&catalog, name)?;
    let loader = Loader::new(budget());
    let loaded = loader.load(summary).map_err(|err| err.to_string())?;

    let text = match body {
        Some(path) => std::fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?,
        None => {
            if loaded.truncated {
                return Err(format!(
                    "the loaded body of {name} is truncated by the budget; edit {} directly or use --body",
                    summary.dir.join("SKILL.md").display()
                ));
            }
            edit_in_editor(name, &loaded.instructions)?
        }
    };
    Manager::new()
        .edit(name, &text)
        .map_err(|err| err.to_string())?;
    println!("updated {name} at {}", summary.dir.display());
    Ok(())
}

fn delete(name: &str, yes: bool, config: &Config) -> Result<(), String> {
    let catalog = Catalog::discover(&discovery_options(config));
    let summary = find(&catalog, name)?;
    if !yes {
        confirm(&format!(
            "delete skill `{name}` at {}? [y/N] ",
            summary.dir.display()
        ))?;
    }
    Manager::new().delete(name).map_err(|err| err.to_string())?;
    println!("deleted {name}");
    Ok(())
}

fn set_enabled(name: &str, enabled: bool) -> Result<(), String> {
    let manager = Manager::new();
    if enabled {
        manager.enable(name).map_err(|err| err.to_string())?;
        println!("enabled {name}");
    } else {
        manager.disable(name).map_err(|err| err.to_string())?;
        println!("disabled {name}");
    }
    Ok(())
}

fn duplicate(name: &str, as_name: &str) -> Result<(), String> {
    let path = Manager::new()
        .duplicate(name, as_name)
        .map_err(|err| err.to_string())?;
    println!("duplicated {name} as {as_name} at {}", path.display());
    Ok(())
}

fn export(name: &str, dest: &Path) -> Result<(), String> {
    let path = Manager::new()
        .export(name, dest)
        .map_err(|err| err.to_string())?;
    println!("exported {name} to {}", path.display());
    Ok(())
}

fn import(path: &Path) -> Result<(), String> {
    let imported = Manager::new().import(path).map_err(|err| err.to_string())?;
    println!("imported {imported}");
    Ok(())
}

fn install(
    source: String,
    git: bool,
    rev: Option<String>,
    subdir: Option<String>,
    yes: bool,
) -> Result<(), String> {
    let mut allow_network = false;
    let install_source = if git {
        println!("source:      {source}");
        if let Some(rev) = rev.as_deref() {
            println!("rev:         {rev}");
        }
        if let Some(subdir) = subdir.as_deref() {
            println!("subdir:      {subdir}");
        }
        println!(
            "this clones over the network into {}",
            user_skills_dir().display()
        );
        if !yes {
            confirm("continue with the network install? [y/N] ")?;
        }
        allow_network = true;
        InstallSource::Git {
            url: source,
            rev,
            subdir,
        }
    } else {
        InstallSource::Directory(PathBuf::from(&source))
    };

    let installer = Installer {
        manager: Manager::new(),
        allow_network,
    };
    let outcome = installer
        .install(&install_source)
        .map_err(|err| err.to_string())?;
    println!("installed {}", outcome.name);
    println!("  source:   {}", outcome.source);
    println!("  rev:      {}", optional(outcome.rev.as_deref()));
    println!("  hash:     {}", outcome.content_hash);
    println!("  path:     {}", outcome.path.display());
    println!("  replaced: {}", yes_no(outcome.replaced));
    Ok(())
}

fn bundles() -> Result<(), String> {
    let index = BundleIndex::load();
    if index.bundles.is_empty() {
        println!("no bundles in {}", bundles_dir().display());
    } else {
        println!("{:<24} {:>6} {:>5}  DESCRIPTION", "NAME", "SKILLS", "OPT");
        for (name, bundle) in &index.bundles {
            println!(
                "{:<24} {:>6} {:>5}  {}",
                name,
                bundle.skills.len(),
                bundle.optional.len(),
                truncate(&bundle.description, BUNDLE_DESCRIPTION_WIDTH)
            );
        }
    }
    let errors = print_diagnostics(&index.diagnostics);
    if errors > 0 {
        return Err(format!("{errors} error-level bundle diagnostic(s)"));
    }
    Ok(())
}

fn bundle(action: BundleAction, config: &Config) -> Result<(), String> {
    match action {
        BundleAction::Show { name } => {
            let index = BundleIndex::load();
            let bundle = index.bundles.get(&name).ok_or_else(|| {
                format!("no bundle named `{name}` in {}", bundles_dir().display())
            })?;
            let catalog = Catalog::discover(&discovery_options(config));
            println!("bundle:      {}", bundle.name);
            println!("description: {}", bundle.description);
            println!("path:        {}", bundle.path.display());

            let mut missing = 0;
            println!("members:");
            for member in &bundle.skills {
                let member = member.to_string();
                match catalog.resolve(&member) {
                    Some(summary) => println!("  {:<24} {}", member, summary.dir.display()),
                    None => {
                        missing += 1;
                        println!("  {:<24} MISSING", member);
                    }
                }
            }
            if !bundle.optional.is_empty() {
                println!("optional:");
                for member in &bundle.optional {
                    let member = member.to_string();
                    let installed = catalog.resolve(&member).is_some();
                    println!(
                        "  {:<24} {}",
                        member,
                        if installed {
                            "installed"
                        } else {
                            "not installed"
                        }
                    );
                }
            }
            match index.resolve(&name) {
                Ok(members) => println!("resolved: {} required member(s)", members.len()),
                Err(err) => println!("resolve: {err}"),
            }
            if missing > 0 {
                return Err(format!("{missing} bundle member(s) missing"));
            }
        }
    }
    Ok(())
}

fn history(name: &str, config: &Config) -> Result<(), String> {
    let catalog = Catalog::discover(&discovery_options(config));
    let summary = find(&catalog, name)?;
    let history = match History::load(&summary.name) {
        Ok(history) => history,
        Err(err) => {
            println!("no recorded history for {name} ({err})");
            return Ok(());
        }
    };
    match history.latest() {
        Some(manifest) => print_manifest(manifest),
        None => println!("no recorded history for {name}"),
    }
    let changed = history
        .detect_local_changes(&summary.dir)
        .map_err(|err| err.to_string())?;
    println!("local changes: {}", yes_no(changed));
    Ok(())
}

fn rollback(name: &str, yes: bool, config: &Config) -> Result<(), String> {
    let catalog = Catalog::discover(&discovery_options(config));
    let summary = find(&catalog, name)?;
    let history = History::load(&summary.name).map_err(|err| err.to_string())?;
    if history.latest().is_none() {
        return Err(format!("no recorded history for {name}"));
    }
    if history
        .detect_local_changes(&summary.dir)
        .map_err(|err| err.to_string())?
    {
        println!("warning: local changes to {name} will be replaced");
    }
    if !yes {
        confirm(&format!(
            "roll back skill `{name}` at {}? [y/N] ",
            summary.dir.display()
        ))?;
    }
    let manifest = history
        .rollback(&summary.dir)
        .map_err(|err| err.to_string())?;
    println!("rolled back {name}");
    print_manifest(&manifest);
    Ok(())
}

fn doctor(config: &Config) -> Result<(), String> {
    println!("VIKRAM skills doctor");
    println!("home: {}", home().display());
    println!("state: {}", state_dir().display());

    let mut problems = Vec::new();
    let skills_dir = user_skills_dir();
    match dir_state(&skills_dir) {
        Ok(()) => println!("  ok   user skills dir {}", skills_dir.display()),
        Err(err) => {
            println!("  fail {err}");
            problems.push(err);
        }
    }

    let workspace = workspace_dir(config);
    let project_dirs: Vec<_> = project_skill_dirs(&workspace)
        .into_iter()
        .filter(|(_, _, path)| path.is_dir())
        .collect();
    if project_dirs.is_empty() {
        println!(
            "  info no project skill dirs found under {}",
            workspace.display()
        );
    } else {
        for (scope, compat, path) in project_dirs {
            println!(
                "  info {:<9} {:<13} {}",
                scope.label(),
                compat.label(),
                path.display()
            );
        }
    }

    let catalog = Catalog::discover(&discovery_options(config));
    println!(
        "catalog: {} source(s), {} dir(s) scanned, {} skill(s), {} duplicate(s), {} invalid, {} disabled",
        catalog.stats.sources,
        catalog.stats.dirs_scanned,
        catalog.stats.skills,
        catalog.stats.duplicates,
        catalog.stats.invalid,
        catalog.stats.disabled
    );
    let errors = print_diagnostics(&catalog.diagnostics);
    if errors > 0 {
        problems.push(format!("{errors} error-level diagnostic(s)"));
    }

    let index = BundleIndex::load();
    println!(
        "bundles: {} in {}",
        index.bundles.len(),
        bundles_dir().display()
    );

    let trust = TrustPolicy::load();
    println!("trust: project trusted: {}", yes_no(trust.project_trusted));
    if !trust.trusted_sources.is_empty() {
        println!(
            "       trusted sources: {}",
            trust.trusted_sources.join(", ")
        );
    }
    if !trust.deny.is_empty() {
        println!("       denied: {}", trust.deny.join(", "));
    }

    if problems.is_empty() {
        println!("skills doctor: no blocking problems");
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

fn discovery_options(config: &Config) -> DiscoveryOptions {
    DiscoveryOptions {
        workspace: Some(workspace_dir(config)),
        ..DiscoveryOptions::default()
    }
}

fn budget() -> Budget {
    Budget::default()
}

fn workspace_dir(config: &Config) -> PathBuf {
    config
        .workspace_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn candidates(catalog: &Catalog, all: bool) -> Vec<&SkillSummary> {
    if all {
        catalog
            .entries
            .values()
            .flat_map(|entries| entries.iter())
            .collect()
    } else {
        catalog.list()
    }
}

fn find<'a>(catalog: &'a Catalog, name: &str) -> Result<&'a SkillSummary, String> {
    catalog.resolve(name).ok_or_else(|| {
        format!(
            "no skill named `{name}` ({} skill(s) in catalog; try `vikram skills list`)",
            catalog.list().len()
        )
    })
}

fn print_rows(rows: &[SkillRow]) {
    if rows.is_empty() {
        println!("no skills matched");
        return;
    }
    println!("{}", header_line());
    for row in rows {
        println!("{}", row_line(row));
    }
}

fn header_line() -> String {
    format!(
        "{:<NAME_WIDTH$} {:<SCOPE_WIDTH$} {:<TRUST_WIDTH$} {:<VERSION_WIDTH$} {:<ENABLED_WIDTH$} {}",
        "NAME", "SCOPE", "TRUST", "VERSION", "ENABLED", "DESCRIPTION"
    )
}

fn row_line(row: &SkillRow) -> String {
    format!(
        "{:<NAME_WIDTH$} {:<SCOPE_WIDTH$} {:<TRUST_WIDTH$} {:<VERSION_WIDTH$} {:<ENABLED_WIDTH$} {}",
        truncate(&row.name, NAME_WIDTH),
        row.scope,
        row.trust,
        optional(row.version.as_deref()),
        yes_no(row.enabled),
        truncate(&row.description, DESCRIPTION_WIDTH)
    )
}

fn print_diagnostics(diagnostics: &[Diagnostic]) -> usize {
    let mut errors = 0;
    for diagnostic in diagnostics {
        if matches!(diagnostic.level, DiagnosticLevel::Error) {
            errors += 1;
        }
        println!(
            "{:<7} {}  {}",
            level_label(&diagnostic.level),
            diagnostic.path.display(),
            diagnostic.message
        );
    }
    if diagnostics.is_empty() {
        println!("no diagnostics");
    }
    errors
}

fn print_manifest(manifest: &Manifest) {
    println!("  version:  {}", optional(manifest.version.as_deref()));
    println!("  hash:     {}", manifest.content_hash);
    println!("  source:   {}", optional(manifest.source.as_deref()));
    println!("  rev:      {}", optional(manifest.rev.as_deref()));
    println!("  recorded: {}", crate::format_date(manifest.installed_ms));
    println!("  files:    {}", manifest.files.len());
}

fn print_list(label: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    println!("{label}:");
    for value in values {
        println!("  - {value}");
    }
}

fn edit_in_editor(name: &str, current: &str) -> Result<String, String> {
    let editor = std::env::var("EDITOR")
        .map_err(|_| "EDITOR is not set; pass --body <file> instead".to_owned())?;
    if editor.trim().is_empty() {
        return Err("EDITOR is empty; pass --body <file> instead".to_owned());
    }
    let path = std::env::temp_dir().join(format!(
        "vikram-skill-{}-{}.md",
        sanitize(name),
        std::process::id()
    ));
    std::fs::write(&path, current)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;

    let mut parts = editor.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| "EDITOR is empty; pass --body <file> instead".to_owned())?;
    let mut command = std::process::Command::new(program);
    for arg in parts {
        command.arg(arg);
    }
    let status = command.arg(&path).status();
    let result = match status {
        Ok(status) if status.success() => std::fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display())),
        Ok(status) => Err(format!("{program} exited with {status}")),
        Err(err) => Err(format!("cannot run {program}: {err}")),
    };
    let _ = std::fs::remove_file(&path);
    result
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

fn dir_state(dir: &Path) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("skills dir is missing: {}", dir.display()));
    }
    let probe = dir.join(format!(".vikram-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"ok")
        .map_err(|err| format!("skills dir is not writable: {} ({err})", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    if max <= 3 {
        return text.chars().take(max).collect();
    }
    let mut out: String = text.chars().take(max - 3).collect();
    out.push_str("...");
    out
}

fn first_lines(text: &str, max: usize) -> (String, usize) {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if max == 0 || total <= max {
        return (lines.join("\n"), total);
    }
    (lines[..max].join("\n"), total)
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn level_label(level: &DiagnosticLevel) -> &'static str {
    match level {
        DiagnosticLevel::Info => "info",
        DiagnosticLevel::Warning => "warn",
        DiagnosticLevel::Error => "error",
    }
}

fn activation_label(policy: &ActivationPolicy) -> &'static str {
    match policy {
        ActivationPolicy::Auto => "auto",
        ActivationPolicy::Suggested => "suggested",
        ActivationPolicy::Manual => "manual",
    }
}

fn resource_label(kind: &ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Script => "script",
        ResourceKind::Reference => "reference",
        ResourceKind::Asset => "asset",
        ResourceKind::Template => "template",
        ResourceKind::AgentConfig => "agent",
        ResourceKind::Other => "other",
    }
}

fn optional(value: Option<&str>) -> &str {
    value.unwrap_or("-")
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(description: &'a str) -> SkillFacts<'a> {
        SkillFacts {
            name: "demo",
            description,
            scope: "user",
            trust: "verified",
            compat: "native",
            version: Some("1.2.0"),
            enabled: true,
            valid: true,
            dir: "/home/tester/.vikram/skills/demo",
        }
    }

    #[test]
    fn row_keeps_full_description_for_json() {
        let long = "a".repeat(120);
        let row = SkillRow::from_facts(facts(&long));
        assert_eq!(row.name, "demo");
        assert_eq!(row.scope, "user");
        assert_eq!(row.version.as_deref(), Some("1.2.0"));
        assert_eq!(row.description.chars().count(), 120);
        assert!(row.enabled);
        assert_eq!(row.dir, "/home/tester/.vikram/skills/demo");
    }

    #[test]
    fn row_without_version_renders_a_dash() {
        let mut facts = facts("short");
        facts.version = None;
        let row = SkillRow::from_facts(facts);
        assert!(row.version.is_none());
        assert!(row_line(&row).contains(" - "));
    }

    #[test]
    fn row_line_truncates_the_description_column() {
        let row = SkillRow::from_facts(facts(&"b".repeat(200)));
        let line = row_line(&row);
        assert!(line.contains("..."));
        assert!(!line.contains(&"b".repeat(DESCRIPTION_WIDTH + 1)));
        assert!(
            line.len()
                <= NAME_WIDTH
                    + SCOPE_WIDTH
                    + TRUST_WIDTH
                    + VERSION_WIDTH
                    + ENABLED_WIDTH
                    + DESCRIPTION_WIDTH
                    + 5
        );
    }

    #[test]
    fn header_line_names_every_column() {
        let header = header_line();
        for column in [
            "NAME",
            "SCOPE",
            "TRUST",
            "VERSION",
            "ENABLED",
            "DESCRIPTION",
        ] {
            assert!(header.contains(column), "{header} lacks {column}");
        }
    }

    #[test]
    fn truncate_respects_the_limit_in_characters() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactlyten", 10), "exactlyten");
        assert_eq!(truncate("abcdefghijklmnop", 10), "abcdefg...");
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("abcdef", 0), "");
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        assert_eq!(truncate("ünïcødé", 7), "ünïcødé");
        assert_eq!(truncate("ünïcødé", 4), "ü...");
        assert_eq!(truncate("ünïcøødé", 5), "ün...");
    }

    #[test]
    fn first_lines_keeps_whole_lines() {
        let text = "one\ntwo\nthree\nfour";
        assert_eq!(first_lines(text, 2), ("one\ntwo".to_owned(), 4));
        assert_eq!(first_lines(text, 0), (text.to_owned(), 4));
        assert_eq!(first_lines(text, 9), (text.to_owned(), 4));
        assert_eq!(first_lines("", 5), (String::new(), 0));
    }

    #[test]
    fn sanitize_keeps_the_temp_file_name_flat() {
        assert_eq!(sanitize("demo-skill_1"), "demo-skill_1");
        assert_eq!(sanitize("../../etc/passwd"), "______etc_passwd");
    }

    #[test]
    fn scope_argument_covers_every_scope_label() {
        for (scope, label) in [
            (ScopeArg::Builtin, "builtin"),
            (ScopeArg::User, "user"),
            (ScopeArg::Project, "project"),
            (ScopeArg::Imported, "imported"),
            (ScopeArg::External, "external"),
        ] {
            assert_eq!(scope.to_scope().label(), label);
        }
    }

    #[test]
    fn yes_no_stays_ascii() {
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
        assert_eq!(optional(None), "-");
        assert_eq!(optional(Some("x")), "x");
    }
}
