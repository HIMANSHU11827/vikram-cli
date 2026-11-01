//! Local prerequisite checks. Nothing here prints a secret: only whether it is present.

use vikram_runtime::config::{McpConfig, database_path, mcp_config_path, plugins_dir};
use vikram_runtime::{Config, McpRegistry, PluginRegistry};

pub async fn run(config: Config) -> Result<(), String> {
    let mut problems = Vec::new();
    println!("VIKRAM doctor");
    println!("home: {}", vikram_runtime::config::vikram_home().display());

    match vikram_runtime::config::ensure_home() {
        Ok(_) => println!("  ok   runtime home writable"),
        Err(err) => {
            println!("  fail runtime home: {err}");
            problems.push("runtime home is not writable".to_owned());
        }
    }

    match vikram_store::Store::open(&database_path()) {
        Ok(store) => {
            let sessions = store.list_sessions().map(|s| s.len()).unwrap_or(0);
            let agents = store.list_agents().map(|a| a.len()).unwrap_or(0);
            println!(
                "  ok   store {} ({sessions} sessions, {agents} agents)",
                database_path().display()
            );
        }
        Err(err) => {
            println!("  fail store: {err}");
            problems.push("store cannot be opened".to_owned());
        }
    }

    if config.model.trim().is_empty() {
        println!("  warn no model configured (`vikram config set model <id>`)");
    } else {
        println!("  ok   model: {}", config.model);
    }

    // The posture a run would actually start in, and the layer that decided it.
    let availability = vikram_runtime::config::ModeAvailability::resolve(&config.security);
    let start = if availability.permits(config.mode) {
        config.mode
    } else {
        availability.fallback_mode()
    };
    let refused = availability.refusal(config.mode);
    println!(
        "  info mode: {}{}",
        start.as_str(),
        match refused {
            Some((rule, layer)) if start != config.mode =>
                format!(" ({} refused by {layer}: {rule})", config.mode.as_str()),
            Some((rule, layer)) => format!(" (restricted by {layer}: {rule})"),
            None => String::new(),
        }
    );
    if availability.always_allow_refusal().is_some() {
        println!("       approvals cannot be saved (`security.disable_always_allow`)");
    }
    let book = vikram_runtime::RuleBook::open();
    println!(
        "       allow-list: {} saved ({})",
        book.rules().len(),
        book.path().display()
    );
    if let Some(problem) = book.error() {
        println!("  warn allow-list: {problem}");
    }

    match config.api_key_with_source() {
        Some((_, source)) => println!("  ok   api key present ({source})"),
        None => println!(
            "  warn api key missing: export ${} or add it to {}",
            config.api_key_env,
            vikram_runtime::config::vikram_home().join(".env").display()
        ),
    }

    let mcp = McpConfig::load();
    println!(
        "  info mcp: {} configured (config: {})",
        mcp.servers.len(),
        mcp_config_path().display()
    );
    if !mcp.servers.is_empty() {
        let registry = McpRegistry::load(mcp);
        println!("       configured servers: {}", registry.configured_count());
    }

    let plugins = PluginRegistry::from_default_dir();
    println!(
        "  info plugins: {} installed, {} loaded ({})",
        plugins.installed(),
        plugins.loaded(),
        plugins_dir().display()
    );

    let size = agents_terminal();
    println!("  info terminal: {size}");

    if problems.is_empty() {
        println!("doctor: no blocking problems");
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

fn agents_terminal() -> String {
    let cols = std::env::var("COLUMNS").unwrap_or_else(|_| "?".into());
    let term = std::env::var("TERM").unwrap_or_else(|_| "unknown".into());
    format!("{cols} columns, TERM={term}")
}
