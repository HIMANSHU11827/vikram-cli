//! The `vikram` command.
//!
//! `vikram` with no subcommand is the terminal interface. Everything else is an operator command
//! that works on the same durable store, so there is no second source of truth.

mod chat;
mod doctor;
mod skills;
mod terminal;
mod tools;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
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
    /// Manage project directories sessions can work in.
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Inspect a session's workspace, file changes and checkpoints.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Browse files a session produced.
    Files {
        #[command(subcommand)]
        action: FilesAction,
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
    /// Manage users.
    Users {
        #[command(subcommand)]
        action: UsersAction,
    },
    /// Manage user profiles.
    Profiles {
        #[command(subcommand)]
        action: ProfilesAction,
    },
    /// Manage AI entities (main agents, agents, bots).
    Entities {
        #[command(subcommand)]
        action: EntitiesAction,
    },
    /// Manage Hive group chats.
    Hive {
        #[command(subcommand)]
        action: HiveAction,
    },
    /// Manage unified tickets (Hive, bots, sub-agents).
    Tickets {
        #[command(subcommand)]
        action: TicketsAction,
    },
    /// Manage learning proposals and policies.
    Learning {
        #[command(subcommand)]
        action: LearningAction,
    },
}

#[derive(Debug, Subcommand)]
enum UsersAction {
    /// List users.
    List,
    /// Create a user.
    New { name: String },
    /// Delete a user and their data.
    Rm { id: String },
}

#[derive(Debug, Subcommand)]
enum ProfilesAction {
    /// List profiles (optionally for one user).
    List { user: Option<String> },
    /// Create a profile for a user.
    New { user: String, name: String },
    /// Delete a profile.
    Rm { id: String },
}

#[derive(Debug, Subcommand)]
enum EntitiesAction {
    /// List entities (optionally by kind).
    List { kind: Option<String> },
    /// Create an entity.
    New {
        kind: String,
        name: String,
        slug: String,
        #[arg(default_value = "")]
        title: String,
        #[arg(default_value = "")]
        role: String,
        #[arg(default_value = "")]
        specialization: String,
        #[arg(default_value = "")]
        description: String,
    },
    /// Delete an entity.
    Rm { id: String },
}

#[derive(Debug, Subcommand)]
enum HiveAction {
    /// List Hive sessions.
    List,
    /// Create a Hive session.
    New {
        name: String,
        #[arg(default_value = "")]
        objective: String,
        #[arg(default_value = "cooperative")]
        policy: String,
    },
    /// Show a Hive session's participants and messages.
    Show { id: String },
    /// Add a participant entity to a Hive.
    AddParticipant { hive: String, entity: String },
    /// Remove a participant entity from a Hive.
    RemoveParticipant { hive: String, entity: String },
    /// Send a message to a Hive.
    Send {
        hive: String,
        sender: String,
        content: String,
    },
    /// Run a Hive conversation with an initial prompt.
    Run {
        hive: String,
        message: String,
        #[arg(long)]
        parallel: bool,
    },
    /// Delete a Hive session.
    Rm { id: String },
}

#[derive(Debug, Subcommand)]
enum LearningAction {
    /// Show learning policies and counts.
    Status,
    /// List pending proposals.
    Pending,
    /// Show a proposal by id.
    Show { id: String },
    /// Approve a proposal (apply it).
    Approve { id: String },
    /// Reject a proposal.
    Reject { id: String },
    /// Show audit history.
    History,
    /// Set a policy: memory|skill <auto|approval|suggest|disabled>.
    SetPolicy { kind: String, policy: String },
}
#[derive(Debug, Subcommand)]
enum TicketsAction {
    /// List tickets.
    List {
        kind: Option<String>,
        status: Option<String>,
    },
    /// Show one ticket.
    Show { id: String },
    /// Create a ticket.
    New {
        kind: String,
        title: String,
        #[arg(default_value = "")]
        request: String,
        #[arg(default_value = "owner")]
        owner: String,
    },
    /// Add a participant.
    AddParticipant {
        ticket: String,
        participant: String,
        kind: String,
    },
    /// Send a message.
    Send {
        ticket: String,
        author: String,
        kind: String,
        content: String,
        #[arg(default_value = "shared")]
        visibility: String,
    },
    /// Create an assignment.
    Assign {
        ticket: String,
        assignee: String,
        kind: String,
        title: String,
    },
    /// Complete an assignment.
    Complete { assignment: String, result: String },
    /// Close a ticket.
    Close { id: String },
    /// Reopen a ticket.
    Reopen { id: String },
    /// Delete a ticket.
    Rm { id: String },
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
    /// Show one session's metadata, workspace and project.
    Show { id: String },
    /// Rename a session.
    Rename { id: String, title: String },
    /// Archive a session (kept, hidden from default listings).
    Archive { id: String },
    /// Duplicate a session's metadata and conversation.
    Duplicate { id: String },
    /// Delete a session, its journal and its metadata. Project files are never touched.
    Rm { id: String },
}

