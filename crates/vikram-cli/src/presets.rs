//! `vikram presets` — named bundles of config-relevant settings.
//!
//! A preset is a JSON file under `~/.vikram/presets/<name>.json` holding a
//! strictly-validated subset of `Config`: model/provider selection, context
//! window and budgeting, memory provider, fallback and title models, posture,
//! and the `context.*`/`search.*` blocks — leaf by leaf, so applying one never
//! resets a sibling field the preset does not mention.
//!
//! Secrets are structurally absent: `Config` stores only `api_key_env`, the
//! *name* of the variable holding the key, and presets carry that name and
//! nothing else. `security` and `theme` are deliberately not presettable —
//! restrictions belong to the layer above the operator's convenience, and a
//! preset must never be a way to widen them.
//!
//! `suggest` is read-only: it scans evolution.db experiences/proposals and
//! learning.json proposals for config-shaped keys and prints what it saw as
//! text. It never writes a preset.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde::{Deserialize, Serialize};
use vikram_contracts::PermissionMode;
use vikram_runtime::config::{Config, SearchBackend, config_path, vikram_home};

/// The only preset format this build reads and writes.
const PRESET_VERSION: u64 = 1;

/// Keys `--set` accepts, for the error message when one is refused.
const ALLOWED_KEYS: &[&str] = &[
    "model",
    "provider",
    "base_url",
    "fallback_model",
    "memory_provider",
    "api_key_env",
    "context_window",
    "workspace_root",
    "max_steps",
    "max_output_tokens",
    "stream_silence_secs",
    "learning_review_every",
    "memory_nudge_interval",
    "title_model",
    "ascii_fallback",
    "thinking",
    "mode",
    "context.auto_compact",
    "context.trigger_percent",
    "context.keep_recent",
    "context.auto_compact_limit",
    "context.safety_margin_percent",
    "context.tool_output_ceiling",
    "context.memory_retrieval",
    "context.memory_budget_tokens",
    "context.project_budget_tokens",
    "context.delegation_budget_tokens",
    "context.hive_budget_tokens",
    "search.backend",
    "search.api_key_env",
    "search.brave_endpoint",
    "search.bing_endpoint",
];

#[derive(Debug, Subcommand)]
pub enum PresetsAction {
    /// List presets: name, version, overridden keys, creation date.
    List,
    /// Show one preset's full JSON.
    Show { name: String },
    /// Create a preset from the current config and/or explicit --set pairs.
    Create {
        name: String,
        /// Capture every presettable field from the current config.json.
        #[arg(long)]
        from_current: bool,
        /// Set one field: key=value. Repeatable. Keys: see `vikram config set`
        /// plus provider, fallback_model, memory_provider, title_model,
        /// stream_silence_secs, learning_review_every, memory_nudge_interval,
        /// search.*, context.project_budget_tokens.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Free-text note stored in the preset.
        #[arg(long)]
        notes: Option<String>,
    },
    /// Merge a preset into config.json (backs up the old file first).
    Apply {
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Delete a preset.
    Delete { name: String },
    /// Print a preset's JSON, or write it to --out.
    Export {
        name: String,
        /// Write to this path instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Validate a preset file and install it into the presets directory.
    Import { file: PathBuf },
    /// Scan recorded experiences and learning proposals for recurring
    /// model/provider/context patterns. Read-only; prints suggestions as text.
    Suggest,
}

pub fn run(action: PresetsAction) -> Result<(), String> {
    match action {
        PresetsAction::List => list(),
        PresetsAction::Show { name } => show(&name),
        PresetsAction::Create {
            name,
            from_current,
            set,
            notes,
        } => create(&name, from_current, &set, notes.as_deref()),
        PresetsAction::Apply { name, yes } => apply(&name, yes),
        PresetsAction::Delete { name } => delete(&name),
        PresetsAction::Export { name, out } => export(&name, out.as_deref()),
        PresetsAction::Import { file } => import(&file),
        PresetsAction::Suggest => suggest(),
    }
}

// ---- preset file ------------------------------------------------------------

/// The `context.*` block of a preset: every leaf optional, merged leaf-wise on
/// apply. `auto_compact_limit` is `Option<Option<u64>>` so a preset can also
/// *clear* the limit (JSON `null`) rather than only set it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PresetContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_recent: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact_limit: Option<Option<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_margin_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output_ceiling: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_retrieval: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_budget_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_budget_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegation_budget_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hive_budget_tokens: Option<u64>,
}

/// The `search.*` block of a preset.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PresetSearch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<SearchBackend>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brave_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bing_endpoint: Option<String>,
}

