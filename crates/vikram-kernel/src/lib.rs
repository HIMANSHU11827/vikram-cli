//! VIKRAM's kernel: the one execution architecture.
//!
//! It owns four things and nothing else: the ordered operation loop, the effect gate that sits in
//! front of every outside action, the journal write that makes each effect replay-safe, and the
//! grant set that only a host-issued approval ticket may widen. Reasoning, planning, provider
//! calls, tools, memory and verification all arrive as operations above this, which is why there
//! is exactly one loop for chat, coding, research and background work.

pub mod effects;
pub mod grants;
pub mod machine;
pub mod run;

pub use effects::{Effect, EffectHandler, Handled};
pub use grants::{ApprovalBroker, ApprovalSource, ApprovalTicket, Grant, GrantSet, GrantSnapshot};
pub use machine::{Limits, Machine, Operation, Outcome, PendingCall, RunContext, RunReport};
pub use run::{CancellationToken, Clock, FixedClock, SystemClock, TokenMeter};