#[derive(Debug, Subcommand)]
enum ProjectAction {
    /// List managed projects.
    List,
    /// Associate an existing directory (internal or external) with a session.
    Select { session: String, path: PathBuf },
    /// Show the project a session is associated with.
    Show { session: String },
    /// Disconnect a session from its project (falls back to its session workspace).
    Disconnect { session: String },
}

#[derive(Debug, Subcommand)]
enum WorkspaceAction {
    /// Show a session's workspace path and mode.
    Show { session: String },
    /// List a session's file changes and checkpoints.
    Changes { session: String },
    /// Restore a file from its most recent checkpoint.
    Restore { session: String, path: PathBuf },
}

#[derive(Debug, Subcommand)]
enum FilesAction {
    /// List artifacts for one session, or across all sessions when omitted.
    List { session: Option<String> },
    /// Remove an artifact record (never deletes the file).
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
        if let Some((rule, layer)) =
            vikram_runtime::config::ModeAvailability::resolve(&config.security).refusal(requested)
        {
            eprintln!(
                "vikram: mode `{}` is refused by {layer}: {rule}",
                requested.as_str()
            );
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
        Command::Project { action } => projects(action),
        Command::Workspace { action } => workspace_cmd(action),
        Command::Files { action } => files(action),
        Command::Agents => agents(),
        Command::Config { action } => config_command(action, config),
        Command::Doctor => run_async(doctor::run(config)),
        Command::Allow { action } => allow_command(action, &config),
        Command::Tools { action } => {
            let action =
                action.unwrap_or_else(|| tools::ToolsAction::List(tools::ListArgs::default()));
            exit(tools::run(action, &config))
        }
        Command::Mcp { action } => {
            let action = action.unwrap_or(McpAction::List);
            run_async(mcp(action))
        }
        Command::Skills { action } => {
            let action = action.unwrap_or_else(|| skills::SkillsAction::List(Default::default()));
            exit(skills::run(action, &config))
        }
        Command::Users { action } => users(action),
        Command::Profiles { action } => profiles(action),
        Command::Entities { action } => entities(action),
        Command::Hive { action } => run_async(hive(action)),
        Command::Learning { action } => learning(action),
        Command::Tickets { action } => tickets(action),
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

fn open_workspace(store: &Store) -> vikram_runtime::WorkspaceManager {
    vikram_runtime::WorkspaceManager::new(store.clone(), vikram_home())
}

fn sessions(action: SessionsAction) -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let store = open_store()?;
        let workspace = open_workspace(&store);
        let now = vikram_runtime::types::now_ms();
        match action {
            SessionsAction::List => {
                let rows = store.list_sessions().map_err(|e| e.to_string())?;
                if rows.is_empty() {
                    println!("no sessions");
                }
                for row in rows {
                    let project = row
                        .project_id
                        .as_ref()
                        .and_then(|p| store.get_project(p).ok().flatten())
                        .map(|p| p.path.display().to_string())
                        .unwrap_or_else(|| "-".to_owned());
                    println!(
                        "{}  {}  project={}  {}  updated {}",
                        row.id,
                        row.title,
                        project,
                        row.cwd.display(),
                        format_age(now, row.updated_ms)
                    );
                }
            }
            SessionsAction::New { title } => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let row = store
                    .create_session(&title, &cwd, None, now)
                    .map_err(|e| e.to_string())?;
                println!("{}", row.id);
            }
            SessionsAction::Show { id } => {
                let id = vikram_contracts::SessionId::new(id);
                let row = store
                    .get_session(&id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("session {id} not found"))?;
                let ws = workspace.session_workspace(&id);
                let project = row
                    .project_id
                    .as_ref()
                    .and_then(|p| store.get_project(p).ok().flatten());
                println!("id:        {}", row.id);
                println!("title:     {}", row.title);
                println!("status:    {}", row.status);
                println!("cwd:       {}", row.cwd.display());
                println!(
                    "project:   {}",
                    project
                        .as_ref()
                        .map(|p| p.path.display().to_string())
                        .unwrap_or_else(|| "(none)".to_owned())
                );
                println!(
                    "workspace: {}",
                    ws.map(|w| w.display().to_string())
                        .unwrap_or_else(|| "(not created yet)".to_owned())
                );
                println!("created:   {}", row.created_ms);
                println!("updated:   {}", row.updated_ms);
            }
            SessionsAction::Rename { id, title } => {
                let id = vikram_contracts::SessionId::new(id);
                store
                    .rename_session(&id, &title, now)
                    .map_err(|e| e.to_string())?;
                println!("renamed {id} to {title}");
            }
            SessionsAction::Archive { id } => {
                let id = vikram_contracts::SessionId::new(id);
                store
                    .set_session_status(&id, "archived", now)
                    .map_err(|e| e.to_string())?;
                println!("archived {id}");
            }
            SessionsAction::Duplicate { id } => {
                let id = vikram_contracts::SessionId::new(id);
                let copy = store
                    .duplicate_session(&id, now)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("session {id} not found"))?;
                println!("{}", copy.id);
            }
            SessionsAction::Rm { id } => {
                let id = vikram_contracts::SessionId::new(id);
                store.delete_session(&id).map_err(|e| e.to_string())?;
                // Remove VIKRAM's own managed folder for the session. Project files are never
                // touched: they live outside the sessions root and are only referenced by id.
                let managed = vikram_home().join("sessions").join(id.as_str());
                if managed.is_dir() {
                    let _ = std::fs::remove_dir_all(&managed);
                }
                println!("removed {id}");
            }
        }
        Ok(())
    })();

    exit(result)
}

