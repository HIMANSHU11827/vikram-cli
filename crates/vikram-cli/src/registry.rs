//! `vikram registry` — browse and install modules from registry indexes.
//!
//! A registry index is a JSON document (`{"version": 1, "entries": [...]}`) naming installable
//! plugin/module packages. Three sources merge into one catalog, later sources winning on id:
//!
//! - the bundled index shipped in this binary (the project's own hook catalog, fetched from the
//!   repository archive),
//! - the cached remote index at `~/.vikram/registry/remote.json`, written by `registry refresh`,
//! - the local index at `~/.vikram/registry/index.json`, which users edit by hand and which may
//!   also carry `"url"` pointing at a remote index.
//!
//! Remote indexes are fetched with `curl` (no HTTP client is a CLI dependency); a missing curl
//! or a failed fetch is an honest error. Entries from a remote index may only name `http(s)`
//! sources — a remote catalog must not install arbitrary local paths. Local and bundled entries
//! may also use `file://` URLs or plain filesystem paths, which is what the tests exercise.
//!
//! Installing resolves the entry's `source` (an archive, or a directory optionally narrowed by
//! `path`) through the same `PluginRegistry::install` path `plugins install` uses, then caches
//! the resolved module under `~/.vikram/plugin-registry/<id>` so `plugins update` keeps working
//! offline.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use clap::Subcommand;
use serde::{Deserialize, Serialize};
use vikram_runtime::plugins::{PluginManifest, PluginRegistry};

const INDEX_VERSION: u32 = 1;
const REPO_ARCHIVE: &str = "https://github.com/VIKRAM-CLI/vikram/archive/HEAD.tar.gz";

