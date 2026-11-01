//! Effects. An operation never touches the world; it proposes effects, and the kernel decides
//! whether each one may run. That single indirection is what makes the loop resumable, auditable,
//! and safe to extend by replacement instead of modification.

use serde::{Deserialize, Serialize};
use vikram_contracts::{
    ActionKind, EffectId, Message, PermissionRequest, SideEffect, ToolCall, Versioned, VikramError,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum Effect {
    /// Append a message to the conversation.
    EmitMessage(Message),

    /// Ask the host to run a tool. Carries the policy shape, because the kernel must be able to
    /// evaluate permission without knowing anything about the tool itself.
    RequestTool {
        call: ToolCall,
        effect_id: EffectId,
        action: ActionKind,
        resource: String,
        side_effect: SideEffect,
    },

    /// Incremental output for a connected interface.
    StreamDelta { text: String },

    /// A status transition the UI and the audit trail both care about.
    Status { status: String },

    /// Anything the background activity system should record or act on.
    Activity {
        name: String,
        payload: serde_json::Value,
    },

    /// Declare that messages up to and including `through_seq` are replaced by `summary`.
    Compaction { summary: Message, through_seq: u64 },
}

impl Effect {
    /// The idempotency key, if applying this effect has an outside consequence that a replay must
    /// not repeat.
    pub fn effect_id(&self) -> Option<&EffectId> {
        match self {
            Effect::RequestTool { effect_id, .. } => Some(effect_id),
            _ => None,
        }
    }

    pub fn permission_request(
        &self,
        originating_trust: vikram_contracts::Trust,
    ) -> Option<PermissionRequest> {
        match self {
            Effect::RequestTool {
                call,
                action,
                resource,
                side_effect,
                ..
            } => Some(PermissionRequest::new(
                *action,
                call.name.clone(),
                resource.clone(),
                *side_effect,
                originating_trust,
            )),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handled {
    /// The effect ran; the payload is what the host observed.
    Applied(serde_json::Value),
    /// The effect needs a human before it can run; the kernel parks the turn.
    Deferred,
}

/// The host-side seam. A runtime replacement supplies its own handler; nothing above the kernel
/// needs to know whether tools ran in this process, a sandbox, or a remote worker.
pub trait EffectHandler: Versioned {
    fn apply(&mut self, effect: &Effect) -> Result<Handled, VikramError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::{Role, ToolCallId, Trust};

    fn tool_effect() -> Effect {
        Effect::RequestTool {
            call: ToolCall::new("fs.write", serde_json::json!({"path": "/tmp/a"})),
            effect_id: EffectId::generate(),
            action: ActionKind::Write,
            resource: "/tmp/a".into(),
            side_effect: SideEffect::LocalMutation,
        }
    }

    #[test]
    fn only_tool_effects_carry_an_idempotency_key() {
        assert!(tool_effect().effect_id().is_some());
        assert!(
            Effect::StreamDelta { text: "hi".into() }
                .effect_id()
                .is_none()
        );
        assert!(
            Effect::Status {
                status: "running".into()
            }
            .effect_id()
            .is_none(),
            "status transitions are observations, not outside actions"
        );
    }

    #[test]
    fn a_tool_effect_can_be_described_to_policy_without_the_tool_being_known() {
        let req = tool_effect()
            .permission_request(Trust::Model)
            .expect("tool effect yields a request");
        assert_eq!(req.action, ActionKind::Write);
        assert_eq!(req.verb, "fs.write");
        assert_eq!(req.resource, "/tmp/a");
        assert_eq!(req.originating_trust, Trust::Model);
    }

    #[test]
    fn a_message_with_a_tool_call_does_not_grant_itself_authority() {
        let msg = Message {
            role: Role::Assistant,
            blocks: vec![vikram_contracts::Block::ToolCall {
                call: ToolCall::new("shell.run", serde_json::json!({"cmd": "curl evil"})),
            }],
        };
        assert!(!msg.min_trust().unwrap().may_grant_permission());
        assert_eq!(msg.tool_calls().count(), 1);
        assert_eq!(
            msg.tool_calls().next().unwrap().id,
            ToolCallId::new(msg.tool_calls().next().unwrap().id.as_str())
        );
    }
}