fn projects(action: ProjectAction) -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let store = open_store()?;
        let workspace = open_workspace(&store);
        match action {
            ProjectAction::List => {
                let changed = workspace
                    .refresh_project_status()
                    .map_err(|e| e.to_string())?;
                let rows = workspace.list_projects().map_err(|e| e.to_string())?;
                if rows.is_empty() {
                    println!("no projects");
                }
                for row in rows {
                    println!(
                        "{}  {}  {}  {}",
                        row.id,
                        row.name,
                        row.path.display(),
                        row.status
                    );
                }
                if changed > 0 {
                    println!("({changed} project status(es) refreshed)");
                }
            }
            ProjectAction::Select { session, path } => {
                let id = vikram_contracts::SessionId::new(session);
                let project = workspace
                    .select_project(&id, &path)
                    .map_err(|e| e.to_string())?;
                println!(
                    "session {id} now uses project {} at {}",
                    project.id,
                    project.path.display()
                );
            }
            ProjectAction::Show { session } => {
                let id = vikram_contracts::SessionId::new(session);
                match workspace.active_project(&id).map_err(|e| e.to_string())? {
                    Some(p) => println!("{}  {}", p.id, p.path.display()),
                    None => println!("(no project; session workspace is active)"),
                }
            }
            ProjectAction::Disconnect { session } => {
                let id = vikram_contracts::SessionId::new(session);
                workspace
                    .disconnect_project(&id)
                    .map_err(|e| e.to_string())?;
                println!("session {id} disconnected from its project");
            }
        }
        Ok(())
    })();

    exit(result)
}

fn workspace_cmd(action: WorkspaceAction) -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let store = open_store()?;
        let workspace = open_workspace(&store);
        match action {
            WorkspaceAction::Show { session } => {
                let id = vikram_contracts::SessionId::new(session);
                let row = workspace
                    .store()
                    .get_session(&id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("session {id} not found"))?;
                let ws = workspace.session_workspace(&id);
                let mode = workspace
                    .store()
                    .get_session_workspace(&id)
                    .map_err(|e| e.to_string())?
                    .map(|w| w.mode)
                    .unwrap_or_else(|| "session".to_owned());
                println!("session:  {}", id);
                println!("mode:     {mode}");
                println!(
                    "workspace: {}",
                    ws.map(|w| w.display().to_string())
                        .unwrap_or_else(|| "(not created yet)".to_owned())
                );
                println!("cwd:      {}", row.cwd.display());
            }
            WorkspaceAction::Changes { session } => {
                let id = vikram_contracts::SessionId::new(session);
                let changes = workspace.file_changes(&id).map_err(|e| e.to_string())?;
                if changes.is_empty() {
                    println!("no tracked file changes");
                }
                for c in changes {
                    println!(
                        "{}  {}  {}  checkpoint={}",
                        c.path.display(),
                        c.operation,
                        c.ts_ms,
                        c.checkpoint_path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "-".to_owned())
                    );
                }
            }
            WorkspaceAction::Restore { session, path } => {
                let id = vikram_contracts::SessionId::new(session);
                workspace
                    .restore_file(&id, &path)
                    .map_err(|e| e.to_string())?;
                println!("restored {}", path.display());
            }
        }
        Ok(())
    })();

    exit(result)
}