/// A named bundle of config fields. Only fields the preset actually manages
/// are `Some`; everything else stays untouched on apply.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PresetConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The *name* of the env var holding the key — never a key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<Option<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<Option<PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<Option<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_silence_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub learning_review_every: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_nudge_interval: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ascii_fallback: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<PresetContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<PresetSearch>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preset {
    name: String,
    version: u64,
    created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    config: PresetConfig,
}

fn presets_dir() -> PathBuf {
    vikram_home().join("presets")
}

fn preset_path(name: &str) -> Result<PathBuf, String> {
    check_name(name)?;
    Ok(presets_dir().join(format!("{name}.json")))
}

/// A preset name becomes a filename, so it must be a plain file stem: no
/// separators, no dots-only names, nothing that could escape the directory.
fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid preset name `{name}`; use letters, digits, `-`, `_`, `.`"
        ))
    }
}

fn load_preset_at(path: &Path) -> Result<Preset, String> {
    parse_preset(path, true)
}

/// `filename_must_match` is set for files inside the presets dir, where the
/// name is the file stem; an import source may be called anything.
fn parse_preset(path: &Path, filename_must_match: bool) -> Result<Preset, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let preset: Preset = serde_json::from_str(&raw)
        .map_err(|e| format!("{} is not a valid preset: {e}", path.display()))?;
    if preset.version != PRESET_VERSION {
        return Err(format!(
            "{}: unsupported preset version {} (this build reads {PRESET_VERSION})",
            path.display(),
            preset.version
        ));
    }
    check_name(&preset.name)?;
    if filename_must_match {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if preset.name != stem {
            return Err(format!(
                "{}: preset name `{}` does not match its file name",
                path.display(),
                preset.name
            ));
        }
    }
    if flatten(&preset.config).is_empty() {
        return Err(format!(
            "{}: preset `{}` manages no config fields",
            path.display(),
            preset.name
        ));
    }
    Ok(preset)
}

fn load_preset(name: &str) -> Result<Preset, String> {
    let path = preset_path(name)?;
    if !path.is_file() {
        return Err(format!("no preset `{name}` (see `vikram presets list`)"));
    }
    load_preset_at(&path)
}

