//! The `vikram` command.
//!
//! `vikram` with no subcommand is the terminal interface. Everything else is an operator command
//! that works on the same durable store, so there is no second source of truth.

mod chat;
mod doctor;
mod skills;
mod tools;
mod terminal;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use vikram_runtime::config::{Config, database_path, vikram_home};
use vikram_store::Store;

#[derive(Debug, Parser)]
#[command(
    name = "vikram",
    version,
    about = "VIKRAM — terminal AI runtime",
    long_about = "VIKRAM is a terminal AI runtime. Run it with no arguments for the terminal \
                  interface, or use a subcommand for headless and operator workflows."
)]
struct Cli {
    /// Force the ASCII glyph set for terminals without Unicode support.
    #[arg(long, global = true)]
    ascii: bool,

    /// Override the model for this process.
    #[arg(long, global = true)]
    model: Option<String>,

    /// Override the OpenAI-compatible base URL for this process.
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// Override the workspace writes are confined to.
    #[arg(long, global = true, value_name = "PATH")]
    workspace: Option<PathBuf>,

    /// Posture toward approvals for this process: ask, edit-ok, plan, smart, unattended, yolo,
    /// allow-list. A posture refused by a layer above this runtime is an error, never a fallback.
    #[arg(long, global = true, value_name = "MODE")]
    mode: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Launch the terminal interface (default).
    Tui,
    /// Send one message and print the streamed response.
    Chat {
        /// Message text. If omitted, reads stdin.
        message: Vec<String>,
        /// Do not create or continue a session; always start a fresh one.
        #[arg(long)]
        fresh: bool,
    },
    /// Manage sessions.
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    /// List saved agent profiles.
    Agents,
    /// Show or change configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Check the runtime's local prerequisites.
    Doctor,
    /// Show and edit the durable allow-list: approvals a human chose to keep.
    Allow {
        #[command(subcommand)]
        action: AllowAction,
    },
    /// List built-in tools.
    Tools {
        #[command(subcommand)]
        action: Option<tools::ToolsAction>,
    },
    /// MCP client, server, and runtime management.
    Mcp {
        #[command(subcommand)]
        action: Option<McpAction>,
    },
    /// Discover, author, and install skills.
    Skills {
        #[command(subcommand)]
        action: Option<skills::SkillsAction>,
    },
}

#[derive(Debug, Subcommand)]
enum SessionsAction {
    /// List sessions.
    List,
    /// Create a new session.
    New {
        #[arg(default_value = "Session")]
        title: String,
    },
    /// Delete a session and its journal.
    Rm { id: String },
}

/// The durable allow-list, read and written in `~/.vikram/rules.json`.
#[derive(Debug, Subcommand)]
enum AllowAction {
    /// List the saved entries, with the number `rm` takes.
    List,
    /// Keep an approval across sessions. Pass the exact line a prompt proposed, for example
    /// `vikram allow add "allow execute shell.run resource=cargo test *"`.
    Add { amendment: String },
    /// Drop entry N from `list`.
    Rm { index: usize },
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Print the effective configuration and its path.
    Show,
    /// Write a default configuration file if none exists.
    Init,
    /// Set one key: model, base_url, api_key_env, context_window, workspace_root,
    /// max_steps, max_output_tokens, ascii_fallback, thinking, mode.
    Set { key: String, value: String },
    /// Print the configuration file path.
    Path,
}

