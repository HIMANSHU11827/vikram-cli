//! `vikram module` — scaffold, validate, and pack module/plugin projects.
//!
//! This is the authoring side of the plugin SDK: `init` writes a directory that already
//! satisfies the runtime's manifest rules, `validate` applies the same checks an install
//! would (plus the permission vocabulary), and `pack` produces a `.tar.gz` an operator can
//! extract and hand to `vikram plugins install` / `vikram skills install`. There is no
//! network and no registry upload — packages are directories.

use std::path::{Component, Path, PathBuf};

use clap::{Subcommand, ValueEnum};
use vikram_runtime::context::ContextFile;
use vikram_runtime::hooks::HookPoint;
use vikram_runtime::plugins::{
    PLUGIN_COMPONENT_TYPES, PluginManifest, is_semver, parse_permission,
};
use vikram_store::EntityKind;

#[derive(Debug, Subcommand)]
pub enum ModuleAction {
    /// Scaffold a new module project into a directory.
    Init {
        /// Module name; the directory and plugin id are its kebab-case form.
        name: String,
        /// What to scaffold: a process-hook plugin, a skill collection, or an agent definition.
        #[arg(long, value_enum, default_value_t = Kind::Plugin)]
        kind: Kind,
        /// Parent directory to create the module in (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Validate a module/package directory: manifest, entry, permissions, skills, entity files.
    Validate {
        /// The module directory to check.
        path: PathBuf,
    },
    /// Pack a validated module directory into a deterministic `.tar.gz`.
    Pack {
        /// The module directory to pack.
        path: PathBuf,
        /// Output archive path (default: `<name>-<version>.tar.gz` here).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    /// A process-hook plugin: `plugin.json` + hook entry script + README.
    Plugin,
    /// A collection of skills: one `<skill>/SKILL.md` per subdirectory.
    SkillPack,
    /// An agent/bot definition: the SOUL/INSTRUCTIONS/MEMORY/TOOLS context files the runtime
    /// loads for an entity, plus a template `entity.example.json`.
    AgentPack,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Plugin => "plugin",
            Kind::SkillPack => "skill-pack",
            Kind::AgentPack => "agent-pack",
        }
    }
}

pub fn run(action: ModuleAction) -> Result<(), String> {
    match action {
        ModuleAction::Init { name, kind, dir } => init(&name, kind, dir),
        ModuleAction::Validate { path } => validate(&path),
        ModuleAction::Pack { path, out } => pack(&path, out),
    }
}

fn init(name: &str, kind: Kind, dir: Option<PathBuf>) -> Result<(), String> {
    let slug = slugify(name);
    if slug.is_empty() {
        return Err(format!(
            "`{name}` has no usable characters; use letters, digits, '-', '_' or spaces"
        ));
    }
    let root = dir.unwrap_or_else(|| PathBuf::from("."));
    let target = root.join(&slug);
    if target.exists()
        && std::fs::read_dir(&target)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
    {
        return Err(format!(
            "{} already exists and is not empty",
            target.display()
        ));
    }
    std::fs::create_dir_all(&target)
        .map_err(|e| format!("cannot create {}: {e}", target.display()))?;
    match kind {
        Kind::Plugin => scaffold_plugin(&target, name.trim(), &slug)?,
        Kind::SkillPack => scaffold_skill_pack(&target, &slug)?,
        Kind::AgentPack => scaffold_agent_pack(&target, &slug)?,
    }
    println!("created {} module at {}", kind.label(), target.display());
    println!("next: vikram module validate {}", target.display());
    Ok(())
}

