//! Stable typed contracts for VIKRAM.
//!
//! Anything that crosses a replacement boundary lives here: identifiers, the error taxonomy,
//! contract versioning, content with its provenance, the permission vocabulary, the journal record
//! shape, and the two primitives every layer prices or stops work with. Every other crate depends
//! on nothing but this one, which is what lets a component be swapped without the components around
//! it changing.

pub mod error;
pub mod ids;
pub mod journal;
pub mod message;
pub mod meter;
pub mod policy;
pub mod ticket;
pub mod trust;
pub mod version;

pub use error::{Result, ToolOutcome, VikramError};
pub use journal::{Appended, InMemoryJournal, JournalRecord, JournalSink, NewRecord, RecordKind};
pub use message::{Block, ImageSource, Message, Role, ToolCall};
pub use meter::{CancellationToken, TokenMeter};
pub use policy::{
    ActionKind, Decision, PermissionMode, PermissionPolicy, PermissionRequest, Posture,
    RuleOutcome, SideEffect, amendment_for,
};
pub use trust::Trust;
pub use version::{
    ContractDecl, ContractSpec, JOURNAL_SINK, MEMORY_STORE, MODEL_PROVIDER, POLICY_EVALUATOR,
    TOOL_EXECUTOR, Versioned,
};

pub use ids::{
    AgentId, ApprovalId, AssignmentId, BotId, EffectId, EntityId, HiveId, HiveMessageId,
    HiveRosterId, ProfileId, ProjectId, SessionId, TicketEventId, TicketId, TicketMessageId,
    ToolCallId, TurnId, UserId, WorkspaceId,
};
pub use ticket::*;

/// The contract version this crate speaks. A component declaring a newer major than the host
/// offers is refused at activation rather than failing at run time.
pub const HOST_CONTRACT: ContractSpec = ContractSpec::new(1, 0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_contract_advertises_components_built_against_it() {
        let component = ContractDecl::new(crate::version::MEMORY_STORE, 1, 0);
        assert!(component.satisfied_by(HOST_CONTRACT).is_ok());
    }

    #[test]
    fn messages_are_json_round_trippable_for_the_wire_protocol() {
        let msg = Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall {
                call: ToolCall::new("fs.read", serde_json::json!({"path": "/etc/hosts"})),
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), msg);
    }
}
