//! Shared vocabulary for the unified Ticket System.
//!
//! A Ticket is one persistent unit of work. Hive teams, independent bots and
//! sub-agents all use the same Ticket engine; only their collaboration and
//! visibility rules differ, expressed through these enums rather than through
//! separate engines.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    #[default]
    Open,
    InProgress,
    Blocked,
    Completed,
    Failed,
    Cancelled,
    Reopened,
}

impl TicketStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Reopened => "reopened",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "in_progress" => Some(Self::InProgress),
            "blocked" => Some(Self::Blocked),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "reopened" => Some(Self::Reopened),
            _ => None,
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

impl std::fmt::Display for TicketStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TicketKind {
    #[default]
    General,
    Hive,
    Bot,
    SubAgent,
}

impl TicketKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Hive => "hive",
            Self::Bot => "bot",
            Self::SubAgent => "subagent",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "general" => Some(Self::General),
            "hive" => Some(Self::Hive),
            "bot" => Some(Self::Bot),
            "subagent" | "sub_agent" | "sub-agent" => Some(Self::SubAgent),
            _ => None,
        }
    }
}

impl std::fmt::Display for TicketKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    #[default]
    User,
    Agent,
    Bot,
    HiveMember,
    SubAgent,
}

impl ParticipantKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Bot => "bot",
            Self::HiveMember => "hive_member",
            Self::SubAgent => "subagent",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "agent" => Some(Self::Agent),
            "bot" => Some(Self::Bot),
            "hive_member" => Some(Self::HiveMember),
            "subagent" | "sub_agent" | "sub-agent" => Some(Self::SubAgent),
            _ => None,
        }
    }
}

impl std::fmt::Display for ParticipantKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TicketVisibility {
    #[default]
    Shared,
    Private,
    Internal,
}

impl TicketVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Private => "private",
            Self::Internal => "internal",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "shared" => Some(Self::Shared),
            "private" => Some(Self::Private),
            "internal" => Some(Self::Internal),
            _ => None,
        }
    }
}

impl std::fmt::Display for TicketVisibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentStatus {
    #[default]
    Pending,
    Assigned,
    InProgress,
    Completed,
    Failed,
    Cancelled,
    Blocked,
}

impl AssignmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Assigned => "assigned",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "assigned" => Some(Self::Assigned),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

impl std::fmt::Display for AssignmentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationStrategy {
    #[default]
    Single,
    Parallel,
    Sequential,
    Specialized,
}

impl CollaborationStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Parallel => "parallel",
            Self::Sequential => "sequential",
            Self::Specialized => "specialized",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "single" => Some(Self::Single),
            "parallel" => Some(Self::Parallel),
            "sequential" => Some(Self::Sequential),
            "specialized" => Some(Self::Specialized),
            _ => None,
        }
    }
}

impl std::fmt::Display for CollaborationStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