/// Lowercase kebab-case: the only spelling every consumer (plugin id, skill name, entity
/// slug) accepts without complaint.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn write(path: &Path, content: &str) -> Result<(), String> {
    std::fs::write(path, content).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn scaffold_plugin(dir: &Path, name: &str, slug: &str) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(&serde_json::json!({
        "id": slug,
        "name": name,
        "version": "0.1.0",
        "description": "TODO: one line on what this plugin does",
        "author": "",
        "license": "",
        "components": { "hooks": [HookPoint::TurnStart.as_str()] },
        "permissions": ["agents", "process"],
        "entry": "hook.sh",
        "enabled": true,
        "priority": 0,
    }))
    .map_err(|e| e.to_string())?;
    // Prove the scaffold round-trips through the same serde shape the runtime reads.
    PluginManifest::parse(&raw).map_err(|e| format!("generated manifest does not parse: {e}"))?;
    write(&dir.join("plugin.json"), &format!("{raw}\n"))?;
    write(
        &dir.join("hook.sh"),
        "#!/bin/sh\n\
         # Process hook: $1 is the hook point, the context JSON arrives on stdin.\n\
         # Print exactly one JSON object; {} allows the operation unchanged.\n\
         cat >/dev/null\n\
         printf '{}\\n'\n",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            std::fs::set_permissions(dir.join("hook.sh"), std::fs::Permissions::from_mode(0o755));
    }
    write(&dir.join("README.md"), PLUGIN_README)?;
    Ok(())
}

fn scaffold_skill_pack(dir: &Path, slug: &str) -> Result<(), String> {
    let skill = dir.join(slug);
    std::fs::create_dir_all(&skill)
        .map_err(|e| format!("cannot create {}: {e}", skill.display()))?;
    write(
        &skill.join("SKILL.md"),
        &format!(
            "---\nname: {slug}\ndescription: TODO one-line description of when to use this skill\n---\n\n\
             # {slug}\n\nInstructions the model follows when this skill activates.\n"
        ),
    )?;
    write(
        &dir.join("README.md"),
        "# Skill pack\n\n\
         Every immediate subdirectory containing a `SKILL.md` is one skill.\n\n\
         Install a single skill:\n\n    vikram skills install <this-dir>/<skill-name>\n\n\
         or pack the directory and install every skill in it:\n\n\
         vikram module pack <this-dir>\n    vikram skills install <name>-<version>.tar.gz\n\n\
         or copy a skill directory into the user skills directory\n\
         (`vikram skills doctor` shows where that is).\n",
    )?;
    Ok(())
}

fn scaffold_agent_pack(dir: &Path, slug: &str) -> Result<(), String> {
    let files: [(ContextFile, String); 4] = [
        (
            ContextFile::Soul,
            format!(
                "# SOUL\n\nIdentity, values and way of speaking for {slug}. Write it in the \
                 operator's own words; an empty file asserts nothing.\n"
            ),
        ),
        (
            ContextFile::Instructions,
            format!("# INSTRUCTIONS\n\nResponsibilities and operating rules for {slug}.\n"),
        ),
        (
            ContextFile::Memory,
            "# MEMORY\n\nPersistent knowledge for this entity. Updated by the runtime.\n"
                .to_owned(),
        ),
        (
            ContextFile::Tools,
            "# TOOLS\n\nTool usage guidance and restrictions for this entity.\n".to_owned(),
        ),
    ];
    for (file, content) in &files {
        write(&dir.join(file.filename()), content)?;
    }
    let entity = serde_json::json!({
        "kind": "agent",
        "name": slug,
        "slug": slug,
        "title": "",
        "role": "agent",
        "specialization": "",
        "description": format!("TODO: what {slug} is for"),
        "status": "active",
    });
    write(
        &dir.join("entity.example.json"),
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&entity).map_err(|e| e.to_string())?
        ),
    )?;
    write(
        &dir.join("README.md"),
        "# Agent pack\n\n\
         These are the context files the runtime loads for an entity (SOUL.md,\n\
         INSTRUCTIONS.md, MEMORY.md, TOOLS.md — see `ContextFile` in\n\
         `crates/vikram-runtime/src/context.rs`). `entity.example.json` templates the\n\
         `entity.json` manifest `vikram entities install-pack` reads; copy it to\n\
         `entity.json` and fill in the fields (all optional — the directory name is the\n\
         default slug and kind is `agent`).\n\n\
         Install:\n\n    vikram entities install-pack <this-dir>\n\n\
         or pack first and install the archive:\n\n    vikram module pack <this-dir>\n\
         vikram entities install-pack <name>-<version>.tar.gz\n",
    )?;
    Ok(())
}

