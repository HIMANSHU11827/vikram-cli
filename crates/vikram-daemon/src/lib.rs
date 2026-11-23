//! Vikram's background runtime: the daemon, the cron scheduler, the heartbeat and the workers.
//!
//! The pieces are separate on purpose. The daemon keeps the process alive and supervised, cron
//! decides *when* work runs, the heartbeat decides whether anything needs *attention*, and the
//! workers do the work. Scheduling never executes, execution never notifies, and notification never
//! pretends it succeeded.

/// A rustls client config rooted at Mozilla's CAs via webpki anchors — bypasses
/// `rustls-platform-verifier`, which panics on Android/Termux (it expects a JVM).
pub(crate) fn tls_config() -> rustls::ClientConfig {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

pub mod cli;
pub mod cron;
pub mod daemon;
pub mod heartbeat;
pub mod jobs;
pub mod notify;
pub mod postbox;
pub mod presence;
pub mod runner;
pub mod service;
pub mod store;
pub mod time;
pub mod tools;
pub mod triggers;
pub mod worker;

/// `VIKRAM_HOME` is process-wide, so the tests that move it take this lock first.
#[cfg(test)]
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