#[derive(Debug, Subcommand)]
pub enum RegistryAction {
    /// List the merged catalog, marking installed entries (default).
    List,
    /// Search id, name, and description.
    Search { query: String },
    /// Show one entry in full.
    Info { id: String },
    /// Install an entry by id.
    Install {
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Fetch the configured remote index into the local cache.
    Refresh {
        /// Remote index URL for this fetch only.
        #[arg(long)]
        registry_url: Option<String>,
    },
    /// Show the remote index URL and where it comes from.
    Url,
    /// Set the remote index URL in the local index file (`--clear` removes it).
    SetUrl {
        /// URL to store; omit with --clear to remove.
        url: Option<String>,
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RegistryEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    /// `module` or `plugin`; both install through the plugin registry.
    pub kind: String,
    /// Archive URL/path, module directory, or `file://` URL. With `path`, a directory or archive
    /// root to look inside.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub example: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct RegistryIndex {
    version: Option<u32>,
    /// Remote index URL; honored by `registry refresh` when nothing overrides it.
    url: Option<String>,
    entries: Vec<RegistryEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Bundled,
    Remote,
    Local,
}

impl Origin {
    fn label(self) -> &'static str {
        match self {
            Origin::Bundled => "bundled",
            Origin::Remote => "remote",
            Origin::Local => "local",
        }
    }
}

struct CatalogEntry {
    entry: RegistryEntry,
    origin: Origin,
}

pub fn run(action: RegistryAction) -> Result<(), String> {
    match action {
        RegistryAction::List => list(&catalog()?),
        RegistryAction::Search { query } => {
            let found = catalog()?;
            let hits = search(&found, &query);
            if hits.is_empty() {
                println!("no registry entries match `{query}`");
            } else {
                print_entries(&hits);
            }
            Ok(())
        }
        RegistryAction::Info { id } => {
            let catalog = catalog()?;
            let entry = find(&catalog, &id)?;
            println!("{}", describe(entry));
            Ok(())
        }
        RegistryAction::Install { id, yes } => {
            let catalog = catalog()?;
            let entry = find(&catalog, &id)?;
            install_entry(&entry.entry, yes)
        }
        RegistryAction::Refresh { registry_url } => refresh(registry_url.as_deref()),
        RegistryAction::Url => url(),
        RegistryAction::SetUrl { url, clear } => set_url(url.as_deref(), clear),
    }
}

fn registry_dir() -> PathBuf {
    vikram_runtime::config::vikram_home().join("registry")
}

fn local_index_path() -> PathBuf {
    registry_dir().join("index.json")
}

fn remote_index_path() -> PathBuf {
    registry_dir().join("remote.json")
}

/// The project's own hook catalog, fetched as a repository archive with `path` narrowing each
/// entry to its module directory. These are real modules from `modules/`, published through the
/// repository rather than a release artifact.
fn bundled_index() -> RegistryIndex {
    let entries = [
        (
            "ctx-no-emoji",
            "Context: no emoji",
            "Adds a standing instruction to every turn: no emoji.",
        ),
        (
            "guard-curl-pipe-bash",
            "Guard: pipe to bash",
            "Refuses or rewrites a tool call: pipe to bash.",
        ),
        (
            "result-warn-timeout",
            "Result: a timeout",
            "Inspects tool output for a timeout.",
        ),
    ];
    RegistryIndex {
        version: Some(INDEX_VERSION),
        url: None,
        entries: entries
            .iter()
            .map(|(id, name, description)| RegistryEntry {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                description: (*description).to_owned(),
                version: "1.0.0".to_owned(),
                kind: "module".to_owned(),
                source: REPO_ARCHIVE.to_owned(),
                path: Some(format!("modules/{id}")),
                example: false,
            })
            .collect(),
    }
}

fn parse_index(raw: &str, origin: &str, allow_local: bool) -> Result<Vec<RegistryEntry>, String> {
    let index: RegistryIndex =
        serde_json::from_str(raw).map_err(|err| format!("{origin} is not valid JSON: {err}"))?;
    if let Some(version) = index.version
        && version != INDEX_VERSION
    {
        return Err(format!(
            "{origin} declares index version {version}; this build understands {INDEX_VERSION}"
        ));
    }
    let mut problems = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for entry in &index.entries {
        let id = entry.id.trim();
        if let Some(problem) = validate_entry(entry, allow_local) {
            problems.push(format!("entry `{id}`: {problem}"));
            continue;
        }
        if !seen.insert(id.to_owned()) {
            problems.push(format!("entry `{id}`: duplicate id in the same index"));
        }
    }
    if problems.is_empty() {
        Ok(index.entries)
    } else {
        Err(format!("{origin}: {}", problems.join("; ")))
    }
}

fn validate_entry(entry: &RegistryEntry, allow_local: bool) -> Option<String> {
    let id = entry.id.trim();
    if id.is_empty() {
        return Some("id is required".to_owned());
    }
    if id == "." || id == ".." || id.contains('/') || id.contains('\\') {
        return Some(format!("`{id}` is not a usable id"));
    }
    if entry.name.trim().is_empty() {
        return Some("name is required".to_owned());
    }
    if entry.version.trim().is_empty() {
        return Some("version is required".to_owned());
    }
    if !matches!(entry.kind.trim(), "module" | "plugin") {
        return Some(format!(
            "kind `{}` must be `module` or `plugin`",
            entry.kind.trim()
        ));
    }
    let source = entry.source.trim();
    if source.is_empty() {
        return Some("source is required".to_owned());
    }
    match source.split_once("://") {
        Some((scheme, _)) => {
            let ok = matches!(scheme, "http" | "https") || (allow_local && scheme == "file");
            if !ok {
                return Some(format!(
                    "source scheme `{scheme}` is not allowed; use http(s){}",
                    if allow_local { " or file" } else { "" }
                ));
            }
        }
        None if allow_local => {}
        None => {
            return Some(
                "source must be an http(s) URL in a remote index; local paths are only allowed \
                 in the local or bundled index"
                    .to_owned(),
            );
        }
    }
    if let Some(path) = &entry.path {
        let path = path.trim();
        let usable = !path.is_empty()
            && Path::new(path)
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
        if !usable {
            return Some(format!(
                "path `{path}` must be a relative path inside the source"
            ));
        }
    }
    None
}

fn read_index_file(path: &Path, allow_local: bool) -> Result<Vec<RegistryEntry>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    parse_index(&raw, &path.display().to_string(), allow_local)
}