fn validate(path: &Path) -> Result<(), String> {
    let (kind, issues) = check_dir(path)?;
    for issue in &issues {
        println!("{issue}");
    }
    let errors = issues.iter().filter(|i| i.starts_with("error:")).count();
    if errors > 0 {
        return Err(format!("{}: {errors} error(s)", path.display()));
    }
    if issues.is_empty() {
        println!("{}: {} — no problems", path.display(), kind);
    } else {
        println!(
            "{}: {} — {} warning(s), no errors",
            path.display(),
            kind,
            issues.len()
        );
    }
    Ok(())
}

fn pack(path: &Path, out: Option<PathBuf>) -> Result<(), String> {
    let (kind, issues) = check_dir(path)?;
    let errors: Vec<&String> = issues.iter().filter(|i| i.starts_with("error:")).collect();
    if !errors.is_empty() {
        for issue in &issues {
            eprintln!("{issue}");
        }
        return Err(format!(
            "refusing to pack {}: {} error(s)",
            path.display(),
            errors.len()
        ));
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "module".to_owned());
    let version = PluginManifest::from_dir(path)
        .map(|m| m.version)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "0.0.0".to_owned());
    let out = out.unwrap_or_else(|| PathBuf::from(format!("{name}-{version}.tar.gz")));
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let archive_name = path
        .file_name()
        .ok_or_else(|| format!("{} has no directory name to archive", path.display()))?;
    let status = std::process::Command::new("tar")
        .args([
            "--sort=name",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--use-compress-program=gzip -n",
            "-cf",
        ])
        .arg(&out)
        .arg("-C")
        .arg(&parent)
        .arg(archive_name)
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "packing needs the `tar` executable, which was not found".to_owned()
            } else {
                format!("cannot run tar: {e}")
            }
        })?;
    if !status.success() {
        return Err(format!(
            "tar exited with {status} while writing {}",
            out.display()
        ));
    }
    println!("packed {} -> {}", path.display(), out.display());
    match kind.as_str() {
        "plugin" => println!("install: vikram plugins install {}", out.display()),
        "skill" => println!("install: vikram skills install {}", out.display()),
        _ => println!("install: vikram entities install-pack {}", out.display()),
    }
    Ok(())
}

/// The entity an agent-pack installs as, read from its optional `entity.json` manifest.
/// Every field defaults so a bare scaffolded pack installs under its directory name.
#[derive(Debug)]
pub struct AgentPackSpec {
    pub kind: EntityKind,
    pub name: String,
    pub slug: String,
    pub title: String,
    pub role: String,
    pub specialization: String,
    pub description: String,
}

/// `entity.json` in an agent-pack: the real import format `entity.example.json` templates.
/// Strict — unknown fields are refused rather than silently ignored.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityManifest {
    kind: Option<String>,
    name: Option<String>,
    slug: Option<String>,
    title: Option<String>,
    role: Option<String>,
    specialization: Option<String>,
    description: Option<String>,
}

