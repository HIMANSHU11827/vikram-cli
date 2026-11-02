//! The one table of slash commands.
//!
//! Every surface that lists commands reads this table: the `/` popup, the command palette, `/help`,
//! and the dispatcher that decides what Enter does. There used to be three lists that disagreed,
//! which is how `/model` could be offered by the type-ahead and answered with "unknown command".
//!
//! A command says *what kind of thing it is*, not how to print it:
//!
//! - [`Outcome::Picker`] — the command cannot be answered in one word, so it opens the chooser for
//!   that kind and the chosen row is the instruction.
//! - [`Outcome::Info`] — the answer is a panel of what the runtime already knows. It is read-only
//!   because there is nothing to choose: inventing a list of options here would be a lie.
//! - [`Outcome::Action`] — it does exactly one thing and closes.
//!
//! Nothing in this file performs an action or reads the runtime; the host does that. That keeps the
//! table assertable in a test without a terminal.

use std::path::{Path, PathBuf};

use crate::widgets::PickerKind;

/// What pressing Enter on a command does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Picker(PickerKind),
    Info,
    Action,
}

/// One command. `aliases` are matched exactly and never shown, so a command that was renamed keeps
/// working without appearing twice in the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub outcome: Outcome,
}

/// Presentation order, most used first — not alphabetical. The palette shows this order, so the
/// order is the ranking.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "model",
        aliases: &[],
        description: "Choose the model this session runs on",
        outcome: Outcome::Picker(PickerKind::Model),
    },
    Command {
        name: "permissions",
        aliases: &[],
        description: "Choose the approval posture",
        outcome: Outcome::Picker(PickerKind::Mode),
    },
    Command {
        name: "provider",
        aliases: &[],
        description: "The endpoint this process was started with",
        outcome: Outcome::Picker(PickerKind::Provider),
    },
    Command {
        name: "policy",
        aliases: &[],
        description: "The policy in force and the saved allow-list",
        outcome: Outcome::Picker(PickerKind::Permissions),
    },
    Command {
        name: "allow",
        aliases: &[],
        description: "Saved allow-list rules; choosing one forgets it",
        outcome: Outcome::Picker(PickerKind::Allow),
    },
    Command {
        name: "sessions",
        aliases: &["resume", "switch"],
        description: "Switch to another conversation",
        outcome: Outcome::Picker(PickerKind::Session),
    },
    Command {
        name: "rename",
        aliases: &[],
        description: "Rename this conversation",
        outcome: Outcome::Picker(PickerKind::RenameSession),
    },
    Command {
        name: "close",
        aliases: &[],
        description: "Close this conversation",
        outcome: Outcome::Picker(PickerKind::CloseSession),
    },
    Command {
        name: "tasks",
        aliases: &["background"],
        description: "Background tasks; choosing one stops it",
        outcome: Outcome::Picker(PickerKind::Task),
    },
    Command {
        name: "approvals",
        aliases: &[],
        description: "What is waiting for your answer",
        outcome: Outcome::Picker(PickerKind::Approvals),
    },
    Command {
        name: "memory",
        aliases: &[],
        description: "What the model has remembered",
        outcome: Outcome::Picker(PickerKind::Memory),
    },
    Command {
        name: "bot",
        aliases: &["bots"],
        description: "Bots in this session",
        outcome: Outcome::Picker(PickerKind::Bot),
    },
    Command {
        name: "agents",
        aliases: &[],
        description: "Pause, resume or stop an agent",
        outcome: Outcome::Picker(PickerKind::Agent),
    },
    Command {
        name: "team",
        aliases: &["teams", "hive"],
        description: "HIVE teams and their members",
        outcome: Outcome::Picker(PickerKind::Team),
    },
    Command {
        name: "skills",
        aliases: &[],
        description: "Skills discovered on this machine",
        outcome: Outcome::Picker(PickerKind::Skill),
    },
    Command {
        name: "tools",
        aliases: &[],
        description: "Tools this runtime can call",
        outcome: Outcome::Picker(PickerKind::Tools),
    },
    Command {
        name: "plugins",
        aliases: &[],
        description: "Plugins installed and loaded",
        outcome: Outcome::Picker(PickerKind::Plugins),
    },
    Command {
        name: "mcp",
        aliases: &[],
        description: "MCP servers; choosing one flips its visibility",
        outcome: Outcome::Picker(PickerKind::Mcp),
    },
    Command {
        name: "files",
        aliases: &[],
        description: "Reference a workspace file",
        outcome: Outcome::Picker(PickerKind::File),
    },
    Command {
        name: "status",
        aliases: &[],
        description: "Runtime and session status",
        outcome: Outcome::Picker(PickerKind::Status),
    },
    Command {
        name: "usage",
        aliases: &[],
        description: "Token usage for this session",
        outcome: Outcome::Picker(PickerKind::Usage),
    },
    Command {
        name: "context",
        aliases: &[],
        description: "How much of the context window is in use",
        outcome: Outcome::Picker(PickerKind::Context),
    },
    Command {
        name: "settings",
        aliases: &["prefs", "preferences"],
        description: "Interface preferences that apply now",
        outcome: Outcome::Picker(PickerKind::Setting),
    },
    Command {
        name: "keys",
        aliases: &["keymap", "shortcuts"],
        description: "Every key this interface listens for",
        outcome: Outcome::Picker(PickerKind::Keys),
    },
    Command {
        name: "doctor",
        aliases: &[],
        description: "Check local prerequisites",
        outcome: Outcome::Picker(PickerKind::Doctor),
    },
    Command {
        name: "diff",
        aliases: &[],
        description: "Run git diff in the workspace",
        outcome: Outcome::Picker(PickerKind::Diff),
    },
    Command {
        name: "test",
        aliases: &[],
        description: "Run the test suite",
        outcome: Outcome::Picker(PickerKind::Test),
    },
    Command {
        name: "build",
        aliases: &[],
        description: "Build the workspace",
        outcome: Outcome::Picker(PickerKind::Build),
    },
    Command {
        name: "clear",
        aliases: &[],
        description: "Clear this conversation",
        outcome: Outcome::Picker(PickerKind::Clear),
    },
    Command {
        name: "new",
        aliases: &[],
        description: "Start a new session",
        outcome: Outcome::Picker(PickerKind::NewSession),
    },
    Command {
        name: "version",
        aliases: &[],
        description: "Which build this is",
        outcome: Outcome::Picker(PickerKind::Version),
    },
    Command {
        name: "help",
        aliases: &["?"],
        description: "Browse every command",
        outcome: Outcome::Picker(PickerKind::Command),
    },
    Command {
        name: "exit",
        aliases: &["quit"],
        description: "Quit VIKRAM",
        outcome: Outcome::Picker(PickerKind::Exit),
    },
];

