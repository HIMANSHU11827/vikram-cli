use std::path::PathBuf;
use vikram_contracts::{HiveRosterId, SessionId, VikramError};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("configuration at `{path}` is invalid: {reason}")]
    Config { path: PathBuf, reason: String },

    #[error("storage failure: {0}")]
    Store(String),

    #[error("i/o failure: {0}")]
    Io(String),

    #[error("model provider failed: {0}")]
    Provider(String),

    #[error("no model configured: set `model` and the API key named by `api_key_env`")]
    ModelUnavailable,

    #[error("session `{0}` does not exist")]
    SessionNotFound(SessionId),

    #[error("unknown agent `{0}`")]
    AgentNotFound(String),

    #[error("unknown team `{0}`")]
    TeamNotFound(HiveRosterId),

    #[error("permission denied: {0}")]
    Denied(String),

    #[error("contract violation: {0}")]
    Contract(#[from] VikramError),

    #[error("runtime is shutting down")]
    ShuttingDown,

    #[error("invalid runtime state: {0}")]
    InvalidState(String),

    #[error("MCP server failure: {0}")]
    Mcp(String),
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

impl From<std::io::Error> for RuntimeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<vikram_store::StoreError> for RuntimeError {
    fn from(e: vikram_store::StoreError) -> Self {
        Self::Store(e.to_string())
    }
}