/// The merged catalog: bundled < remote cache < local index, deduplicated by id.
fn catalog() -> Result<Vec<CatalogEntry>, String> {
    let bundled_raw = serde_json::to_string(&bundled_index()).expect("bundled index serializes");
    let bundled = parse_index(&bundled_raw, "bundled index", true)?;
    let mut entries: BTreeMap<String, CatalogEntry> = bundled
        .into_iter()
        .map(|entry| {
            (
                entry.id.trim().to_owned(),
                CatalogEntry {
                    entry,
                    origin: Origin::Bundled,
                },
            )
        })
        .collect();
    let remote_path = remote_index_path();
    if remote_path.is_file() {
        for entry in read_index_file(&remote_path, false)? {
            entries.insert(
                entry.id.trim().to_owned(),
                CatalogEntry {
                    entry,
                    origin: Origin::Remote,
                },
            );
        }
    }
    let local_path = local_index_path();
    if local_path.is_file() {
        for entry in read_index_file(&local_path, true)? {
            entries.insert(
                entry.id.trim().to_owned(),
                CatalogEntry {
                    entry,
                    origin: Origin::Local,
                },
            );
        }
    }
    Ok(entries.into_values().collect())
}

fn find<'a>(catalog: &'a [CatalogEntry], id: &str) -> Result<&'a CatalogEntry, String> {
    catalog
        .iter()
        .find(|candidate| candidate.entry.id == id)
        .ok_or_else(|| {
            let near = catalog
                .iter()
                .map(|candidate| candidate.entry.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("no registry entry `{id}`; available: {near}")
        })
}

fn search<'a>(catalog: &'a [CatalogEntry], query: &str) -> Vec<&'a CatalogEntry> {
    let needle = query.to_lowercase();
    catalog
        .iter()
        .filter(|candidate| {
            let entry = &candidate.entry;
            entry.id.to_lowercase().contains(&needle)
                || entry.name.to_lowercase().contains(&needle)
                || entry.description.to_lowercase().contains(&needle)
        })
        .collect()
}

fn installed_versions() -> BTreeMap<String, String> {
    PluginRegistry::from_default_dir()
        .plugins()
        .iter()
        .filter(|plugin| plugin.installed)
        .map(|plugin| (plugin.id(), plugin.version.clone().unwrap_or_default()))
        .collect()
}

fn print_entries(entries: &[&CatalogEntry]) {
    let installed = installed_versions();
    println!(
        "{:<28} {:<10} {:<7} {:<8} {:<8} NAME",
        "ID", "VERSION", "KIND", "INSTALLED", "SOURCE"
    );
    for candidate in entries {
        let entry = &candidate.entry;
        let state = installed
            .get(entry.id.trim())
            .cloned()
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{:<28} {:<10} {:<7} {:<8} {:<8} {}{}",
            entry.id,
            entry.version,
            entry.kind,
            state,
            candidate.origin.label(),
            entry.name,
            if entry.example { " (example)" } else { "" }
        );
    }
}

fn describe(candidate: &CatalogEntry) -> String {
    let entry = &candidate.entry;
    let installed = installed_versions();
    let state = installed
        .get(entry.id.trim())
        .map(|version| format!("installed {version}"))
        .unwrap_or_else(|| "not installed".to_owned());
    let mut lines = vec![
        format!("id:          {}", entry.id),
        format!("name:        {}", entry.name),
        format!("version:     {}", entry.version),
        format!("kind:        {}", entry.kind),
        format!("index:       {}", candidate.origin.label()),
        format!("state:       {state}"),
        format!("source:      {}", entry.source),
    ];
    if let Some(path) = &entry.path {
        lines.push(format!("path:        {path}"));
    }
    if !entry.description.is_empty() {
        lines.push(format!("description: {}", entry.description));
    }
    if entry.example {
        lines.push("note:        example entry; the source may not be published".to_owned());
    }
    lines.join("\n")
}

fn list(entries: &[CatalogEntry]) -> Result<(), String> {
    if entries.is_empty() {
        println!("registry catalog is empty");
        return Ok(());
    }
    let refs: Vec<&CatalogEntry> = entries.iter().collect();
    print_entries(&refs);
    if !remote_index_path().is_file() {
        println!("note: remote index not fetched; run `vikram registry refresh`");
    }
    Ok(())
}