/// Read and validate an agent-pack directory the way `vikram entities install-pack`
/// consumes it: the same checks `validate` runs (all four context files present), plus a
/// strictly-parsed `entity.json` when one exists.
pub fn agent_pack(path: &Path) -> Result<AgentPackSpec, String> {
    let (kind, issues) = check_dir(path)?;
    if kind != "agent-pack" {
        return Err(format!(
            "{} is a {kind} module, not an agent-pack",
            path.display()
        ));
    }
    let errors: Vec<&str> = issues
        .iter()
        .filter(|issue| issue.starts_with("error:"))
        .map(String::as_str)
        .collect();
    if !errors.is_empty() {
        return Err(format!("{}: {}", path.display(), errors.join("; ")));
    }
    let manifest_path = path.join("entity.json");
    let manifest: Option<EntityManifest> = if manifest_path.is_file() {
        let text = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
        Some(
            serde_json::from_str(&text)
                .map_err(|e| format!("{}: invalid entity.json: {e}", manifest_path.display()))?,
        )
    } else {
        None
    };
    let dir_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let slug = manifest
        .as_ref()
        .and_then(|m| m.slug.clone())
        .unwrap_or(dir_name);
    if slug.is_empty()
        || slug == "."
        || slug == ".."
        || slug.contains('/')
        || slug.contains('\\')
        || slug.contains('\0')
    {
        return Err(format!("`{slug}` is not a usable entity slug"));
    }
    let kind = match manifest.as_ref().and_then(|m| m.kind.as_deref()) {
        Some(raw) => EntityKind::parse(raw).map_err(|e| e.to_string())?,
        None => EntityKind::Agent,
    };
    let name = manifest
        .as_ref()
        .and_then(|m| m.name.clone())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| slug.clone());
    Ok(AgentPackSpec {
        kind,
        name,
        slug,
        title: manifest
            .as_ref()
            .and_then(|m| m.title.clone())
            .unwrap_or_default(),
        role: manifest
            .as_ref()
            .and_then(|m| m.role.clone())
            .unwrap_or_default(),
        specialization: manifest
            .as_ref()
            .and_then(|m| m.specialization.clone())
            .unwrap_or_default(),
        description: manifest
            .as_ref()
            .and_then(|m| m.description.clone())
            .unwrap_or_default(),
    })
}

/// What a directory is, and every problem found in it. `Err` means it is not a module at all.
fn check_dir(path: &Path) -> Result<(String, Vec<String>), String> {
    if !path.is_dir() {
        return Err(format!("not a directory: {}", path.display()));
    }
    if path.join("plugin.json").is_file() {
        let mut issues = Vec::new();
        check_plugin(path, &mut issues);
        return Ok(("plugin".to_owned(), issues));
    }
    if path.join("SKILL.md").is_file() {
        let mut issues = Vec::new();
        check_skill(path, &mut issues);
        return Ok(("skill".to_owned(), issues));
    }
    let skills: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|child| child.is_dir() && child.join("SKILL.md").is_file())
        .collect();
    if !skills.is_empty() {
        let mut issues = Vec::new();
        for skill in &skills {
            check_skill(skill, &mut issues);
        }
        return Ok(("skill-pack".to_owned(), issues));
    }
    let entity_files = [
        ContextFile::Soul,
        ContextFile::Instructions,
        ContextFile::Memory,
        ContextFile::Tools,
    ];
    if entity_files
        .iter()
        .any(|f| path.join(f.filename()).is_file())
    {
        let mut issues = Vec::new();
        for file in &entity_files {
            if !path.join(file.filename()).is_file() {
                issues.push(format!("error: missing {}", file.filename()));
            }
        }
        return Ok(("agent-pack".to_owned(), issues));
    }
    Err(format!(
        "not a module directory: {} has no plugin.json, SKILL.md, skill subdirectories, \
         or entity context files",
        path.display()
    ))
}