fn write_preset(preset: &Preset) -> Result<(), String> {
    let path = preset_path(&preset.name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut body = serde_json::to_string_pretty(preset)
        .map_err(|e| format!("cannot serialize preset: {e}"))?;
    body.push('\n');
    std::fs::write(&path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

// ---- config <-> preset -------------------------------------------------------

/// Every presettable field set to the config's current value.
fn capture(config: &Config) -> PresetConfig {
    PresetConfig {
        base_url: Some(config.base_url.clone()),
        provider: Some(config.provider.clone()),
        fallback_model: Some(config.fallback_model.clone()),
        memory_provider: Some(config.memory_provider.clone()),
        model: Some(config.model.clone()),
        api_key_env: Some(config.api_key_env.clone()),
        context_window: Some(config.context_window),
        workspace_root: Some(config.workspace_root.clone()),
        max_steps: Some(config.max_steps),
        max_output_tokens: Some(config.max_output_tokens),
        stream_silence_secs: Some(config.stream_silence_secs),
        learning_review_every: Some(config.learning_review_every),
        memory_nudge_interval: Some(config.memory_nudge_interval),
        title_model: Some(config.title_model.clone()),
        ascii_fallback: Some(config.ascii_fallback),
        thinking: Some(config.thinking),
        mode: Some(config.mode),
        context: Some(PresetContext {
            auto_compact: Some(config.context.auto_compact),
            trigger_percent: Some(config.context.trigger_percent),
            keep_recent: Some(config.context.keep_recent),
            auto_compact_limit: Some(config.context.auto_compact_limit),
            safety_margin_percent: Some(config.context.safety_margin_percent),
            tool_output_ceiling: Some(config.context.tool_output_ceiling),
            memory_retrieval: Some(config.context.memory_retrieval),
            memory_budget_tokens: Some(config.context.memory_budget_tokens),
            project_budget_tokens: Some(config.context.project_budget_tokens),
            delegation_budget_tokens: Some(config.context.delegation_budget_tokens),
            hive_budget_tokens: Some(config.context.hive_budget_tokens),
        }),
        search: Some(PresetSearch {
            backend: Some(config.search.backend),
            api_key_env: Some(config.search.api_key_env.clone()),
            brave_endpoint: Some(config.search.brave_endpoint.clone()),
            bing_endpoint: Some(config.search.bing_endpoint.clone()),
        }),
    }
}

/// Merge the fields a preset manages into `config`, leaf by leaf. Fields the
/// preset leaves `None` are untouched.
fn apply_to(config: &mut Config, preset: &PresetConfig) {
    if let Some(v) = &preset.base_url {
        config.base_url = v.clone();
    }
    if let Some(v) = &preset.provider {
        config.provider = v.clone();
    }
    if let Some(v) = &preset.fallback_model {
        config.fallback_model = v.clone();
    }
    if let Some(v) = &preset.memory_provider {
        config.memory_provider = v.clone();
    }
    if let Some(v) = &preset.model {
        config.model = v.clone();
    }
    if let Some(v) = &preset.api_key_env {
        config.api_key_env = v.clone();
    }
    if let Some(v) = preset.context_window {
        config.context_window = v;
    }
    if let Some(v) = &preset.workspace_root {
        config.workspace_root = v.clone();
    }
    if let Some(v) = preset.max_steps {
        config.max_steps = v;
    }
    if let Some(v) = preset.max_output_tokens {
        config.max_output_tokens = v;
    }
    if let Some(v) = preset.stream_silence_secs {
        config.stream_silence_secs = v;
    }
    if let Some(v) = preset.learning_review_every {
        config.learning_review_every = v;
    }
    if let Some(v) = preset.memory_nudge_interval {
        config.memory_nudge_interval = v;
    }
    if let Some(v) = &preset.title_model {
        config.title_model = v.clone();
    }
    if let Some(v) = preset.ascii_fallback {
        config.ascii_fallback = v;
    }
    if let Some(v) = preset.thinking {
        config.thinking = v;
    }
    if let Some(v) = preset.mode {
        config.mode = v;
    }
    if let Some(ctx) = &preset.context {
        let c = &mut config.context;
        if let Some(v) = ctx.auto_compact {
            c.auto_compact = v;
        }
        if let Some(v) = ctx.trigger_percent {
            c.trigger_percent = v;
        }
        if let Some(v) = ctx.keep_recent {
            c.keep_recent = v;
        }
        if let Some(v) = ctx.auto_compact_limit {
            c.auto_compact_limit = v;
        }
        if let Some(v) = ctx.safety_margin_percent {
            c.safety_margin_percent = v;
        }
        if let Some(v) = ctx.tool_output_ceiling {
            c.tool_output_ceiling = v;
        }
        if let Some(v) = ctx.memory_retrieval {
            c.memory_retrieval = v;
        }
        if let Some(v) = ctx.memory_budget_tokens {
            c.memory_budget_tokens = v;
        }
        if let Some(v) = ctx.project_budget_tokens {
            c.project_budget_tokens = v;
        }
        if let Some(v) = ctx.delegation_budget_tokens {
            c.delegation_budget_tokens = v;
        }
        if let Some(v) = ctx.hive_budget_tokens {
            c.hive_budget_tokens = v;
        }
    }
    if let Some(search) = &preset.search {
        let s = &mut config.search;
        if let Some(v) = search.backend {
            s.backend = v;
        }
        if let Some(v) = &search.api_key_env {
            s.api_key_env = v.clone();
        }
        if let Some(v) = &search.brave_endpoint {
            s.brave_endpoint = v.clone();
        }
        if let Some(v) = &search.bing_endpoint {
            s.bing_endpoint = v.clone();
        }
    }
}

/// Dotted-key -> JSON-value view of the fields a preset manages. `Some(None)`
/// fields survive as `null` because the outer `Option` is skipped during
/// serialization while the inner one is not.
fn flatten(config: &PresetConfig) -> BTreeMap<String, serde_json::Value> {
    let value = serde_json::to_value(config).unwrap_or(serde_json::Value::Null);
    let mut out = BTreeMap::new();
    flatten_into(&value, "", &mut out);
    out
}

fn flatten_into(
    value: &serde_json::Value,
    prefix: &str,
    out: &mut BTreeMap<String, serde_json::Value>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                let dotted = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_into(item, &dotted, out);
            }
        }
        other => {
            out.insert(prefix.to_owned(), other.clone());
        }
    }
}

fn show_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "(cleared)".to_owned(),
        serde_json::Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    }
}

/// `key: old -> new` for every field the preset manages that would change.
fn diff_lines(config: &Config, preset: &PresetConfig) -> Vec<String> {
    let current = flatten(&capture(config));
    let mut lines = Vec::new();
    for (key, new) in flatten(preset) {
        let old = current.get(&key);
        if old == Some(&new) {
            continue;
        }
        let before = old.map(show_value).unwrap_or_else(|| "(unset)".to_owned());
        lines.push(format!("{key}: {before} -> {}", show_value(&new)));
    }
    lines
}

// ---- --set parsing ------------------------------------------------------------

fn parse_bool(raw: &str) -> Result<bool, String> {
    raw.parse::<bool>()
        .map_err(|_| format!("expected true or false, got `{raw}`"))
}

fn parse_u64(raw: &str) -> Result<u64, String> {
    raw.parse::<u64>()
        .map_err(|_| format!("expected an integer, got `{raw}`"))
}

