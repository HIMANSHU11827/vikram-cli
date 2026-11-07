//! Messaging platform gateways for VIKRAM.
//!
//! Ingress from chat platforms (Telegram, Discord, Slack, ...) in, an agent turn
//! through `vikram-runtime`, delivery back out. Rust port of the hermes-agent
//! gateway — see `docs/gateway-port-plan.md`.
//!
//! Nothing here formats terminal output; each platform renders for its own wire
//! format. The gateway is one subscriber to the runtime, exactly like the TUI and
//! the CLI chat path.

#![forbid(unsafe_code)]

pub mod access;
pub mod delivery;
pub mod event;
pub mod helpers;
pub mod host;
pub mod ingress;
pub mod jwt;
pub mod media_fetch;
pub mod pairing;
pub mod platform;
pub mod platforms;
pub mod registry;
pub mod routing;
pub mod socket;
pub mod transport;