fn check_plugin(dir: &Path, issues: &mut Vec<String>) {
    let manifest = match PluginManifest::from_dir(dir) {
        Ok(manifest) => manifest,
        Err(err) => {
            issues.push(format!("error: plugin.json: {err}"));
            return;
        }
    };
    let id = manifest.id();
    if id.trim().is_empty() {
        issues.push("error: manifest declares neither `id` nor `name`".to_owned());
    } else if id == "." || id == ".." || id.contains('/') || id.contains('\\') || id.contains('\0')
    {
        issues.push(format!("error: `{id}` is not a usable plugin id"));
    }
    if manifest.version.trim().is_empty() {
        issues.push("error: manifest declares no `version`".to_owned());
    } else if !is_semver(&manifest.version) {
        issues.push(format!(
            "error: `version` `{}` is not semver (expected MAJOR.MINOR.PATCH)",
            manifest.version
        ));
    }
    if manifest.description.trim().is_empty() {
        issues.push("warning: manifest has no `description`".to_owned());
    }
    match manifest.entry_point() {
        Some(entry) => match entry_inside(dir, entry) {
            Err(reason) => issues.push(format!("error: {reason}")),
            Ok(path) if !path.exists() => issues.push(format!(
                "error: declared entry point `{entry}` does not exist"
            )),
            Ok(_) => {}
        },
        None => {
            let hooks_declared = manifest
                .components
                .as_ref()
                .and_then(|c| c.get("hooks"))
                .is_some_and(|points| !points.is_empty())
                || !manifest.hook_rules.is_empty();
            if !hooks_declared {
                issues.push(
                    "error: manifest declares no `entry` and no hooks \
                     (`components.hooks` or `hook_rules`)"
                        .to_owned(),
                );
            }
        }
    }
    for declared in &manifest.permissions {
        if parse_permission(declared).is_none() {
            issues.push(format!("error: unknown permission `{declared}`"));
        }
    }
    if let Some(components) = &manifest.components {
        for kind in components.keys() {
            if !PLUGIN_COMPONENT_TYPES.contains(&kind.trim().to_ascii_lowercase().as_str()) {
                issues.push(format!("warning: unknown component type `{kind}`"));
            }
        }
        if let Some(points) = components.get("hooks") {
            for raw in points {
                if HookPoint::parse(raw).is_none() {
                    issues.push(format!("error: unknown hook point `{raw}`"));
                }
            }
        }
    }
    for rule in &manifest.hook_rules {
        if let Err(reason) = rule.validate() {
            issues.push(format!("error: hook_rules: {reason}"));
        }
    }
}

/// Resolve a manifest `entry` the same way the runtime does: relative, inside the plugin dir.
fn entry_inside(dir: &Path, entry: &str) -> Result<PathBuf, String> {
    let entry = entry.trim();
    let inside = Path::new(entry)
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if entry.is_empty() || !inside {
        return Err(format!(
            "entry `{entry}` must stay inside the plugin directory"
        ));
    }
    Ok(dir.join(entry))
}

fn check_skill(dir: &Path, issues: &mut Vec<String>) {
    for diagnostic in vikram_skills::Manager::new()
        .validate(dir)
        .unwrap_or_else(|err| {
            issues.push(format!("error: cannot validate {}: {err}", dir.display()));
            Vec::new()
        })
    {
        let level = match diagnostic.level {
            vikram_skills::DiagnosticLevel::Error => "error",
            vikram_skills::DiagnosticLevel::Warning => "warning",
            vikram_skills::DiagnosticLevel::Info => "info",
        };
        if level == "info" {
            continue;
        }
        issues.push(format!(
            "{level}: {}: {}",
            diagnostic.path.display(),
            diagnostic.message
        ));
    }
}