fn parse_u32(raw: &str) -> Result<u32, String> {
    raw.parse::<u32>()
        .map_err(|_| format!("expected an integer, got `{raw}`"))
}

fn parse_u8(raw: &str) -> Result<u8, String> {
    raw.parse::<u8>()
        .map_err(|_| format!("expected an integer, got `{raw}`"))
}

fn parse_opt<T>(raw: &str, parse: fn(&str) -> Result<T, String>) -> Result<Option<T>, String> {
    if raw.trim().is_empty() || raw == "null" {
        Ok(None)
    } else {
        parse(raw).map(Some)
    }
}

fn context_of(pc: &mut PresetConfig) -> &mut PresetContext {
    pc.context.get_or_insert_with(PresetContext::default)
}

fn search_of(pc: &mut PresetConfig) -> &mut PresetSearch {
    pc.search.get_or_insert_with(PresetSearch::default)
}

/// Set one `key=value` pair on a preset under construction. Only allowlisted
/// keys are accepted; `api_key` is not a key anywhere in this file.
fn set_field(pc: &mut PresetConfig, key: &str, raw: &str) -> Result<(), String> {
    match key {
        "model" => pc.model = Some(raw.to_owned()),
        "provider" => pc.provider = Some(raw.to_owned()),
        "base_url" => pc.base_url = Some(raw.to_owned()),
        "fallback_model" => pc.fallback_model = Some(raw.to_owned()),
        "memory_provider" => pc.memory_provider = Some(raw.to_owned()),
        "api_key_env" => pc.api_key_env = Some(raw.to_owned()),
        "context_window" => pc.context_window = Some(parse_opt(raw, parse_u64)?),
        "workspace_root" => {
            pc.workspace_root = Some(if raw.trim().is_empty() {
                None
            } else {
                Some(PathBuf::from(raw))
            });
        }
        "max_steps" => pc.max_steps = Some(parse_u32(raw)?),
        "max_output_tokens" => pc.max_output_tokens = Some(parse_opt(raw, parse_u32)?),
        "stream_silence_secs" => pc.stream_silence_secs = Some(parse_u64(raw)?),
        "learning_review_every" => pc.learning_review_every = Some(parse_u64(raw)?),
        "memory_nudge_interval" => pc.memory_nudge_interval = Some(parse_u64(raw)?),
        "title_model" => pc.title_model = Some(raw.to_owned()),
        "ascii_fallback" => pc.ascii_fallback = Some(parse_bool(raw)?),
        "thinking" => pc.thinking = Some(parse_bool(raw)?),
        "mode" => {
            pc.mode =
                Some(PermissionMode::parse(raw).ok_or_else(|| format!("unknown mode `{raw}`"))?);
        }
        "context.auto_compact" => context_of(pc).auto_compact = Some(parse_bool(raw)?),
        "context.trigger_percent" => context_of(pc).trigger_percent = Some(parse_u8(raw)?),
        "context.keep_recent" => context_of(pc).keep_recent = Some(parse_u32(raw)?),
        "context.auto_compact_limit" => {
            context_of(pc).auto_compact_limit = Some(parse_opt(raw, parse_u64)?);
        }
        "context.safety_margin_percent" => {
            context_of(pc).safety_margin_percent = Some(parse_u8(raw)?);
        }
        "context.tool_output_ceiling" => {
            context_of(pc).tool_output_ceiling = Some(parse_u32(raw)?);
        }
        "context.memory_retrieval" => context_of(pc).memory_retrieval = Some(parse_bool(raw)?),
        "context.memory_budget_tokens" => {
            context_of(pc).memory_budget_tokens = Some(parse_u64(raw)?);
        }
        "context.project_budget_tokens" => {
            context_of(pc).project_budget_tokens = Some(parse_u64(raw)?);
        }
        "context.delegation_budget_tokens" => {
            context_of(pc).delegation_budget_tokens = Some(parse_u64(raw)?);
        }
        "context.hive_budget_tokens" => {
            context_of(pc).hive_budget_tokens = Some(parse_u64(raw)?);
        }
        "search.backend" => {
            search_of(pc).backend = Some(parse_backend(raw)?);
        }
        "search.api_key_env" => search_of(pc).api_key_env = Some(raw.to_owned()),
        "search.brave_endpoint" => search_of(pc).brave_endpoint = Some(raw.to_owned()),
        "search.bing_endpoint" => search_of(pc).bing_endpoint = Some(raw.to_owned()),
        other => {
            return Err(format!(
                "unknown preset key `{other}`; allowed keys: {}",
                ALLOWED_KEYS.join(", ")
            ));
        }
    }
    Ok(())
}

fn parse_backend(raw: &str) -> Result<SearchBackend, String> {
    serde_json::from_value(serde_json::Value::String(raw.to_owned()))
        .map_err(|_| format!("unknown search backend `{raw}` (auto | brave | bing)"))
}

