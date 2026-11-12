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
            let bots = store.list_bots().map(|b| b.len()).unwrap_or(0);
            out.push(Check::new(
                Level::Ok,
                format!(
                    "store {} · {sessions} session(s), {bots} bot(s)",
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
        Some((rule, layer)) if start != config.mode => {
            format!(" ({} refused by {layer}: {rule})", config.mode.as_str())
        }
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
    let mcp_enabled = mcp.servers.iter().filter(|server| server.enabled).count();
    out.push(Check::new(
        Level::Info,
        format!(
            "mcp: {} configured, {} enabled ({})",
            mcp.servers.len(),
            mcp_enabled,
            mcp_config_path().display()
        ),
    ));
    if !mcp.servers.is_empty() {
        let registry = McpRegistry::load(mcp.into_specs());
        out.push(Check::new(
            Level::Info,
            format!("enabled servers: {}", registry.configured_count()),
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

    // External tooling the runtime shells out to. `node` and `python` are optional (only some
    // features need them); everything else here is required for the corresponding capability.
    check_tool(&mut out, "git", &["git"], true);
    check_tool(&mut out, "cargo", &["cargo"], true);
    check_tool(&mut out, "rustc", &["rustc"], true);
    check_tool(&mut out, "node", &["node"], false);
    check_tool(&mut out, "python", &["python", "python3"], false);

    match vikram_runtime::browser::available() {
        Some(path) => out.push(Check::new(
            Level::Ok,
            format!("chromium/chrome: {}", path.display()),
        )),
        None => out.push(Check::new(
            Level::Fail,
            format!(
                "chromium/chrome: not found (install: {})",
                dep_hint("chromium")
            ),
        )),
    }

    check_tool(&mut out, "tesseract", &["tesseract"], true);
    check_tool(&mut out, "xdotool", &["xdotool"], true);

    if cfg!(target_os = "linux") {
        check_tool(&mut out, "bwrap", &["bwrap"], true);
    } else {
        out.push(Check::new(
            Level::Info,
            "bwrap: not required on this platform".to_owned(),
        ));
    }

    check_tool(&mut out, "sqlite3", &["sqlite3"], true);

    // Config validity: parse the file on disk rather than trusting the already-loaded value, so a
    // corrupt config is reported here instead of aborting before `vikram doctor` can run.
    let config_path = vikram_runtime::config::config_path();
    if !config_path.exists() {
        out.push(Check::new(
            Level::Ok,
            format!(
                "config valid: {} (defaults; no file)",
                config_path.display()
            ),
        ));
    } else {
        match std::fs::read_to_string(&config_path) {
            Ok(raw) => match serde_json::from_str::<Config>(&raw) {
                Ok(_) => out.push(Check::new(
                    Level::Ok,
                    format!("config valid: {}", config_path.display()),
                )),
                Err(err) => out.push(Check::new(
                    Level::Fail,
                    format!("config invalid: {} ({err})", config_path.display()),
                )),
            },
            Err(err) => out.push(Check::new(
                Level::Fail,
                format!("config unreadable: {} ({err})", config_path.display()),
            )),
        }
    }

    out.push(Check::new(
        Level::Info,
        format!("terminal: {}", terminal_size()),
    ));
    out
}

pub async fn run(config: Config) -> Result<(), String> {
    println!("VIKRAM doctor");
    println!("home: {}", vikram_runtime::config::vikram_home().display());
    let checks = checks(&config);
    for check in &checks {
        let icon = match check.level {
            Level::Ok => "✅",
            Level::Fail => "❌",
            Level::Warn => "⚠️",
            Level::Info => "ℹ️",
        };
        println!("  {} {}", icon, check.text);
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

/// Look up any of the candidate program names on `PATH` and report a pass/fail check.
fn check_tool(out: &mut Vec<Check>, label: &str, candidates: &[&str], required: bool) {
    if let Some(path) = candidates
        .iter()
        .find_map(|candidate| vikram_runtime::computer::which(candidate))
    {
        out.push(Check::new(
            Level::Ok,
            format!("{label}: {}", path.display()),
        ));
    } else {
        let level = if required { Level::Fail } else { Level::Warn };
        out.push(Check::new(
            level,
            format!("{label}: not found (install: {})", dep_hint(label)),
        ));
    }
}

/// Platform-aware install hint for a dependency name.
fn dep_hint(name: &str) -> &'static str {
    if name == "tesseract" || name == "xdotool" {
        return vikram_runtime::computer::install_hint(name);
    }
    if cfg!(target_os = "macos") {
        match name {
            "git" => "brew install git",
            "cargo" | "rustc" => "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh",
            "node" => "brew install node",
            "python" => "brew install python",
            "chromium" => "brew install --cask chromium",
            "bwrap" => "brew install bubblewrap",
            "sqlite3" => "brew install sqlite",
            _ => "install it and keep it on PATH",
        }
    } else if cfg!(target_os = "windows") {
        match name {
            "git" => "choco install git",
            "cargo" | "rustc" => "winget install Rustlang.Rustup",
            "node" => "choco install nodejs",
            "python" => "choco install python",
            "chromium" => "choco install chromium",
            "bwrap" => "install manually",
            "sqlite3" => "choco install sqlite",
            _ => "install it and keep it on PATH",
        }
    } else {
        match name {
            "git" => "apt install git",
            "cargo" | "rustc" => "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh",
            "node" => "apt install nodejs",
            "python" => "apt install python3",
            "chromium" => "apt install chromium (or chromium-browser)",
            "bwrap" => "apt install bubblewrap",
            "sqlite3" => "apt install sqlite3",
            _ => "install it and keep it on PATH",
        }
    }
}

fn terminal_size() -> String {
    let cols = std::env::var("COLUMNS").unwrap_or_else(|_| "?".into());
    let term = std::env::var("TERM").unwrap_or_else(|_| "unknown".into());
    format!("{cols} columns, TERM={term}")
}
