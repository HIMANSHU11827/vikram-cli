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

/// A rustls client config rooted at Mozilla's CAs via webpki anchors. Handed to reqwest through
/// `use_preconfigured_tls`, it bypasses `rustls-platform-verifier` — which panics on
/// Android/Termux because it expects a JVM — and behaves identically on every platform.
pub(crate) fn tls_config() -> std::sync::Arc<rustls::ClientConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

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
