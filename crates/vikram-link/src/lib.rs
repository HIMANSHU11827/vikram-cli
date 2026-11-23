//! VIKRAM LINK: one runtime, many linked devices, one tier each.
//!
//! The server is the whole of this crate. It speaks HTTP for questions and a WebSocket for the same
//! [`vikram_runtime::types::RuntimeEvent`] stream the terminal consumes, and it answers every
//! request through one boundary — [`policy::authorize`] — before the runtime is touched.
//!
//! Three rules hold everywhere:
//!
//! * a secret is shown once, and only its SHA-256 is stored;
//! * a refusal names the tier, so a device can be told what it needs instead of guessing;
//! * every remote action, allowed or denied, appends one line to `link/audit.jsonl`.
//!
//! ```no_run
//! use vikram_link::{Server, ServerConfig};
//! # fn main() -> Result<(), String> {
//! # let runtime: vikram_runtime::RuntimeHandle = unimplemented!();
//! # let tokio: tokio::runtime::Handle = unimplemented!();
//! let server = Server::bind(ServerConfig::default())?;
//! println!("serving on {}://{}", server.scheme(), server.local_addr());
//! server.serve(runtime, tokio)
//! # }
//! ```

pub mod audit;
pub mod http;
pub mod policy;
pub mod server;
pub mod tier;
pub mod time;
pub mod token;
pub mod wire;

pub use audit::{Audit, AuditEntry};
pub use policy::{Action, Denial, Target, authorize};
pub use server::{Server, ServerConfig};
pub use tier::{SessionOwner, TIER_LIST, TIERS, Tier, TierError};
pub use token::{
    AddRequest, Lifetime, Minted, TokenStore, TokenSummary, Verdict, Verified, max_temp_ms,
};