/// Where `registry refresh` fetches from: `--registry-url` > `VIKRAM_REGISTRY_URL` > the local
/// index `url` field > `registry_url` in `~/.vikram/config.json`.
fn configured_url(flag: Option<&str>) -> Option<String> {
    if let Some(url) = flag
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
    {
        return Some(url);
    }
    if let Some(url) = std::env::var("VIKRAM_REGISTRY_URL")
        .ok()
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
    {
        return Some(url);
    }
    if let Ok(raw) = std::fs::read_to_string(local_index_path())
        && let Ok(index) = serde_json::from_str::<RegistryIndex>(&raw)
        && let Some(url) = index.url.map(|url| url.trim().to_owned())
        && !url.is_empty()
    {
        return Some(url);
    }
    let config_path = vikram_runtime::config::config_path();
    let raw = std::fs::read_to_string(&config_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("registry_url")?
        .as_str()
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
}

fn url() -> Result<(), String> {
    match configured_url(None) {
        Some(url) => println!("{url}"),
        None => println!("no remote index URL configured"),
    }
    Ok(())
}

fn set_url(url: Option<&str>, clear: bool) -> Result<(), String> {
    if clear == url.is_some() {
        return Err("pass a URL to set, or --clear to remove it".to_owned());
    }
    let path = local_index_path();
    std::fs::create_dir_all(path.parent().unwrap())
        .map_err(|err| format!("cannot create {}: {err}", path.display()))?;
    let mut index = if path.is_file() {
        let raw = std::fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        serde_json::from_str::<RegistryIndex>(&raw)
            .map_err(|err| format!("{} is not valid JSON: {err}", path.display()))?
    } else {
        RegistryIndex::default()
    };
    index.url = if clear {
        None
    } else {
        let url = url.unwrap().trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("registry URL must start with http:// or https://".to_owned());
        }
        Some(url.to_owned())
    };
    let body = serde_json::to_string_pretty(&index).expect("registry index serializes");
    std::fs::write(&path, format!("{body}\n"))
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    match &index.url {
        Some(url) => println!("remote index URL set to {url} ({})", path.display()),
        None => println!("remote index URL cleared ({})", path.display()),
    }
    Ok(())
}

/// `curl -fsSL` to stdout: `-f` makes HTTP errors exit non-zero, `-sS` keeps errors readable.
fn curl(url: &str, output: Option<&Path>) -> Result<Vec<u8>, String> {
    let mut command = Command::new("curl");
    command.args(["-fsSL", "--max-time", "120", "--retry", "2"]);
    if let Some(dest) = output {
        command.arg("-o").arg(dest);
    }
    let result = command.arg(url).output().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            "fetching remote indexes needs the `curl` executable, which was not found".to_owned()
        } else {
            format!("cannot run curl: {err}")
        }
    })?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("curl failed")
            .to_owned();
        return Err(format!("could not fetch {url}: {detail}"));
    }
    if let Some(dest) = output {
        std::fs::read(dest).map_err(|err| format!("cannot read {}: {err}", dest.display()))
    } else {
        Ok(result.stdout)
    }
}