fn files(action: FilesAction) -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let store = open_store()?;
        let workspace = open_workspace(&store);
        match action {
            FilesAction::List { session } => {
                let filter = session
                    .as_ref()
                    .map(|s| vikram_contracts::SessionId::new(s.clone()));
                let rows = workspace
                    .list_artifacts(filter.as_ref())
                    .map_err(|e| e.to_string())?;
                if rows.is_empty() {
                    println!("no artifacts");
                }
                for a in rows {
                    println!(
                        "{}  {}  {}  {}  {}",
                        a.id,
                        a.kind,
                        a.size,
                        a.path.display(),
                        a.session.as_str()
                    );
                }
            }
            FilesAction::Rm { id } => {
                let removed = workspace.remove_artifact(&id).map_err(|e| e.to_string())?;
                if removed {
                    println!("removed artifact record {id} (file untouched)");
                } else {
                    println!("no artifact {id}");
                }
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

fn users(action: UsersAction) -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let store = open_store()?;
        match action {
            UsersAction::List => {
                let rows = store.list_users().map_err(|e| e.to_string())?;
                if rows.is_empty() {
                    println!("no users");
                }
                for row in rows {
                    println!("{:<24} {}", row.id, row.name);
                }
            }
            UsersAction::New { name } => {
                let now = vikram_runtime::types::now_ms();
                let row = store.create_user(&name, now).map_err(|e| e.to_string())?;
                println!("{}", row.id);
            }
            UsersAction::Rm { id } => {
                let id = vikram_contracts::UserId::new(id);
                store.delete_user(&id).map_err(|e| e.to_string())?;
                println!("removed {id}");
            }
        }
        Ok(())
    })();
    exit(result)
}

fn profiles(action: ProfilesAction) -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let store = open_store()?;
        match action {
            ProfilesAction::List { user } => {
                let rows = match user {
                    Some(u) => {
                        let uid = vikram_contracts::UserId::new(u);
                        store
                            .list_profiles_by_user(&uid)
                            .map_err(|e| e.to_string())?
                    }
                    None => store.list_profiles().map_err(|e| e.to_string())?,
                };
                if rows.is_empty() {
                    println!("no profiles");
                }
                for row in rows {
                    println!("{:<24} {:<24} {}", row.id, row.user, row.name);
                }
            }
            ProfilesAction::New { user, name } => {
                let uid = vikram_contracts::UserId::new(user);
                let now = vikram_runtime::types::now_ms();
                let row = store
                    .create_profile(&uid, &name, now)
                    .map_err(|e| e.to_string())?;
                println!("{}", row.id);
            }
            ProfilesAction::Rm { id } => {
                let id = vikram_contracts::ProfileId::new(id);
                store.delete_profile(&id).map_err(|e| e.to_string())?;
                println!("removed {id}");
            }
        }
        Ok(())
    })();
    exit(result)
}

fn entities(action: EntitiesAction) -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let store = open_store()?;
        match action {
            EntitiesAction::List { kind } => {
                let rows = match kind {
                    Some(k) => {
                        let ek = vikram_store::EntityKind::parse(&k)
                            .ok_or_else(|| format!("unknown kind {k}"))?;
                        store.list_entities_by_kind(ek).map_err(|e| e.to_string())?
                    }
                    None => store.list_entities().map_err(|e| e.to_string())?,
                };
                if rows.is_empty() {
                    println!("no entities");
                }
                for row in rows {
                    println!(
                        "{:<24} {:<12} {:<24} {}",
                        row.id, row.kind, row.slug, row.name
                    );
                }
            }
            EntitiesAction::New {
                kind,
                name,
                slug,
                title,
                role,
                specialization,
                description,
            } => {
                use vikram_store::EntityKind;
                let ek = EntityKind::parse(&kind).ok_or_else(|| format!("unknown kind {kind}"))?;
                let now = vikram_runtime::types::now_ms();
                let row = vikram_store::EntityRow {
                    id: vikram_contracts::EntityId::generate(),
                    owner_user: None,
                    kind: ek,
                    name: name.clone(),
                    slug: slug.clone(),
                    title: title.clone(),
                    role: if role.is_empty() {
                        ek.as_str().to_owned()
                    } else {
                        role
                    },
                    specialization: specialization.clone(),
                    description: if description.is_empty() {
                        if title.is_empty() {
                            format!("{name} ({})", ek.as_str())
                        } else {
                            format!("{name} — {title}")
                        }
                    } else {
                        description.clone()
                    },
                    icon: None,
                    model: None,
                    profile_id: None,
                    status: "active".to_owned(),
                    created_ms: now,
                    updated_ms: now,
                };
                store.save_entity(&row).map_err(|e| e.to_string())?;
                // Scaffold context files.
                let base = vikram_runtime::config::vikram_home();
                let dir = base.join(ek.folder()).join(&slug);
                let loader = vikram_runtime::ContextLoader::new(
                    base.clone(),
                    std::env::current_dir().unwrap_or_default(),
                );
                loader.scaffold_entity(&dir, &row.name, ek.as_str(), &row.title);
                println!("{}", row.id);
            }
            EntitiesAction::Rm { id } => {
                let id = vikram_contracts::EntityId::new(id);
                store.delete_entity(&id).map_err(|e| e.to_string())?;
                println!("removed {id}");
            }
        }
        Ok(())
    })();
    exit(result)
}