// ---- commands -----------------------------------------------------------------

fn list() -> Result<(), String> {
    let dir = presets_dir();
    let mut names: Vec<String> = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .flatten()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json") {
                names.push(name.trim_end_matches(".json").to_owned());
            }
        }
    }
    names.sort();
    if names.is_empty() {
        println!("no presets — `vikram presets create <name> --from-current` makes one");
        return Ok(());
    }
    for name in names {
        let path = dir.join(format!("{name}.json"));
        match load_preset_at(&path) {
            Ok(preset) => {
                let keys = flatten(&preset.config)
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{:<20} v{}  {}  {}",
                    preset.name,
                    preset.version,
                    crate::format_date(preset.created_ms),
                    keys
                );
            }
            Err(err) => println!("{name:<20}  (skipped: {err})"),
        }
    }
    Ok(())
}

fn show(name: &str) -> Result<(), String> {
    load_preset(name)?;
    let path = preset_path(name)?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    // Print the file as stored: what `show` shows is what `import`/`export` carry.
    print!("{raw}");
    if !raw.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn create(
    name: &str,
    from_current: bool,
    sets: &[String],
    notes: Option<&str>,
) -> Result<(), String> {
    check_name(name)?;
    let path = preset_path(name)?;
    if path.exists() {
        return Err(format!(
            "preset `{name}` already exists; `vikram presets delete {name}` first to replace it"
        ));
    }
    if !from_current && sets.is_empty() {
        return Err(
            "nothing to capture: pass --from-current and/or --set key=value pairs".to_owned(),
        );
    }
    let mut config = PresetConfig::default();
    if from_current {
        // Read the durable file, not the in-memory config: global flags like
        // --model must not leak into a preset the operator thinks is "current".
        config = capture(&Config::load().map_err(|e| e.to_string())?);
    }
    for pair in sets {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(format!("--set expects KEY=VALUE, got `{pair}`"));
        };
        set_field(&mut config, key.trim(), value)?;
    }
    if flatten(&config).is_empty() {
        return Err("the preset would manage no config fields".to_owned());
    }
    let preset = Preset {
        name: name.to_owned(),
        version: PRESET_VERSION,
        created_ms: vikram_runtime::types::now_ms(),
        notes: notes.map(str::to_owned),
        config,
    };
    write_preset(&preset)?;
    println!(
        "created preset `{name}` at {} ({} field(s))",
        path.display(),
        flatten(&preset.config).len()
    );
    Ok(())
}

fn apply(name: &str, yes: bool) -> Result<(), String> {
    let preset = load_preset(name)?;
    // Same rule as `config validate`: apply against the durable file, not the
    // config this process may have been launched with overrides for.
    let mut config = Config::load().map_err(|e| e.to_string())?;
    let lines = diff_lines(&config, &preset.config);
    if lines.is_empty() {
        println!("config already matches preset `{name}`; nothing to do");
        return Ok(());
    }
    println!("applying preset `{name}`:");
    for line in &lines {
        println!("  {line}");
    }
    if !yes {
        crate::dist::confirm(&format!("write {}? [y/N] ", config_path().display()))?;
    }
    apply_to(&mut config, &preset.config);
    // Back up before replacing, same convention `vikram dist import` uses.
    let dst = config_path();
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    if dst.is_file() {
        let backup = dst.with_file_name(format!(
            "config.json.bak-{}",
            vikram_runtime::types::now_ms()
        ));
        std::fs::copy(&dst, &backup).map_err(|e| {
            format!(
                "cannot back up {} to {}: {e}",
                dst.display(),
                backup.display()
            )
        })?;
    }
    config
        .save()
        .map_err(|e| format!("cannot write {}: {e}", dst.display()))?;
    println!("applied preset `{name}` to {}", dst.display());
    Ok(())
}

fn delete(name: &str) -> Result<(), String> {
    let path = preset_path(name)?;
    if !path.is_file() {
        return Err(format!("no preset `{name}` (see `vikram presets list`)"));
    }
    std::fs::remove_file(&path).map_err(|e| format!("cannot delete {}: {e}", path.display()))?;
    println!("deleted preset `{name}`");
    Ok(())
}

fn export(name: &str, out: Option<&Path>) -> Result<(), String> {
    let preset = load_preset(name)?;
    let body = serde_json::to_string_pretty(&preset).map_err(|e| e.to_string())?;
    match out {
        Some(path) => {
            std::fs::write(path, format!("{body}\n"))
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            println!("exported preset `{name}` to {}", path.display());
        }
        None => println!("{body}"),
    }
    Ok(())
}