fn refresh(flag_url: Option<&str>) -> Result<(), String> {
    let url = configured_url(flag_url).ok_or_else(|| {
        "no remote index URL configured; pass `registry set-url <url>`, set \
         VIKRAM_REGISTRY_URL, or add `registry_url` to the vikram config"
            .to_owned()
    })?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!(
            "registry URL must start with http:// or https://: {url}"
        ));
    }
    let body = curl(&url, None)?;
    let raw =
        String::from_utf8(body).map_err(|_| format!("remote index {url} is not UTF-8 JSON"))?;
    // A remote index is untrusted input: local paths are refused outright.
    let entries = parse_index(&raw, &format!("remote index {url}"), false)?;
    let index = RegistryIndex {
        version: Some(INDEX_VERSION),
        url: None,
        entries,
    };
    let body = serde_json::to_string_pretty(&index).expect("registry index serializes");
    let path = remote_index_path();
    std::fs::create_dir_all(path.parent().unwrap())
        .map_err(|err| format!("cannot create {}: {err}", path.display()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{body}\n"))
        .map_err(|err| format!("cannot write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    println!(
        "fetched {} entr{} from {url} -> {}",
        index.entries.len(),
        if index.entries.len() == 1 { "y" } else { "ies" },
        path.display()
    );
    Ok(())
}

/// Strip a `file://` scheme; anything else is already a filesystem path.
fn local_source_path(source: &str) -> PathBuf {
    source
        .strip_prefix("file://")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(source))
}

/// Download an http(s) source and extract it like any other archive. The returned value owns
/// both temp dirs; it must outlive the install that reads `path()`.
fn fetch_remote(source: &str) -> Result<crate::archive::ExtractedModule, String> {
    let tmp = tempfile::tempdir()
        .map_err(|err| format!("cannot create a temp dir for the download: {err}"))?;
    let dest = tmp.path().join("source.tar.gz");
    curl(source, Some(&dest))?;
    crate::archive::extract_module_dir(&dest)
}

/// Resolve an entry to the directory holding `plugin.json`, keeping any extraction temp dirs
/// alive in `held` for the caller.
fn resolve_source(
    entry: &RegistryEntry,
) -> Result<(PathBuf, Vec<crate::archive::ExtractedModule>), String> {
    let mut held = Vec::new();
    let source = entry.source.trim();
    let root = if source.starts_with("http://") || source.starts_with("https://") {
        let extracted = fetch_remote(source)?;
        let root = extracted.path().to_path_buf();
        held.push(extracted);
        root
    } else {
        let local = local_source_path(source);
        if crate::archive::looks_like_archive(&local) {
            let extracted = crate::archive::extract_module_dir(&local)?;
            let root = extracted.path().to_path_buf();
            held.push(extracted);
            root
        } else if local.is_dir() {
            return Ok((module_in_dir(&local, entry)?, held));
        } else {
            return Err(format!(
                "registry source `{source}` for `{}` is not a plugin directory or archive",
                entry.id
            ));
        }
    };
    Ok((apply_entry_path(root, entry)?, held))
}

fn apply_entry_path(root: PathBuf, entry: &RegistryEntry) -> Result<PathBuf, String> {
    let dir = match &entry.path {
        Some(path) => root.join(path.trim()),
        None => root,
    };
    if !dir.join("plugin.json").is_file() {
        return Err(format!(
            "registry entry `{}` resolves to {}, which has no plugin.json",
            entry.id,
            dir.display()
        ));
    }
    Ok(dir)
}

/// A local directory source may be the module itself, a parent containing `<path>` or `<id>`.
fn module_in_dir(dir: &Path, entry: &RegistryEntry) -> Result<PathBuf, String> {
    if let Some(path) = &entry.path {
        let candidate = dir.join(path.trim());
        if candidate.join("plugin.json").is_file() {
            return Ok(candidate);
        }
    }
    if entry.path.is_none() && dir.join("plugin.json").is_file() {
        return Ok(dir.to_path_buf());
    }
    let by_id = dir.join(entry.id.trim());
    if by_id.join("plugin.json").is_file() {
        return Ok(by_id);
    }
    Err(format!(
        "registry source {} has no plugin module for `{}`",
        dir.display(),
        entry.id
    ))
}

fn copy_dir(source: &Path, target: &Path) -> Result<(), String> {
    std::fs::create_dir_all(target)
        .map_err(|err| format!("cannot create {}: {err}", target.display()))?;
    for entry in std::fs::read_dir(source)
        .map_err(|err| format!("cannot read {}: {err}", source.display()))?
        .flatten()
    {
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|err| format!("cannot inspect {}: {err}", from.display()))?;
        if kind.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|err| format!("cannot copy {}: {err}", from.display()))?;
        }
    }
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

