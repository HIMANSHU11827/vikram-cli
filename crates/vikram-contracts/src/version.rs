//! Contract identity and compatibility. Every replaceable component declares one of these and
//! the registry refuses to activate it unless the host satisfies the declaration.

use crate::error::{Result, VikramError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ContractSpec {
    pub major: u32,
    pub minor: u32,
}

impl ContractSpec {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractDecl {
    pub id: &'static str,
    pub spec: ContractSpec,
}

impl ContractDecl {
    pub const fn new(id: &'static str, major: u32, minor: u32) -> Self {
        Self {
            id,
            spec: ContractSpec { major, minor },
        }
    }

    /// Same major line, and the host already offers at least the minor the component needs.
    pub fn satisfied_by(&self, host: ContractSpec) -> Result<()> {
        if host.major != self.spec.major {
            return Err(VikramError::ContractIncompatible {
                id: self.id.into(),
                required: self.spec.major,
                host: host.major,
            });
        }
        if host.minor < self.spec.minor {
            return Err(VikramError::ContractIncompatible {
                id: self.id.into(),
                required: self.spec.minor,
                host: host.minor,
            });
        }
        Ok(())
    }
}

pub trait Versioned {
    fn contract(&self) -> ContractDecl;
}

pub const KERNEL_LIFECYCLE: &str = "vikram.kernel.lifecycle";
pub const JOURNAL_SINK: &str = "vikram.store.journal";
pub const POLICY_EVALUATOR: &str = "vikram.policy.evaluator";
pub const EFFECT_HANDLER: &str = "vikram.runtime.effects";
pub const MEMORY_STORE: &str = "vikram.memory.store";
pub const MODEL_PROVIDER: &str = "vikram.model.provider";
pub const TOOL_EXECUTOR: &str = "vikram.tool.executor";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minor_upgrade_is_accepted_and_major_upgrade_is_rejected() {
        let component = ContractDecl::new(MEMORY_STORE, 2, 1);
        assert!(component.satisfied_by(ContractSpec::new(2, 4)).is_ok());
        assert!(component.satisfied_by(ContractSpec::new(2, 1)).is_ok());
        let err = component.satisfied_by(ContractSpec::new(3, 0)).unwrap_err();
        assert!(matches!(
            err,
            VikramError::ContractIncompatible {
                required: 2,
                host: 3,
                ..
            }
        ));
    }

    #[test]
    fn a_component_needing_newer_minor_than_the_host_has_is_refused() {
        let component = ContractDecl::new(JOURNAL_SINK, 1, 5);
        let err = component.satisfied_by(ContractSpec::new(1, 2)).unwrap_err();
        assert!(matches!(err, VikramError::ContractIncompatible { .. }));
    }
}