fn import(file: &Path) -> Result<(), String> {
    let preset = parse_preset(file, false)?;
    let path = preset_path(&preset.name)?;
    if path.exists() {
        return Err(format!(
            "preset `{}` already exists; `vikram presets delete {}` first to replace it",
            preset.name, preset.name
        ));
    }
    let raw = std::fs::read(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    std::fs::create_dir_all(presets_dir())
        .map_err(|e| format!("cannot create {}: {e}", presets_dir().display()))?;
    std::fs::write(&path, &raw).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("imported preset `{}` to {}", preset.name, path.display());
    Ok(())
}

// ---- suggest -------------------------------------------------------------------

/// Leaf key names that count as config-shaped when found in recorded data.
fn is_signal_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "model"
            | "provider"
            | "base_url"
            | "fallback_model"
            | "memory_provider"
            | "api_key_env"
            | "context_window"
            | "workspace_root"
            | "max_steps"
            | "max_output_tokens"
            | "stream_silence_secs"
            | "learning_review_every"
            | "memory_nudge_interval"
            | "title_model"
            | "ascii_fallback"
            | "thinking"
            | "mode"
            | "auto_compact"
            | "trigger_percent"
            | "keep_recent"
            | "auto_compact_limit"
            | "safety_margin_percent"
            | "tool_output_ceiling"
            | "memory_retrieval"
            | "memory_budget_tokens"
            | "project_budget_tokens"
            | "delegation_budget_tokens"
            | "hive_budget_tokens"
            | "backend"
            | "brave_endpoint"
            | "bing_endpoint"
    )
}

fn scalar_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Walk a JSON value; every object member whose key names a presettable field
/// and whose value is a scalar becomes one observation.
fn collect_signals(
    value: &serde_json::Value,
    counts: &mut BTreeMap<String, BTreeMap<String, usize>>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                if is_signal_key(key)
                    && let Some(text) = scalar_text(item)
                {
                    *counts
                        .entry(key.to_ascii_lowercase())
                        .or_default()
                        .entry(text)
                        .or_default() += 1;
                }
                collect_signals(item, counts);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_signals(item, counts);
            }
        }
        _ => {}
    }
}

/// `key`/`content` of a learning proposal counts when the key is config-shaped
/// (`config.model`, `preset.context_window`, or a bare allowlisted name); the
/// observed value is the proposal's content, trimmed to one line.
fn collect_learning_signals(
    proposal: &vikram_runtime::learning::LearningProposal,
    counts: &mut BTreeMap<String, BTreeMap<String, usize>>,
) {
    let key = proposal.key.trim();
    let field = key
        .strip_prefix("config.")
        .or_else(|| key.strip_prefix("preset."))
        .unwrap_or(key);
    if !is_signal_key(field) {
        return;
    }
    let value = proposal
        .content
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(120)
        .collect::<String>();
    if value.is_empty() {
        return;
    }
    *counts
        .entry(field.to_ascii_lowercase())
        .or_default()
        .entry(value)
        .or_default() += 1;
}

fn suggest_at(db: &Path, skills_root: &Path, learning_path: &Path) -> Result<Vec<String>, String> {
    let mut counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut experiences = 0usize;
    let mut evolution_proposals = 0usize;
    let mut learning_proposals = 0usize;

    if db.exists() {
        let engine =
            vikram_evolution::EvolutionEngine::open(db.to_path_buf(), skills_root.to_path_buf())
                .map_err(|e| e.to_string())?;
        for exp in engine
            .experience
            .list(None, 500)
            .map_err(|e| e.to_string())?
        {
            experiences += 1;
            collect_signals(&exp.metadata, &mut counts);
        }
        for proposal in engine
            .store
            .list_proposals(None)
            .map_err(|e| e.to_string())?
        {
            evolution_proposals += 1;
            collect_signals(&proposal.details, &mut counts);
            collect_signals(
                &serde_json::json!({"summary": proposal.summary}),
                &mut counts,
            );
        }
    }

    let manager =
        vikram_runtime::learning::LearningManager::open(Some(learning_path.to_path_buf()));
    for proposal in manager.all_proposals() {
        learning_proposals += 1;
        collect_learning_signals(&proposal, &mut counts);
    }

    let mut out = Vec::new();
    if counts.is_empty() {
        out.push(format!(
            "no recurring model/provider/context patterns found \
             (scanned {experiences} experiences, {evolution_proposals} evolution proposals, \
             {learning_proposals} learning proposals)"
        ));
        return Ok(out);
    }

    out.push(format!(
        "config patterns observed ({} experiences, {} evolution proposals, {} learning proposals):",
        experiences, evolution_proposals, learning_proposals
    ));
    let mut sets = Vec::new();
    for (field, values) in &counts {
        let (value, seen) = values
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(v, c)| (v.as_str(), *c))
            .unwrap_or(("", 0));
        if value.is_empty() {
            continue;
        }
        out.push(format!("  {field} = {value:?}  (seen {seen} time(s))"));
        // Values with spaces or quotes cannot survive `--set key=value` parsing
        // honestly; suggest the create flags only for simple scalars.
        if !value.contains(char::is_whitespace) && !value.contains('"') {
            sets.push(format!("--set {field}={value}"));
        }
    }
    if !sets.is_empty() {
        out.push(format!(
            "suggested preset (review before creating):\n  vikram presets create suggested \\\n    {}",
            sets.join(" \\\n    ")
        ));
    }
    out.push("nothing was written; `vikram presets suggest` only reports".to_owned());
    Ok(out)
}

