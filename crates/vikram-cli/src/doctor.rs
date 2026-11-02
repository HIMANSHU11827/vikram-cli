//! Local prerequisite checks. Nothing here prints a secret: only whether it is present.
//!
//! The checks are returned as data rather than printed, so `/doctor` can draw them in a panel and
//! `vikram doctor` can print them — one set of checks, two surfaces, no drift.

use vikram_runtime::config::{McpConfig, database_path, mcp_config_path, plugins_dir};
use vikram_runtime::{Config, McpRegistry, PluginRegistry};

/// How a check went. `Fail` is the only level that makes `vikram doctor` exit unhappily.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
    Info,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Info => "info",
        }
    }

    pub fn is_failure(self) -> bool {
        matches!(self, Self::Fail)
    }
}

/// One checked thing: the level, and the sentence the operator reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub level: Level,
    pub text: String,
}

impl Check {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Self {
            level,
            text: text.into(),
        }
    }
}

/// Every check, in the order an operator should read them: can we store anything, is there a model,
/// what posture are we in, what credentials exist, what is attached.
pub fn checks(config: &Config) -> Vec<Check> {
    let mut out = Vec::new();

    match vikram_runtime::config::ensure_home() {
        Ok(_) => out.push(Check::new(
            Level::Ok,
            format!(
                "runtime home writable: {}",
                vikram_runtime::config::vikram_home().display()
            ),
        )),
        Err(err) => out.push(Check::new(Level::Fail, format!("runtime home: {err}"))),
    }

    match vikram_store::Store::open(&database_path()) {
        Ok(store) => {
            let sessions = store.list_sessions().map(|s| s.len()).unwrap_or(0);
            let agents = store.list_agents().map(|a| a.len()).unwrap_or(0);
            out.push(Check::new(
                Level::Ok,
                format!(
                    "store {} · {sessions} session(s), {agents} agent(s)",
                    database_path().display()
                ),
            ));
        }
        Err(err) => out.push(Check::new(Level::Fail, format!("store: {err}"))),
    }

    if config.model.trim().is_empty() {
        out.push(Check::new(
            Level::Warn,
            "no model configured (`vikram config set model <id>`)",
        ));
    } else {
        out.push(Check::new(Level::Ok, format!("model: {}", config.model)));
    }

    // The posture a run would actually start in, and the layer that decided it.
    let availability = vikram_runtime::config::ModeAvailability::resolve(&config.security);
    let start = if availability.permits(config.mode) {
        config.mode
    } else {
        availability.fallback_mode()
    };
    let detail = match availability.refusal(config.mode) {
        Some((rule, layer)) if start != config.mode => format!(
            " ({} refused by {layer}: {rule})",
            config.mode.as_str()
        ),
        Some((rule, layer)) => format!(" (restricted by {layer}: {rule})"),
        None => String::new(),
    };
    out.push(Check::new(
        Level::Info,
        format!("mode: {}{detail}", start.as_str()),
    ));
    if availability.always_allow_refusal().is_some() {
        out.push(Check::new(
            Level::Info,
            "approvals cannot be saved (`security.disable_always_allow`)",
        ));
    }

    let book = vikram_runtime::RuleBook::open();
    out.push(Check::new(
        Level::Info,
        format!(
            "allow-list: {} saved ({})",
            book.rules().len(),
            book.path().display()
        ),
    ));
    if let Some(problem) = book.error() {
        out.push(Check::new(Level::Warn, format!("allow-list: {problem}")));
    }

    match config.api_key_with_source() {
        Some((_, source)) => out.push(Check::new(Level::Ok, format!("api key present ({source})"))),
        None => out.push(Check::new(
            Level::Warn,
            format!(
                "api key missing: export ${} or add it to {}",
                config.api_key_env,
                vikram_runtime::config::vikram_home().join(".env").display()
            ),
        )),
    }

    let mcp = McpConfig::load();
    out.push(Check::new(
        Level::Info,
        format!(
            "mcp: {} configured ({})",
            mcp.servers.len(),
            mcp_config_path().display()
        ),
    ));
    if !mcp.servers.is_empty() {
        let registry = McpRegistry::load(mcp);
        out.push(Check::new(
            Level::Info,
            format!("configured servers: {}", registry.configured_count()),
        ));
    }

    let plugins = PluginRegistry::from_default_dir();
    out.push(Check::new(
        Level::Info,
        format!(
            "plugins: {} installed, {} loaded ({})",
            plugins.installed(),
            plugins.loaded(),
            plugins_dir().display()
        ),
    ));

    out.push(Check::new(Level::Info, format!("terminal: {}", terminal_size())));
    out
}

pub async fn run(config: Config) -> Result<(), String> {
    println!("VIKRAM doctor");
    println!("home: {}", vikram_runtime::config::vikram_home().display());
    let checks = checks(&config);
    for check in &checks {
        println!("  {:4} {}", check.level.label(), check.text);
    }
    let problems: Vec<&str> = checks
        .iter()
        .filter(|check| check.level.is_failure())
        .map(|check| check.text.as_str())
        .collect();
    if problems.is_empty() {
        println!("doctor: no blocking problems");
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

fn terminal_size() -> String {
    let cols = std::env::var("COLUMNS").unwrap_or_else(|_| "?".into());
    let term = std::env::var("TERM").unwrap_or_else(|_| "unknown".into());
    format!("{cols} columns, TERM={term}")
}