async fn hive(action: HiveAction) -> Result<(), String> {
    let store = open_store()?;
    match action {
        HiveAction::List => {
            let rows = store.list_hives().map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("no hive sessions");
            }
            for row in rows {
                println!(
                    "{:<24} {:<24} {}  [{}]",
                    row.id, row.name, row.objective, row.state
                );
            }
        }
        HiveAction::New {
            name,
            objective,
            policy,
        } => {
            let now = vikram_runtime::types::now_ms();
            // Default owner: first user, or create an anonymous one.
            let owner = store
                .list_users()
                .map_err(|e| e.to_string())?
                .into_iter()
                .next()
                .map(|u| u.id)
                .unwrap_or_else(|| {
                    store
                        .create_user("default", now)
                        .map(|u| u.id)
                        .unwrap_or_else(|_| vikram_contracts::UserId::generate())
                });
            let row = vikram_store::HiveRow {
                id: vikram_contracts::HiveId::generate(),
                owner_user: owner,
                profile_id: None,
                name,
                objective,
                session_id: None,
                state: "active".to_owned(),
                policy,
                created_ms: now,
                updated_ms: now,
            };
            let saved = store.create_hive(&row).map_err(|e| e.to_string())?;
            println!("{}", saved.id);
        }
        HiveAction::Show { id } => {
            let hid = vikram_contracts::HiveId::new(id);
            let hive = store
                .get_hive(&hid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "hive not found".to_owned())?;
            println!("Hive: {} ({})", hive.name, hive.id);
            println!("Objective: {}", hive.objective);
            println!("State: {}", hive.state);
            println!("Participants:");
            for p in store
                .list_hive_participants(&hid)
                .map_err(|e| e.to_string())?
            {
                println!("  - {} ({})", p.entity, p.kind);
            }
            println!("Messages:");
            for m in store.list_hive_messages(&hid).map_err(|e| e.to_string())? {
                println!("  [{}] {}: {}", m.ts_ms, m.sender, m.content);
            }
        }
        HiveAction::AddParticipant { hive, entity } => {
            let hid = vikram_contracts::HiveId::new(hive);
            let eid = vikram_contracts::EntityId::new(entity);
            let ent = store
                .get_entity(&eid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "entity not found".to_owned())?;
            let now = vikram_runtime::types::now_ms();
            let p = vikram_store::HiveParticipantRow {
                hive: hid,
                entity: eid,
                kind: ent.kind,
                role: "member".to_owned(),
                joined_ms: now,
            };
            store.add_hive_participant(&p).map_err(|e| e.to_string())?;
            println!("added");
        }
        HiveAction::RemoveParticipant { hive, entity } => {
            let hid = vikram_contracts::HiveId::new(hive);
            let eid = vikram_contracts::EntityId::new(entity);
            store
                .remove_hive_participant(&hid, &eid)
                .map_err(|e| e.to_string())?;
            println!("removed");
        }
        HiveAction::Send {
            hive,
            sender,
            content,
        } => {
            let hid = vikram_contracts::HiveId::new(hive);
            let now = vikram_runtime::types::now_ms();
            let msg = vikram_store::HiveMessageRow {
                id: vikram_contracts::HiveMessageId::generate(),
                hive: hid,
                sender: sender.clone(),
                sender_kind: "user".to_owned(),
                content,
                task_ref: None,
                ts_ms: now,
            };
            store.append_hive_message(&msg).map_err(|e| e.to_string())?;
            println!("sent");
        }
        HiveAction::Run {
            hive,
            message,
            parallel,
        } => {
            // Run requires a live runtime/provider; use the runtime handle.
            let rt = vikram_runtime::Runtime::start(
                vikram_runtime::Config::load().map_err(|e| e.to_string())?,
            )
            .await
            .map_err(|e| e.to_string())?;
            let handle = rt.handle();
            let hid = vikram_contracts::HiveId::new(hive);
            let transcript = handle
                .hive_run(&hid, &message, parallel)
                .await
                .map_err(|e| e.to_string())?;
            for m in transcript {
                println!("[{}] {}: {}", m.ts_ms, m.sender, m.content);
            }
        }
        HiveAction::Rm { id } => {
            let hid = vikram_contracts::HiveId::new(id);
            store.delete_hive(&hid).map_err(|e| e.to_string())?;
            println!("removed {hid}");
        }
    }
    Ok(())
}

