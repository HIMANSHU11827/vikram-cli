//! Provenance. Every piece of content a model can see carries the trust level of where it came
//! from, so policy can treat instructions and observations differently instead of flattening both
//! into prompt text.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Trust {
    /// Issued by the host process itself: system policy, kernel messages, approval tickets.
    Host,
    /// Typed by the user in a trusted terminal.
    User,
    /// Produced by a model. Never a permission grant.
    Model,
    /// Produced by a tool, a webpage, a file, or any other external source.
    External,
}

impl Trust {
    /// Ordering is "how much authority this may carry", not quality of information.
    pub fn authority(self) -> u8 {
        match self {
            Trust::Host => 3,
            Trust::User => 2,
            Trust::Model => 1,
            Trust::External => 0,
        }
    }

    pub fn is_untrusted(self) -> bool {
        matches!(self, Trust::Model | Trust::External)
    }

    /// The load-bearing rule: nothing below `Host` may widen what an agent is allowed to do,
    /// so a model or a webpage can never authorise itself.
    pub fn may_grant_permission(self) -> bool {
        matches!(self, Trust::Host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_host_content_can_grant_permission() {
        assert!(Trust::Host.may_grant_permission());
        for t in [Trust::User, Trust::Model, Trust::External] {
            assert!(!t.may_grant_permission(), "{t:?} must not grant permission");
        }
    }

    #[test]
    fn model_and_external_are_the_untrusted_tier() {
        assert!(Trust::Model.is_untrusted());
        assert!(Trust::External.is_untrusted());
        assert!(!Trust::User.is_untrusted());
    }

    #[test]
    fn authority_orders_host_above_the_content_a_model_controls() {
        assert!(Trust::Host.authority() > Trust::Model.authority());
        assert!(Trust::Model.authority() > Trust::External.authority());
    }
}