/// Install through the same registry path `plugins install` uses, then cache the resolved module
/// in the local source registry (`plugin-registry/<id>`) when absent so `plugins update` has a
/// durable source. An existing cache entry is left alone: it is the operator's local registry.
fn install_entry(entry: &RegistryEntry, yes: bool) -> Result<(), String> {
    let (dir, _held) = resolve_source(entry)?;
    let manifest = PluginManifest::from_dir(&dir)
        .map_err(|err| format!("registry entry `{}` is not a plugin: {err}", entry.id))?;
    let id = manifest.id().trim().to_owned();
    if !yes {
        confirm(&format!(
            "install registry entry `{id}` ({})? [y/N] ",
            entry.source
        ))?;
    }
    let registry = PluginRegistry::from_default_dir();
    let installed = registry.install(&dir)?;
    let cache = registry.registry_root().join(&id);
    if !cache.join("plugin.json").is_file()
        && let Err(err) = copy_dir(&dir, &cache)
    {
        println!("installed {installed} (could not cache the source: {err})");
        return Ok(());
    }
    if cache.join("plugin.json").is_file() {
        crate::plugins::record_source(&id, &cache)?;
    }
    println!("installed {installed}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, source: &str) -> RegistryEntry {
        RegistryEntry {
            id: id.to_owned(),
            name: format!("Module {id}"),
            description: format!("Test module {id}"),
            version: "1.0.0".to_owned(),
            kind: "module".to_owned(),
            source: source.to_owned(),
            path: None,
            example: false,
        }
    }

    fn index_json(entries: &[RegistryEntry]) -> String {
        serde_json::to_string(&RegistryIndex {
            version: Some(INDEX_VERSION),
            url: None,
            entries: entries.to_vec(),
        })
        .unwrap()
    }

    #[test]
    fn parse_accepts_a_valid_index() {
        let raw = index_json(&[entry("acme", "https://example.com/acme.tar.gz")]);
        let entries = parse_index(&raw, "test", false).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "acme");
    }

    #[test]
    fn parse_requires_the_mandatory_fields() {
        for bad in [
            r#"{"entries":[{"name":"x","version":"1","kind":"module","source":"https://a/b.tgz"}]}"#,
            r#"{"entries":[{"id":"x","version":"1","kind":"module","source":"https://a/b.tgz"}]}"#,
            r#"{"entries":[{"id":"x","name":"n","kind":"module","source":"https://a/b.tgz"}]}"#,
            r#"{"entries":[{"id":"x","name":"n","version":"1","source":"https://a/b.tgz"}]}"#,
            r#"{"entries":[{"id":"x","name":"n","version":"1","kind":"module"}]}"#,
        ] {
            assert!(parse_index(bad, "test", false).is_err(), "{bad}");
        }
    }

    #[test]
    fn remote_index_refuses_non_http_sources() {
        for source in [
            "ftp://example.com/x.tar.gz",
            "file:///etc/passwd",
            "/tmp/local/module",
            "javascript:alert(1)",
        ] {
            let raw = index_json(&[entry("acme", source)]);
            let err = parse_index(&raw, "test", false).unwrap_err();
            assert!(err.contains("acme"), "{source}: {err}");
        }
        // The same entries are fine in a local index.
        let raw = index_json(&[entry("acme", "/tmp/local/module")]);
        assert!(parse_index(&raw, "test", true).is_ok());
        let raw = index_json(&[entry("acme", "file:///tmp/x.tar.gz")]);
        assert!(parse_index(&raw, "test", true).is_ok());
    }

    #[test]
    fn parse_rejects_bad_kind_duplicate_ids_and_traversal() {
        let mut bad = entry("acme", "https://a/b.tgz");
        bad.kind = "skill".to_owned();
        assert!(parse_index(&index_json(&[bad]), "test", false).is_err());

        let dup = index_json(&[
            entry("acme", "https://a/1.tgz"),
            entry("acme", "https://a/2.tgz"),
        ]);
        let err = parse_index(&dup, "test", false).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");

        let mut traversal = entry("acme", "https://a/repo.tar.gz");
        traversal.path = Some("../escape".to_owned());
        assert!(parse_index(&index_json(&[traversal]), "test", false).is_err());

        let mut dotdot = entry("../x", "https://a/b.tgz");
        dotdot.id = "../x".to_owned();
        assert!(parse_index(&index_json(&[dotdot]), "test", false).is_err());
    }

    #[test]
    fn corrupt_index_is_an_honest_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.json");
        std::fs::write(&path, "{not json").unwrap();
        let err = read_index_file(&path, true).unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
        std::fs::write(&path, r#"{"version":2,"entries":[]}"#).unwrap();
        let err = read_index_file(&path, true).unwrap_err();
        assert!(err.contains("version 2"), "{err}");
    }

    #[test]
    fn search_filters_id_name_and_description() {
        let mut entries = vec![
            CatalogEntry {
                entry: entry("alpha", "https://a/1.tgz"),
                origin: Origin::Local,
            },
            CatalogEntry {
                entry: {
                    let mut e = entry("beta", "https://a/2.tgz");
                    e.description = "guards timeouts".to_owned();
                    e
                },
                origin: Origin::Remote,
            },
        ];
        entries[0].entry.name = "Timeout guard".to_owned();
        assert_eq!(search(&entries, "TIMEOUT").len(), 2);
        assert_eq!(search(&entries, "beta").len(), 1);
        assert!(search(&entries, "zzz").is_empty());
    }

    fn write_module(dir: &Path, id: &str) {
        let module = dir.join(id);
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(
            module.join("plugin.json"),
            format!(r#"{{"id": "{id}", "name": "{id}", "version": "1.0.0"}}"#),
        )
        .unwrap();
    }

    fn tar_dir(dir: &Path, out: &Path) {
        let name = dir.file_name().unwrap();
        let parent = dir.parent().unwrap();
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(out)
            .arg("-C")
            .arg(parent)
            .arg(name)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn install_from_a_local_index_entry() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };

        let source = tmp.path().join("src").join("packed-mod");
        write_module(&tmp.path().join("src"), "packed-mod");
        let archive = tmp.path().join("packed-mod.tar.gz");
        tar_dir(&source, &archive);

        let index = tmp.path().join("home").join("registry").join("index.json");
        std::fs::create_dir_all(index.parent().unwrap()).unwrap();
        let local = entry("packed-mod", &archive.to_string_lossy());
        std::fs::write(&index, index_json(&[local])).unwrap();

        let catalog = catalog().unwrap();
        let found = find(&catalog, "packed-mod").unwrap();
        assert_eq!(found.origin, Origin::Local);
        install_entry(&found.entry, true).unwrap();

        let plugins = vikram_runtime::config::plugins_dir();
        assert!(plugins.join("packed-mod").join("plugin.json").is_file());
        // The resolved module is cached in the local source registry for `plugins update`.
        let cache = vikram_runtime::config::vikram_home()
            .join("plugin-registry")
            .join("packed-mod");
        assert!(cache.join("plugin.json").is_file());
        let installed = installed_versions();
        assert_eq!(
            installed.get("packed-mod").map(String::as_str),
            Some("1.0.0")
        );
    }

    #[test]
    fn install_from_a_directory_source_with_path() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };

        let catalog_dir = tmp.path().join("catalog");
        write_module(&catalog_dir, "nested-mod");
        let mut local = entry("nested-mod", &catalog_dir.to_string_lossy());
        local.path = Some("nested-mod".to_owned());
        install_entry(&local, true).unwrap();
        assert!(
            vikram_runtime::config::plugins_dir()
                .join("nested-mod")
                .join("plugin.json")
                .is_file()
        );
    }

    #[test]
    fn install_reports_a_missing_source() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let local = entry("ghost", "/nonexistent/source");
        let err = install_entry(&local, true).unwrap_err();
        assert!(err.contains("not a plugin directory or archive"), "{err}");
    }

    #[test]
    fn bundled_index_parses_and_points_at_the_repo() {
        let index = bundled_index();
        assert_eq!(index.entries.len(), 3);
        for entry in &index.entries {
            assert!(entry.source.starts_with("https://"));
            assert!(entry.path.as_deref().unwrap().starts_with("modules/"));
        }
    }

    #[test]
    fn catalog_merges_local_over_bundled() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        let mut shadow = entry("ctx-no-emoji", "/tmp/local/ctx-no-emoji");
        shadow.name = "Local override".to_owned();
        let index = local_index_path();
        std::fs::create_dir_all(index.parent().unwrap()).unwrap();
        std::fs::write(&index, index_json(&[shadow])).unwrap();
        let catalog = catalog().unwrap();
        let found = catalog
            .iter()
            .find(|candidate| candidate.entry.id == "ctx-no-emoji")
            .unwrap();
        assert_eq!(found.origin, Origin::Local);
        assert_eq!(found.entry.name, "Local override");
        assert!(catalog.len() >= 3);
    }

    #[test]
    fn set_url_round_trips_through_the_local_index() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", tmp.path().join("home")) };
        set_url(Some("https://registry.example.com/index.json"), false).unwrap();
        assert_eq!(
            configured_url(None).as_deref(),
            Some("https://registry.example.com/index.json")
        );
        let raw = std::fs::read_to_string(local_index_path()).unwrap();
        let index: RegistryIndex = serde_json::from_str(&raw).unwrap();
        assert!(index.entries.is_empty());
        set_url(None, true).unwrap();
        assert!(configured_url(None).is_none());
        assert!(set_url(Some("ftp://x"), false).is_err());
        assert!(set_url(None, false).is_err());
    }
}
