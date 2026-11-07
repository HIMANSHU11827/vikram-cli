use thiserror::Error;

#[derive(Debug, Error)]
pub enum EvolutionError {
    #[error("store error: {0}")]
    Store(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("validation error: {0}")]
    Validation(String),
    #[error("security error: {0}")]
    Security(String),
    // Retained for the public API: no caller currently produces these, but
    // removing a variant is a breaking change for downstream matches.
    #[error("budget exceeded: {0}")]
    Budget(String),
    #[error("conflict: {0}")]
    Conflict(String),
}

pub type Result<T> = std::result::Result<T, EvolutionError>;