const PLUGIN_README: &str = "# Plugin\n\n\
A VIKRAM process-hook plugin. `plugin.json` is the manifest; `hook.sh` is spawned once per\n\
dispatch for every hook point listed under `components.hooks`.\n\n\
## Hook protocol\n\n\
`argv[1]` is the hook point (`turn_start`, `tool_call`, ...). The context JSON —\n\
`{point, session, turn, workspace, model, text, tool, result, details}` — arrives on\n\
stdin. Reply with exactly one JSON object on stdout; `{}` allows the operation\n\
unchanged. See `HookEffect::from_json` in `crates/vikram-runtime/src/hooks.rs` for the\n\
fields (`deny`, `prepend`, `append`, `arguments`, `content`, `ok`,\n\
`max_output_tokens`, `text`). A hook that fails, times out (5s) or prints malformed\n\
JSON is skipped — the turn continues.\n\n\
## Permissions (deny-first, closed vocabulary)\n\n\
`permissions` in `plugin.json` may only name:\n\n\
- `fs.read`, `fs.write` — filesystem access\n\
- `network` — outbound connections (also allows entry execution)\n\
- `process` — spawn the entry script (required for process hooks)\n\
- `env` — inherit the full environment; without it the hook gets a safe subset\n\
- `secrets`, `skills`, `tools`, `agents`, `hive`, `bots`, `ui`, `background`\n\n\
A `:scope` suffix (`fs.read:workspace`) parses but is advisory — scopes are not\n\
enforced yet. Hook points need their surface permission: `tool_call`,\n\
`tool_result`, `memory_recall`, `memory_write` need `tools`; every other point\n\
needs `agents`. A manifest that declares nothing executable is refused at run\n\
time.\n\n\
## Install\n\n\n    vikram plugins validate .\n    vikram plugins install . --yes\n\n\
Install copies the directory into `~/.vikram/plugins/<id>`; workspace plugin\n\
directories are listed but never executed.\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_plugin_scaffolds_a_valid_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        init(
            "My Cool Plugin",
            Kind::Plugin,
            Some(tmp.path().to_path_buf()),
        )
        .unwrap();
        let dir = tmp.path().join("my-cool-plugin");
        let manifest = PluginManifest::from_dir(&dir).unwrap();
        assert_eq!(manifest.id(), "my-cool-plugin");
        assert_eq!(manifest.version, "0.1.0");
        assert!(dir.join("hook.sh").is_file());
        let (kind, issues) = check_dir(&dir).unwrap();
        assert_eq!(kind, "plugin");
        assert!(
            !issues.iter().any(|i| i.starts_with("error:")),
            "{issues:?}"
        );
    }

    #[test]
    fn init_skill_pack_and_agent_pack_validate_clean() {
        let tmp = tempfile::tempdir().unwrap();
        init("Pack One", Kind::SkillPack, Some(tmp.path().to_path_buf())).unwrap();
        let (kind, issues) = check_dir(&tmp.path().join("pack-one")).unwrap();
        assert_eq!(kind, "skill-pack");
        assert!(
            !issues.iter().any(|i| i.starts_with("error:")),
            "{issues:?}"
        );

        init("Bot Two", Kind::AgentPack, Some(tmp.path().to_path_buf())).unwrap();
        let (kind, issues) = check_dir(&tmp.path().join("bot-two")).unwrap();
        assert_eq!(kind, "agent-pack");
        assert!(
            !issues.iter().any(|i| i.starts_with("error:")),
            "{issues:?}"
        );
    }

    #[test]
    fn init_refuses_a_nonempty_target() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("taken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file"), "x").unwrap();
        assert!(init("taken", Kind::Plugin, Some(tmp.path().to_path_buf())).is_err());
    }

    #[test]
    fn validate_catches_a_bad_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bad");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), "{ not json").unwrap();
        let (_, issues) = check_dir(&dir).unwrap();
        assert!(
            issues.iter().any(|i| i.contains("plugin.json")),
            "{issues:?}"
        );
    }

    #[test]
    fn validate_catches_missing_entry_and_bad_permission() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mod");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{"id":"mod","name":"mod","version":"1.0.0","entry":"gone.sh","permissions":["fly"]}"#,
        )
        .unwrap();
        let (_, issues) = check_dir(&dir).unwrap();
        assert!(
            issues.iter().any(|i| i.contains("does not exist")),
            "{issues:?}"
        );
        assert!(
            issues.iter().any(|i| i.contains("unknown permission")),
            "{issues:?}"
        );
    }

    #[test]
    fn validate_accepts_hooks_without_an_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("rules-only");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{"id":"rules-only","name":"rules-only","version":"1.0.0","permissions":["agents"],"components":{"hooks":["turn_start"]},"hook_rules":[{"point":"turn_start","append":"hi"}]}"#,
        )
        .unwrap();
        let (_, issues) = check_dir(&dir).unwrap();
        assert!(
            !issues.iter().any(|i| i.starts_with("error:")),
            "{issues:?}"
        );
    }

    #[test]
    fn validate_rejects_an_unrecognized_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let err = check_dir(tmp.path()).unwrap_err();
        assert!(err.contains("not a module directory"), "{err}");
    }

    #[test]
    fn pack_produces_a_tarball_listing_the_module() {
        if std::process::Command::new("tar")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        init("Pack Me", Kind::Plugin, Some(tmp.path().to_path_buf())).unwrap();
        let out = tmp.path().join("pack-me.tar.gz");
        pack(&tmp.path().join("pack-me"), Some(out.clone())).unwrap();
        assert!(out.is_file());
        let listing = std::process::Command::new("tar")
            .args(["-tzf"])
            .arg(&out)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&listing.stdout);
        assert!(text.contains("pack-me/plugin.json"), "{text}");
    }

    #[test]
    fn agent_pack_defaults_to_the_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        init("Bot Three", Kind::AgentPack, Some(tmp.path().to_path_buf())).unwrap();
        let spec = agent_pack(&tmp.path().join("bot-three")).unwrap();
        assert_eq!(spec.kind, EntityKind::Agent);
        assert_eq!(spec.slug, "bot-three");
        assert_eq!(spec.name, "bot-three");
        assert!(spec.title.is_empty() && spec.role.is_empty());
    }

    #[test]
    fn agent_pack_reads_entity_json() {
        let tmp = tempfile::tempdir().unwrap();
        init("Bot Four", Kind::AgentPack, Some(tmp.path().to_path_buf())).unwrap();
        let dir = tmp.path().join("bot-four");
        std::fs::write(
            dir.join("entity.json"),
            r#"{"kind":"bot","name":"Four","slug":"four-bot","title":"Lead","role":"bot","specialization":"ops","description":"does bot things"}"#,
        )
        .unwrap();
        let spec = agent_pack(&dir).unwrap();
        assert_eq!(spec.kind, EntityKind::Bot);
        assert_eq!(spec.slug, "four-bot");
        assert_eq!(spec.name, "Four");
        assert_eq!(spec.title, "Lead");
        assert_eq!(spec.specialization, "ops");
        assert_eq!(spec.description, "does bot things");
    }

    #[test]
    fn agent_pack_rejects_bad_manifests_and_non_packs() {
        let tmp = tempfile::tempdir().unwrap();
        init("Bot Five", Kind::AgentPack, Some(tmp.path().to_path_buf())).unwrap();
        let dir = tmp.path().join("bot-five");
        // Unknown fields are refused, not ignored.
        std::fs::write(dir.join("entity.json"), r#"{"bogus":1}"#).unwrap();
        assert!(agent_pack(&dir).unwrap_err().contains("entity.json"));
        // An unknown kind is refused.
        std::fs::write(dir.join("entity.json"), r#"{"kind":"wizard"}"#).unwrap();
        assert!(agent_pack(&dir).unwrap_err().contains("entity kind"));
        // A traversal slug is refused.
        std::fs::write(dir.join("entity.json"), r#"{"slug":"../x"}"#).unwrap();
        assert!(agent_pack(&dir).unwrap_err().contains("slug"));
        // A plugin directory is not an agent-pack.
        init("Not A Pack", Kind::Plugin, Some(tmp.path().to_path_buf())).unwrap();
        let err = agent_pack(&tmp.path().join("not-a-pack")).unwrap_err();
        assert!(err.contains("not an agent-pack"), "{err}");
        // Missing context files still fail validation.
        std::fs::remove_file(dir.join("MEMORY.md")).unwrap();
        std::fs::remove_file(dir.join("entity.json")).unwrap();
        let err = agent_pack(&dir).unwrap_err();
        assert!(err.contains("MEMORY.md"), "{err}");
    }
}