/// Look a command up by the word typed after `/`. Aliases resolve to the command they name.
pub fn find(word: &str) -> Option<&'static Command> {
    let word = word.trim().trim_start_matches('/').to_lowercase();
    COMMANDS
        .iter()
        .find(|command| command.name == word || command.aliases.contains(&word.as_str()))
}

/// The command the operator most likely meant when the word they typed is not one. A singular for a
/// plural it knows, or a prefix of one: `/session` finds `/sessions`, `/ski` finds `/skills`.
pub fn nearest(word: &str) -> Option<&'static Command> {
    let word = word.trim().trim_start_matches('/').to_lowercase();
    if word.is_empty() {
        return None;
    }
    COMMANDS.iter().find(|command| {
        command.name.starts_with(&word) || word.starts_with(command.name)
    })
}

/// The commands whose name starts with `prefix`, in table order. Used by the inline `/` popup.
pub fn starting_with(prefix: &str) -> Vec<&'static Command> {
    let prefix = prefix.trim().trim_start_matches('/').to_lowercase();
    COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(&prefix))
        .collect()
}

/// Shell commands offered after `!`. They are only suggestions: the `!` panel also runs whatever is
/// typed, because the operator's own command line is not limited to this list.
pub const SHELL_COMMANDS: &[&str] = &[
    "git status",
    "git diff",
    "git diff --stat",
    "git log --oneline -20",
    "git branch --show-current",
    "cargo build",
    "cargo test",
    "cargo clippy --all-targets",
    "cargo fmt --check",
    "ls",
];

/// Files under `root`, relative and `/`-separated, skipping the directories that are never worth a
/// mention. `.git` and `target` are the two that make a naive walk useless in a Rust project.
pub fn workspace_files(root: &Path, cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if out.len() >= cap || depth > 3 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(
                name.as_str(),
                ".git" | "target" | "node_modules" | "dist" | "build" | ".venv" | "__pycache__"
            ) {
                continue;
            }
            if path.is_dir() {
                stack.push((path, depth + 1));
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
                if out.len() >= cap {
                    return out;
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_command_resolves_to_itself_and_its_aliases() {
        for command in COMMANDS {
            assert_eq!(find(command.name).map(|c| c.name), Some(command.name));
            for alias in command.aliases {
                assert_eq!(
                    find(alias).map(|c| c.name),
                    Some(command.name),
                    "`/{alias}` has to reach /{}",
                    command.name
                );
            }
        }
    }

    #[test]
    fn no_command_is_listed_twice_and_no_alias_shadows_a_name() {
        let mut names = std::collections::HashSet::new();
        for command in COMMANDS {
            assert!(names.insert(command.name), "/{} is listed twice", command.name);
        }
        for command in COMMANDS {
            for alias in command.aliases {
                assert!(
                    !names.contains(*alias),
                    "alias `{alias}` shadows a command with that name"
                );
            }
        }
    }

    #[test]
    fn a_command_that_was_never_declared_is_not_offered_anywhere() {
        assert!(find("nonsense").is_none());
        assert!(starting_with("nonsense").is_empty());
    }

    #[test]
    fn the_popup_prefix_matches_names_and_never_aliases() {
        let mo: Vec<&str> = starting_with("/mo").into_iter().map(|c| c.name).collect();
        assert_eq!(mo, vec!["model"], "the renamed command no longer widens the popup");
        assert!(
            starting_with("bots").is_empty(),
            "an alias is matched exactly, so it cannot also widen the popup"
        );
        assert!(
            find("/mode").is_none(),
            "the old spelling is gone, not aliased"
        );
    }

    #[test]
    fn every_command_says_what_kind_of_thing_it_is_and_no_description_is_empty() {
        for command in COMMANDS {
            assert!(!command.description.trim().is_empty(), "{}", command.name);
            assert!(
                !command.description.starts_with("Show"),
                "`{}` describes an outcome, not a verb: {}",
                command.name,
                command.description
            );
        }
        let pickers = COMMANDS
            .iter()
            .filter(|c| matches!(c.outcome, Outcome::Picker(_)))
            .count();
        assert!(pickers >= 6, "the choosers are the point of the table");
    }
}
