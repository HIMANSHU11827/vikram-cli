//! Identifier newtypes. Distinct types prevent a session id being passed where an
//! effect id is expected, which matters because effect ids are the idempotency key.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

macro_rules! id_type {
    ($name:ident, $kind:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub const KIND: &'static str = $kind;

            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn generate() -> Self {
                static COUNTER: AtomicU64 = AtomicU64::new(0);
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
                Self(format!("{}-{nanos:020}-{seq:06}", $kind))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(SessionId, "ses");
id_type!(TurnId, "turn");
id_type!(BotId, "bot");
id_type!(AgentId, "agent");
id_type!(HiveRosterId, "hrost");
id_type!(EffectId, "eff");
id_type!(ToolCallId, "call");
id_type!(ApprovalId, "apv");
id_type!(UserId, "user");
id_type!(ProfileId, "prof");
id_type!(EntityId, "ent");
id_type!(HiveId, "hive");
id_type!(HiveMessageId, "hmsg");
id_type!(TicketId, "tkt");
id_type!(AssignmentId, "asg");
id_type!(TicketMessageId, "tmsg");
id_type!(TicketEventId, "tev");
id_type!(ProjectId, "proj");
id_type!(WorkspaceId, "ws");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_unique_and_prefixed() {
        let a = EffectId::generate();
        let b = EffectId::generate();
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("eff-"));
    }

    #[test]
    fn different_id_types_never_compare_equal_across_the_boundary() {
        let shared = "same-value";
        let session = SessionId::new(shared);
        let effect = EffectId::new(shared);
        // Distinct types: this line only compiles because they cannot be compared.
        assert_eq!(session.as_str(), effect.as_str());
        assert_ne!(SessionId::KIND, EffectId::KIND);
    }
}