fn tickets(action: TicketsAction) -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let store = open_store()?;
        let now = vikram_runtime::types::now_ms();
        match action {
            TicketsAction::List { kind, status } => {
                let mut rows = store.list_tickets().map_err(|e| e.to_string())?;
                if let Some(k) = kind {
                    rows.retain(|t| t.kind == k);
                }
                if let Some(s) = status {
                    rows.retain(|t| t.status == s);
                }
                if rows.is_empty() {
                    println!("no tickets");
                }
                for t in rows {
                    println!("{:<24} {:<10} {:<12} {}", t.id, t.kind, t.status, t.title);
                }
            }
            TicketsAction::Show { id } => {
                let tid = vikram_contracts::TicketId::new(id);
                let t = store
                    .get_ticket(&tid)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("ticket {tid} not found"))?;
                println!("id:          {}", t.id);
                println!("kind:        {}", t.kind);
                println!("status:      {}", t.status);
                println!("owner:       {}", t.owner_key);
                println!("title:       {}", t.title);
                println!("request:     {}", t.request);
                if let Some(r) = &t.result {
                    println!("result:      {}", r);
                }
                if let Some(e) = &t.failure_reason {
                    println!("failure:     {}", e);
                }
                println!("participants:");
                for p in store
                    .list_ticket_participants(&tid)
                    .map_err(|e| e.to_string())?
                {
                    println!(
                        "  - {} ({}) read={} write={} admin={}",
                        p.participant_key, p.participant_kind, p.can_read, p.can_write, p.can_admin
                    );
                }
                println!("messages:");
                for m in store
                    .list_ticket_messages(&tid)
                    .map_err(|e| e.to_string())?
                {
                    println!(
                        "  [{}] {} ({}): {}",
                        m.ts_ms, m.author_key, m.visibility, m.content
                    );
                }
                println!("assignments:");
                for a in store.list_assignments(&tid).map_err(|e| e.to_string())? {
                    println!(
                        "  - {} [{}] {} -> {}",
                        a.id, a.status, a.title, a.assignee_key
                    );
                }
            }
            TicketsAction::New {
                kind,
                title,
                request,
                owner,
            } => {
                let row = vikram_store::TicketRow {
                    id: vikram_contracts::TicketId::generate(),
                    owner_key: owner.clone(),
                    owner_kind: "user".into(),
                    title: title.clone(),
                    description: "".into(),
                    request: request.clone(),
                    kind: kind.clone(),
                    status: "open".into(),
                    parent_ticket: None,
                    hive_id: None,
                    session_id: None,
                    strategy: None,
                    result: None,
                    failure_reason: None,
                    created_ms: now,
                    updated_ms: now,
                    closed_ms: None,
                };
                let saved = store.create_ticket(&row).map_err(|e| e.to_string())?;
                store
                    .add_ticket_participant(&vikram_store::TicketParticipantRow {
                        ticket: saved.id.clone(),
                        participant_key: owner,
                        participant_kind: "user".into(),
                        entity_id: None,
                        user_id: None,
                        role: "owner".into(),
                        can_read: true,
                        can_write: true,
                        can_admin: true,
                        joined_ms: now,
                    })
                    .map_err(|e| e.to_string())?;
                println!("{}", saved.id);
            }
            TicketsAction::AddParticipant {
                ticket,
                participant,
                kind,
            } => {
                let tid = vikram_contracts::TicketId::new(ticket);
                store
                    .add_ticket_participant(&vikram_store::TicketParticipantRow {
                        ticket: tid,
                        participant_key: participant,
                        participant_kind: kind,
                        entity_id: None,
                        user_id: None,
                        role: "member".into(),
                        can_read: true,
                        can_write: true,
                        can_admin: false,
                        joined_ms: now,
                    })
                    .map_err(|e| e.to_string())?;
                println!("added");
            }
            TicketsAction::Send {
                ticket,
                author,
                kind,
                content,
                visibility,
            } => {
                let tid = vikram_contracts::TicketId::new(ticket);
                let msg = vikram_store::TicketMessageRow {
                    id: vikram_contracts::TicketMessageId::generate(),
                    ticket: tid,
                    author_key: author,
                    author_kind: kind,
                    content,
                    visibility,
                    ts_ms: now,
                    task_ref: None,
                };
                store
                    .append_ticket_message(&msg)
                    .map_err(|e| e.to_string())?;
                println!("sent");
            }
            TicketsAction::Assign {
                ticket,
                assignee,
                kind,
                title,
            } => {
                let tid = vikram_contracts::TicketId::new(ticket);
                let a = vikram_store::AssignmentRow {
                    id: vikram_contracts::AssignmentId::generate(),
                    ticket: tid,
                    parent_assignment: None,
                    assignee_key: assignee.clone(),
                    assignee_kind: kind,
                    entity_id: None,
                    title,
                    description: "".into(),
                    status: "assigned".into(),
                    strategy: None,
                    result: None,
                    error: None,
                    created_ms: now,
                    updated_ms: now,
                    completed_ms: None,
                };
                let saved = store.create_assignment(&a).map_err(|e| e.to_string())?;
                println!("{}", saved.id);
            }
            TicketsAction::Complete { assignment, result } => {
                let aid = vikram_contracts::AssignmentId::new(assignment);
                store
                    .set_assignment_result(&aid, &result, now)
                    .map_err(|e| e.to_string())?;
                println!("completed");
            }
            TicketsAction::Close { id } => {
                let tid = vikram_contracts::TicketId::new(id);
                store
                    .set_ticket_status(&tid, "completed", now)
                    .map_err(|e| e.to_string())?;
                println!("closed");
            }
            TicketsAction::Reopen { id } => {
                let tid = vikram_contracts::TicketId::new(id);
                store.reopen_ticket(&tid, now).map_err(|e| e.to_string())?;
                println!("reopened");
            }
            TicketsAction::Rm { id } => {
                let tid = vikram_contracts::TicketId::new(id);
                store.delete_ticket(&tid).map_err(|e| e.to_string())?;
                println!("removed");
            }
        }
        Ok(())
    })();
    exit(result)
}