fn suggest() -> Result<(), String> {
    let home = vikram_home();
    let db = home.join("evolution.db");
    let learning = home.join("learning.json");
    if !db.exists() && !learning.exists() {
        return Err(format!(
            "nothing recorded yet — no {} and no {}",
            db.display(),
            learning.display()
        ));
    }
    for line in suggest_at(&db, &home.join("skills"), &learning)? {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    fn home_guard(dir: &Path) -> MutexGuard<'static, ()> {
        let guard = vikram_runtime::config::lock_security_env();
        unsafe { std::env::set_var("VIKRAM_HOME", dir) };
        guard
    }

    fn preset_json(name: &str, config: serde_json::Value) -> String {
        serde_json::json!({
            "name": name,
            "version": 1,
            "created_ms": 1_700_000_000_000u64,
            "config": config,
        })
        .to_string()
    }

    fn write_config(home: &Path, model: &str) {
        let config = Config {
            model: model.to_owned(),
            max_steps: 10,
            ..Config::default()
        };
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(
            home.join("config.json"),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn create_list_show_apply_delete_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = home_guard(tmp.path());
        write_config(tmp.path(), "gpt-old");

        create(
            "work",
            false,
            &["model=gpt-new".to_owned(), "max_steps=42".to_owned()],
            None,
        )
        .unwrap();
        let path = tmp.path().join("presets/work.json");
        assert!(path.is_file());

        list().unwrap();
        show("work").unwrap();

        // Apply merges only the managed fields; the backup keeps the old file.
        apply("work", true).unwrap();
        let applied = Config::load().unwrap();
        assert_eq!(applied.model, "gpt-new");
        assert_eq!(applied.max_steps, 42);
        assert!(
            tmp.path().read_dir().unwrap().flatten().any(|e| e
                .file_name()
                .to_string_lossy()
                .starts_with("config.json.bak-")),
            "expected a timestamped config backup"
        );

        delete("work").unwrap();
        assert!(!path.exists());
        assert!(load_preset("work").is_err());
    }

    #[test]
    fn from_current_captures_the_durable_config() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = home_guard(tmp.path());
        write_config(tmp.path(), "gpt-snapshot");

        create("snap", true, &[], Some("a note")).unwrap();
        let preset = load_preset("snap").unwrap();
        assert_eq!(preset.config.model.as_deref(), Some("gpt-snapshot"));
        assert_eq!(preset.notes.as_deref(), Some("a note"));
        // api_key_env is a name, never a key; the whole context block came along.
        assert!(preset.config.context.is_some());
        assert!(preset.config.api_key_env.is_some());
    }

    #[test]
    fn invalid_and_secret_keys_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = home_guard(tmp.path());

        let err = create("bad", false, &["api_key=sk-secret".to_owned()], None).unwrap_err();
        assert!(err.contains("unknown preset key"), "{err}");
        let err = create(
            "bad",
            false,
            &["security.disable_yolo=true".to_owned()],
            None,
        )
        .unwrap_err();
        assert!(err.contains("unknown preset key"), "{err}");
        let err = create("bad", false, &["model".to_owned()], None).unwrap_err();
        assert!(err.contains("KEY=VALUE"), "{err}");
        let err = create("bad", false, &[], None).unwrap_err();
        assert!(err.contains("nothing to capture"), "{err}");
        let err = create("../escape", false, &["model=x".to_owned()], None).unwrap_err();
        assert!(err.contains("invalid preset name"), "{err}");
    }

    #[test]
    fn malformed_and_unknown_field_presets_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = home_guard(tmp.path());
        let dir = tmp.path().join("presets");
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(
            dir.join("wrongname.json"),
            preset_json("other", serde_json::json!({"model": "x"})),
        )
        .unwrap();
        assert!(load_preset("wrongname").is_err());

        std::fs::write(
            dir.join("future.json"),
            preset_json("future", serde_json::json!({"model": "x"}))
                .replace("\"version\":1", "\"version\":99"),
        )
        .unwrap();
        assert!(load_preset("future").is_err());

        std::fs::write(
            dir.join("sneaky.json"),
            preset_json(
                "sneaky",
                serde_json::json!({"model": "x", "api_key": "sk-live"}),
            ),
        )
        .unwrap();
        let err = load_preset("sneaky").unwrap_err();
        assert!(err.contains("not a valid preset"), "{err}");
    }

    #[test]
    fn export_and_import_round_trip_and_import_validates() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = home_guard(tmp.path());
        create("portable", false, &["model=gpt-x".to_owned()], None).unwrap();

        let out = tmp.path().join("out.json");
        export("portable", Some(&out)).unwrap();
        delete("portable").unwrap();
        import(&out).unwrap();
        assert!(load_preset("portable").is_ok());

        // A second import of the same preset is refused, not clobbered.
        assert!(import(&out).is_err());

        let bogus = tmp.path().join("bogus.json");
        std::fs::write(&bogus, "{\"name\":\"x\"}").unwrap();
        assert!(import(&bogus).is_err());
    }

    #[test]
    fn apply_shows_a_diff_and_noops_when_nothing_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = home_guard(tmp.path());
        write_config(tmp.path(), "same");
        create("same", false, &["model=same".to_owned()], None).unwrap();
        // Second apply of an identical preset reports no work instead of
        // writing another backup.
        apply("same", true).unwrap();
    }

    #[test]
    fn suggest_reports_seeded_patterns_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = home_guard(tmp.path());
        let home = tmp.path();
        let db = home.join("evolution.db");
        let skills = home.join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let engine = vikram_evolution::EvolutionEngine::open(db.clone(), skills.clone()).unwrap();
        engine
            .experience
            .record(vikram_evolution::Experience {
                id: String::new(),
                timestamp_ms: 0,
                agent_id: "main".to_owned(),
                project_id: None,
                task_category: "chat".to_owned(),
                user_request: "hi".to_owned(),
                outcome: vikram_evolution::ExperienceOutcome::Success,
                tools_used: vec![],
                errors: vec![],
                corrections: vec![],
                feedback: None,
                reusable: false,
                sensitivity: vikram_evolution::Sensitivity::Private,
                metadata: serde_json::json!({
                    "session": "s1",
                    "config": {"model": "gpt-seeded", "context_window": 200000},
                }),
            })
            .unwrap();

        let lines = suggest_at(&db, &skills, &home.join("learning.json")).unwrap();
        let text = lines.join("\n");
        assert!(text.contains("model"), "{text}");
        assert!(text.contains("gpt-seeded"), "{text}");
        assert!(text.contains("context_window"), "{text}");
        assert!(text.contains("nothing was written"), "{text}");
        assert!(!home.join("presets").exists());
    }

    #[test]
    fn suggest_picks_up_config_signals_the_learning_pipeline_records() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = home_guard(tmp.path());
        let home = tmp.path();
        let db = home.join("evolution.db");
        let skills = home.join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let engine = vikram_evolution::EvolutionEngine::open(db.clone(), skills.clone()).unwrap();
        // The producer side: a user correction mentioning a model and a context
        // size becomes `metadata.config.*`, exactly as `record_and_reflect` writes it.
        let signals = vikram_runtime::learning::config_signals(&[
            "no, use gpt-4o instead".to_owned(),
            "no, the context window of 200k was too small".to_owned(),
        ]);
        engine
            .experience
            .record(vikram_evolution::Experience {
                id: String::new(),
                timestamp_ms: 0,
                agent_id: "main".to_owned(),
                project_id: None,
                task_category: "chat".to_owned(),
                user_request: "hi".to_owned(),
                outcome: vikram_evolution::ExperienceOutcome::Success,
                tools_used: vec![],
                errors: vec![],
                corrections: vec!["no, use gpt-4o instead".to_owned()],
                feedback: None,
                reusable: false,
                sensitivity: vikram_evolution::Sensitivity::Private,
                metadata: serde_json::json!({
                    "session": "s1",
                    "config": signals,
                }),
            })
            .unwrap();

        let lines = suggest_at(&db, &skills, &home.join("learning.json")).unwrap();
        let text = lines.join("\n");
        assert!(text.contains("model"), "{text}");
        assert!(text.contains("gpt-4o"), "{text}");
        assert!(text.contains("context_window"), "{text}");
        assert!(text.contains("200000"), "{text}");
        assert!(text.contains("--set model=gpt-4o"), "{text}");
    }

    #[test]
    fn suggest_is_honest_when_nothing_is_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let lines = suggest_at(
            &tmp.path().join("missing.db"),
            &tmp.path().join("skills"),
            &tmp.path().join("learning.json"),
        )
        .unwrap();
        assert!(lines[0].contains("no recurring"), "{lines:?}");
    }
}