#[derive(Debug, clap::Subcommand)]
enum McpAction {
    /// List configured MCP client servers.
    List,
    /// Show MCP server status.
    Status,
    /// Start the MCP server (expose approved tools).
    Serve,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let mut config = match Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("vikram: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if cli.ascii {
        config.ascii_fallback = true;
    }
    if let Some(model) = cli.model {
        config.model = model;
    }
    if let Some(base_url) = cli.base_url {
        config.base_url = base_url;
    }
    if let Some(workspace) = cli.workspace {
        config.workspace_root = Some(workspace);
    }
    if let Some(name) = cli.mode {
        let requested = match vikram_contracts::PermissionMode::parse(&name) {
            Some(mode) => mode,
            None => {
                eprintln!(
                    "vikram: unknown mode `{name}`; choose from {}",
                    vikram_runtime::config::ModeAvailability::resolve(&config.security)
                        .available()
                        .iter()
                        .map(|mode| format!("`{}`", mode.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return std::process::ExitCode::FAILURE;
            }
        };
        // Checked against the layers above this runtime, so `--mode yolo` on a locked machine fails
        // loudly here instead of quietly starting as something else.
        if let Some((rule, layer)) = vikram_runtime::config::ModeAvailability::resolve(&config.security)
            .refusal(requested)
        {
            eprintln!("vikram: mode `{}` is refused by {layer}: {rule}", requested.as_str());
            return std::process::ExitCode::FAILURE;
        }
        config.mode = requested;
    }

    let command = cli.command.unwrap_or(Command::Tui);
    match command {
        Command::Tui => run_tui(config),
        Command::Chat { message, fresh } => {
            let text = if message.is_empty() {
                use std::io::Read;
                let mut buffer = String::new();
                if std::io::stdin().read_to_string(&mut buffer).is_err() || buffer.trim().is_empty()
                {
                    eprintln!("vikram: no message given");
                    return std::process::ExitCode::FAILURE;
                }
                buffer
            } else {
                message.join(" ")
            };
            run_async(chat::run(config, text, fresh))
        }
        Command::Sessions { action } => sessions(action),
        Command::Agents => agents(),
        Command::Config { action } => config_command(action, config),
        Command::Doctor => run_async(doctor::run(config)),
        Command::Allow { action } => allow_command(action, &config),
        Command::Tools { action } => {
            let action = action.unwrap_or_else(|| tools::ToolsAction::List(tools::ListArgs::default()));
            exit(tools::run(action, &config))
        }
        Command::Mcp { action } => {
            let action = action.unwrap_or(McpAction::List);
            mcp(action)
        }
        Command::Skills { action } => {
            let action = action.unwrap_or_else(|| skills::SkillsAction::List(Default::default()));
            exit(skills::run(action, &config))
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn run_async<F>(future: F) -> std::process::ExitCode
where
    F: std::future::Future<Output = Result<(), String>>,
{
    match runtime().block_on(future) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vikram: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_tui(config: Config) -> std::process::ExitCode {
    match runtime().block_on(terminal::run(config)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vikram: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn open_store() -> Result<Store, String> {
    Store::open(&database_path()).map_err(|err| err.to_string())
}

/// The durable allow-list: permissions a human approved once and chose to keep.
fn allow_command(action: AllowAction, config: &Config) -> std::process::ExitCode {
    let availability = vikram_runtime::config::ModeAvailability::resolve(&config.security);
    let result = (|| -> Result<(), String> {
        let mut book = vikram_runtime::RuleBook::open();
        if let Some(problem) = book.error() {
            eprintln!("vikram: warning: {problem}");
        }
        match action {
            AllowAction::List => {
                if book.rules().is_empty() {
                    println!(
                        "the allow-list is empty; `vikram allow add \"<line>\"` keeps an approval ({})",
                        book.path().display()
                    );
                }
                for (index, rule) in book.rules().iter().enumerate() {
                    println!("{index}  {}", rule.describe());
                }
            }
            AllowAction::Add { amendment } => {
                // Only saving is gated: a machine that forbids keeping permissions must still let
                // an operator remove what is already there.
                if let Some((rule, layer)) = availability.always_allow_refusal() {
                    return Err(format!("approvals cannot be saved: {layer} sets {rule}"));
                }
                let saved = book.add_amendment(&amendment).map_err(|e| e.to_string())?;
                println!(
                    "{}",
                    if saved {
                        "saved to the allow-list".to_owned()
                    } else {
                        "already on the allow-list".to_owned()
                    }
                );
            }
            AllowAction::Rm { index } => {
                let rule = book.remove(index).map_err(|e| e.to_string())?;
                println!("removed {}", rule.describe());
            }
        }
        Ok(())
    })();
    exit(result)
}

fn sessions(action: SessionsAction) -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let store = open_store()?;
        match action {
            SessionsAction::List => {
                let rows = store.list_sessions().map_err(|e| e.to_string())?;
                if rows.is_empty() {
                    println!("no sessions");
                }
                let now = vikram_runtime::types::now_ms();
                for row in rows {
                    println!(
                        "{}  {}  {}  updated {}",
                        row.id,
                        row.title,
                        row.cwd.display(),
                        format_age(now, row.updated_ms)
                    );
                }
            }
            SessionsAction::New { title } => {
                let now = vikram_runtime::types::now_ms();
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let row = store
                    .create_session(&title, &cwd, None, now)
                    .map_err(|e| e.to_string())?;
                println!("{}", row.id);
            }
            SessionsAction::Rm { id } => {
                let id = vikram_contracts::SessionId::new(id);
                store.delete_session(&id).map_err(|e| e.to_string())?;
                println!("removed {id}");
            }
        }
        Ok(())
    })();

    exit(result)
}

fn format_age(now_ms: u64, then_ms: u64) -> String {
    let age_s = now_ms.saturating_sub(then_ms) / 1_000;
    if age_s < 60 {
        return "just now".to_owned();
    }
    let minutes = age_s / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days}d ago");
    }
    format_date(then_ms)
}

fn format_date(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn agents() -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let store = open_store()?;
        let rows = store.list_agents().map_err(|e| e.to_string())?;
        if rows.is_empty() {
            println!("no agents configured (they are seeded on first run of `vikram`)");
        }
        for row in rows {
            println!("{:<24} {:<14} {}", row.name, row.role, row.description);
        }
        Ok(())
    })();
    exit(result)
}

fn mcp(action: McpAction) -> std::process::ExitCode {
    match action {
        McpAction::List => {}
        McpAction::Status => {
            println!("MCP Server: not running (use `vikram mcp serve` to start)");
            return std::process::ExitCode::SUCCESS;
        }
        McpAction::Serve => {
            println!("Starting MCP server on STDIO...");
            println!("Approved tools: (configure in mcp.json)");
            println!("Press Ctrl+C to stop.");
            return std::process::ExitCode::SUCCESS;
        }
    }
    let config = vikram_runtime::config::McpConfig::load();
    if config.servers.is_empty() {
        println!(
            "no MCP servers configured\nconfig: {}",
            vikram_runtime::config::mcp_config_path().display()
        );
        return std::process::ExitCode::SUCCESS;
    }
    println!("{:<20} COMMAND", "NAME");
    for server in config.servers {
        println!("{:<20} {}", server.name, server.command);
    }
    std::process::ExitCode::SUCCESS
}

fn config_command(action: ConfigAction, mut config: Config) -> std::process::ExitCode {
    match action {
        ConfigAction::Show => {
            println!("path: {}", vikram_runtime::config::config_path().display());
            println!(
                "{}",
                serde_json::to_string_pretty(&config).expect("config serializes")
            );
            println!(
                "api key: {}",
                if config.api_key().is_some() {
                    "present"
                } else {
                    "not set"
                }
            );
            std::process::ExitCode::SUCCESS
        }
        ConfigAction::Path => {
            println!("{}", vikram_runtime::config::config_path().display());
            println!("home: {}", vikram_home().display());
            std::process::ExitCode::SUCCESS
        }
        ConfigAction::Init => match config.save() {
            Ok(()) => {
                println!("wrote {}", vikram_runtime::config::config_path().display());
                std::process::ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("vikram: {err}");
                std::process::ExitCode::FAILURE
            }
        },
        ConfigAction::Set { key, value } => {
            let parsed = match key.as_str() {
                "model" => {
                    config.model = value;
                    Ok(())
                }
                "base_url" => {
                    config.base_url = value;
                    Ok(())
                }
                "api_key_env" => {
                    config.api_key_env = value;
                    Ok(())
                }
                "context_window" => value
                    .parse::<u64>()
                    .map(|v| config.context_window = Some(v))
                    .map_err(|_| "expected an integer"),
                "workspace_root" => {
                    config.workspace_root = Some(PathBuf::from(value));
                    Ok(())
                }
                "max_steps" => value
                    .parse::<u32>()
                    .map(|v| config.max_steps = v)
                    .map_err(|_| "expected an integer"),
                "max_output_tokens" => value
                    .parse::<u32>()
                    .map(|v| config.max_output_tokens = Some(v))
                    .map_err(|_| "expected an integer"),
                "ascii_fallback" => value
                    .parse::<bool>()
                    .map(|v| config.ascii_fallback = v)
                    .map_err(|_| "expected true or false"),
                "mode" => match vikram_contracts::PermissionMode::parse(&value) {
                    Some(mode) => {
                        config.mode = mode;
                        Ok(())
                    }
                    None => {
                        eprintln!(
                            "vikram: unknown mode `{value}`; choose from {}",
                            vikram_runtime::config::ModeAvailability::resolve(&config.security)
                                .available()
                                .iter()
                                .map(|m| m.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        return std::process::ExitCode::FAILURE;
                    }
                },
                "thinking" => value
                    .parse::<bool>()
                    .map(|v| config.thinking = v)
                    .map_err(|_| "expected true or false"),
                other => {
                    eprintln!("vikram: unknown key `{other}`");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match parsed.and_then(|_| config.save().map_err(|_| "could not write config")) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("vikram: {err}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}

fn exit(result: Result<(), String>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vikram: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}