fn learning(action: LearningAction) -> std::process::ExitCode {
    let result = (|| -> Result<(), String> {
        let path = vikram_runtime::config::vikram_home().join("learning.json");
        let mgr = vikram_runtime::LearningManager::open(Some(path.clone()));
        match action {
            LearningAction::Status => {
                println!("memory policy: {}", mgr.policy_memory().as_str());
                println!("skill policy: {}", mgr.policy_skill().as_str());
                println!("pending proposals: {}", mgr.pending_proposals().len());
                println!("audit entries: {}", mgr.audit_log().len());
            }
            LearningAction::Pending => {
                let props = mgr.pending_proposals();
                if props.is_empty() {
                    println!("no pending proposals");
                }
                for p in props {
                    println!("{}  {}  {}  {}", p.id, p.kind, p.target, p.action);
                    println!("  reason: {}", p.reason);
                    println!("  content: {}", p.content);
                }
            }
            LearningAction::Show { id } => match mgr.find_proposal(&id) {
                Some(p) => println!("{}", serde_json::to_string_pretty(&p).unwrap_or_default()),
                None => println!("not found"),
            },
            LearningAction::Approve { id } => {
                if let Some(p) = mgr.find_proposal(&id) {
                    if p.kind == "memory" {
                        let scope = if p.target == "user" { "user" } else { "memory" };
                        let mem = vikram_runtime::MemoryStore::open(
                            vikram_runtime::config::database_path(),
                        )
                        .map_err(|e| e.to_string())?;
                        mem.save(&p.key, &p.content, scope)
                            .map_err(|e| e.to_string())?;
                    } else if p.kind == "skill" {
                        let m = vikram_skills::Manager::new();
                        let name = if p.target.is_empty() {
                            "learned-skill"
                        } else {
                            &p.target
                        };
                        let _ = m.create(name, &p.reason, &p.content);
                    }
                    mgr.set_proposal_status(&id, "approved");
                    mgr.audit(vikram_runtime::LearningAuditEntry {
                        ts_ms: vikram_runtime::types::now_ms(),
                        session: p.session.clone(),
                        kind: p.kind.clone(),
                        action: p.action.clone(),
                        target: p.target.clone(),
                        outcome: "applied".to_owned(),
                        detail: "approved via cli".to_owned(),
                    });
                    mgr.save();
                    println!("approved and applied {id}");
                } else {
                    return Err("proposal not found".to_owned());
                }
            }
            LearningAction::Reject { id } => {
                mgr.set_proposal_status(&id, "rejected");
                mgr.save();
                println!("rejected {id}");
            }
            LearningAction::History => {
                for e in mgr.audit_log() {
                    println!(
                        "{}  {}  {}  {}  {}",
                        e.ts_ms, e.kind, e.action, e.target, e.outcome
                    );
                }
            }
            LearningAction::SetPolicy { kind, policy } => {
                let pol = vikram_runtime::LearningPolicy::parse(&policy);
                match kind.as_str() {
                    "memory" => mgr.set_policy_memory(pol),
                    "skill" => mgr.set_policy_skill(pol),
                    _ => return Err(format!("unknown kind {kind}")),
                }
                mgr.save();
                println!("set {kind} policy to {}", pol.as_str());
            }
        }
        Ok(())
    })();
    exit(result)
}

