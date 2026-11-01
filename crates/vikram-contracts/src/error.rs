use crate::ids::{ApprovalId, ToolCallId};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, VikramError>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VikramError {
    #[error(
        "contract `{id}` incompatible: component requires {required}.x, host provides {host}.x"
    )]
    ContractIncompatible {
        id: String,
        required: u32,
        host: u32,
    },

    #[error("permission denied by policy: {reason}")]
    PolicyDenied { reason: String },

    #[error("approval required before this effect may run ({approval} for {subject})")]
    ApprovalRequired {
        approval: ApprovalId,
        subject: String,
    },

    #[error("operation cancelled")]
    Cancelled,

    #[error("provider is transiently unavailable: {0}")]
    ProviderTransient(String),

    #[error("provider rejected the request: {0}")]
    ProviderRejected(String),

    #[error("storage failure: {0}")]
    Storage(String),

    #[error("effect `{0}` failed to apply")]
    EffectFailed(String),

    #[error("invalid runtime state: {0}")]
    InvalidState(String),

    #[error("tool `{0}` is not registered")]
    UnknownTool(String),

    #[error("step budget exhausted after {0} steps")]
    BudgetExhausted(u32),
}

impl VikramError {
    /// Transient failures may be retried without changing the journal; terminal ones may not.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::ProviderTransient(_) | Self::Storage(_))
    }

    pub fn is_permission_failure(&self) -> bool {
        matches!(
            self,
            Self::PolicyDenied { .. } | Self::ApprovalRequired { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    Completed { call: ToolCallId, content: String },
    Failed { call: ToolCallId, error: String },
    Denied { call: ToolCallId, reason: String },
    Cancelled { call: ToolCallId },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_and_terminal_errors_are_classified_differently() {
        assert!(VikramError::ProviderTransient("429".into()).is_retryable());
        assert!(!VikramError::Cancelled.is_retryable());
        assert!(!VikramError::PolicyDenied { reason: "x".into() }.is_retryable());
        assert!(VikramError::PolicyDenied { reason: "x".into() }.is_permission_failure());
    }

    #[test]
    fn errors_render_the_context_an_operator_needs() {
        let err = VikramError::ContractIncompatible {
            id: "vikram.memory.store".into(),
            required: 3,
            host: 2,
        };
        assert_eq!(
            err.to_string(),
            "contract `vikram.memory.store` incompatible: component requires 3.x, host provides 2.x"
        );
    }
}