async fn mcp(action: McpAction) -> Result<(), String> {
    match action {
        McpAction::List => {}
        McpAction::Status => {
            println!("MCP Server: not running (use `vikram mcp serve` to start)");
            return Ok(());
        }
        McpAction::Serve => {
            return mcp_serve().await;
        }
    }
    let config = vikram_runtime::config::McpConfig::load();
    if config.servers.is_empty() {
        println!(
            "no MCP servers configured\nconfig: {}",
            vikram_runtime::config::mcp_config_path().display()
        );
        return Ok(());
    }
    println!("{:<20} COMMAND", "NAME");
    for server in config.servers {
        println!("{:<20} {}", server.name, server.command);
    }
    Ok(())
}

struct ServeExecutor {
    registry: Arc<vikram_runtime::tools::ToolRegistry>,
    workspace_root: PathBuf,
    sandbox: vikram_runtime::config::SandboxSettings,
}

impl vikram_mcp::server::ToolExecutor for ServeExecutor {
    fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| format!("tool not found: {name}"))?;
        let _ = tool.permission(&arguments).map_err(|e| e.to_string())?;
        let ctx =
            vikram_runtime::tools::ToolContext::new(self.workspace_root.clone(), &self.sandbox);
        let output = tool.run(&arguments, &ctx).map_err(|e| e.to_string())?;
        let mut result = serde_json::json!({ "text": output.content });
        if !output.ok {
            result["isError"] = serde_json::json!(true);
        }
        Ok(result)
    }
}

async fn mcp_serve() -> Result<(), String> {
    let serve_config = vikram_runtime::config::McpConfig::load();
    if serve_config.approved_tools.is_empty() {
        return Err(format!(
            "no approved tools configured; refusing to expose anything.\nadd `approvedTools` to {}",
            vikram_runtime::config::mcp_config_path().display()
        ));
    }

    let mut server_cfg = vikram_mcp::ServerConfig::default();
    server_cfg.approved_tools = serve_config.approved_tools.clone();
    server_cfg.auth_required = serve_config.auth_required;
    server_cfg.auth_token = serve_config.auth_token.clone();

    let app_config = Config::load().unwrap_or_default();
    let workspace_root = app_config
        .workspace_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let sandbox_settings = app_config.security.sandbox.clone();

    let registry = Arc::new(vikram_runtime::tools::ToolRegistry::with_builtins());
    for name in &serve_config.approved_tools {
        if registry.get(name).is_none() {
            return Err(format!("approved tool `{name}` is not a known tool"));
        }
    }

    let mut server = vikram_mcp::McpServer::new(server_cfg);
    for name in &serve_config.approved_tools {
        let spec = registry
            .get(name)
            .map(|t| t.spec())
            .expect("approved tools verified above");
        server.register_tool(vikram_mcp::server::ExposedTool {
            name: spec.name,
            description: spec.description,
            input_schema: spec.schema,
        });
    }

    server.set_tool_executor(Arc::new(ServeExecutor {
        registry,
        workspace_root,
        sandbox: sandbox_settings,
    }));

    eprintln!(
        "vikram: mcp serve running on stdio with approved tools: {}",
        serve_config.approved_tools.join(", ")
    );
    server.run_stdio().await
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
