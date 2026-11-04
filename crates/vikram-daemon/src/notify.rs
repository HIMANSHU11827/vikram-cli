//! Where the result of a background job goes.
//!
//! A scheduler that says "done" without knowing whether anyone heard it is worse than one that
//! stays quiet: the operator plans around a report that never arrived. Every function here therefore
//! returns what actually happened, and a delivery that could not be made is reported as such rather
//! than recorded as a success.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::time::{self, Millis, Zone};

/// How long a webhook may take before the delivery counts as failed.
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(15);

/// A destination a job's result can be sent to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// Deliberately nowhere: a job whose only purpose is the work itself.
    None,
    /// A line in this machine's notification log, which is what the TUI and the CLI read.
    Local,
    /// A vikram session, addressed by id, which is how a background result reaches a conversation.
    Session(String),
    /// An HTTP endpoint, posted as JSON.
    Webhook(String),
}

impl Destination {
    /// Read what the operator wrote in `--deliver`.
    ///
    /// An empty target means nowhere, because a job with no destination has nothing to be quiet
    /// about. A bare `session` without an id is refused: guessing which conversation a report belongs
    /// to is exactly the mistake that leaks one job's output into another's.
    pub fn parse(text: &str) -> Result<Self, NotifyError> {
        let text = text.trim();
        if text.is_empty() || text.eq_ignore_ascii_case("none") || text.eq_ignore_ascii_case("off")
        {
            return Ok(Self::None);
        }
        if text.eq_ignore_ascii_case("local") || text.eq_ignore_ascii_case("log") {
            return Ok(Self::Local);
        }
        if let Some(id) = text.strip_prefix("session:") {
            let id = id.trim();
            if id.is_empty() {
                return Err(NotifyError::Unknown(text.to_string()));
            }
            return Ok(Self::Session(id.to_string()));
        }
        if let Some(url) = text.strip_prefix("webhook:") {
            return Ok(Self::Webhook(url.trim().to_string()));
        }
        if text.starts_with("http://") || text.starts_with("https://") {
            return Ok(Self::Webhook(text.to_string()));
        }
        Err(NotifyError::Unknown(text.to_string()))
    }

    /// The name stored on a run record, so history says where the result went.
    pub fn label(&self) -> String {
        match self {
            Self::None => "none".into(),
            Self::Local => "local".into(),
            Self::Session(id) => format!("session:{id}"),
            Self::Webhook(url) => format!("webhook:{url}"),
        }
    }
}

/// What a delivery attempt is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub job: String,
    pub name: String,
    /// `ok`, `failed`, `unknown` — the same word the run record uses.
    pub status: String,
    pub summary: String,
    pub session: Option<String>,
    pub at: Millis,
}

impl Notice {
    /// One line, exactly as it appears in the log and in a chat message.
    pub fn line(&self) -> String {
        let stamp = time::format_ms(Zone::Local, self.at);
        format!(
            "{stamp}  {}  {}  {}",
            self.name,
            self.status,
            self.summary.trim()
        )
    }
}

/// Whether the notice reached somewhere, and what to tell the operator when it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub delivered: bool,
    pub detail: String,
}

impl Report {
    fn yes(detail: impl Into<String>) -> Self {
        Self {
            delivered: true,
            detail: detail.into(),
        }
    }

    fn no(detail: impl Into<String>) -> Self {
        Self {
            delivered: false,
            detail: detail.into(),
        }
    }
}

/// How a session delivery is performed. The daemon owns the running runtime, so it supplies this
/// closure instead of this module owning a runtime of its own.
pub type SessionSink<'a> = dyn Fn(&str, &str) -> Result<(), String> + Send + Sync + 'a;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NotifyError {
    #[error("unknown delivery target `{0}`: use local, session:<id>, webhook:<url> or none")]
    Unknown(String),
}

/// Deliver `notice` to `destination`, and say whether it arrived.
pub async fn deliver(
    destination: &Destination,
    notice: &Notice,
    home: &Path,
    sink: Option<&SessionSink<'_>>,
) -> Report {
    match destination {
        Destination::None => Report::yes("nowhere to deliver"),
        Destination::Local => append_log(home, &notice.line()),
        Destination::Session(id) => match sink {
            Some(sink) => match sink(id, &notice.line()) {
                Ok(()) => Report::yes(format!("session {id}")),
                Err(err) => Report::no(format!("session {id}: {err}")),
            },
            // Without a sink there is no runtime listening, and pretending otherwise would tell the
            // operator a report was handed over when nobody was there to take it.
            None => Report::no(format!("session {id}: no runtime is listening")),
        },
        Destination::Webhook(url) => post_webhook(url, notice).await,
    }
}

/// The notification log's path, whether or not it exists yet.
pub fn log_path(home: &Path) -> PathBuf {
    home.join("daemon").join("notified.log")
}

fn append_log(home: &Path, line: &str) -> Report {
    let path = log_path(home);
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return Report::no(format!("{}: {err}", parent.display()));
    }
    use std::io::Write;
    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    let mut file = match opened {
        Ok(file) => file,
        Err(err) => return Report::no(format!("{}: {err}", path.display())),
    };
    match writeln!(file, "{line}").and_then(|()| file.flush()) {
        Ok(()) => Report::yes(path.display().to_string()),
        Err(err) => Report::no(format!("{}: {err}", path.display())),
    }
}

async fn post_webhook(url: &str, notice: &Notice) -> Report {
    let body = serde_json::json!({
        "job": notice.job,
        "name": notice.name,
        "status": notice.status,
        "summary": notice.summary,
        "session": notice.session,
        "at": notice.at,
        "at_text": time::format_ms(Zone::Local, notice.at),
    });
    let client = match reqwest::Client::builder().timeout(WEBHOOK_TIMEOUT).build() {
        Ok(client) => client,
        Err(err) => return Report::no(format!("client: {err}")),
    };
    match client.post(url).json(&body).send().await {
        Ok(response) if response.status().is_success() => {
            Report::yes(format!("{url} {}", response.status().as_u16()))
        }
        Ok(response) => Report::no(format!("{url} answered {}", response.status().as_u16())),
        Err(err) => Report::no(format!("{url}: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice() -> Notice {
        Notice {
            job: "nightly".into(),
            name: "nightly report".into(),
            status: "ok".into(),
            summary: "3 findings".into(),
            session: None,
            at: 1_800_000_000_000,
        }
    }

    #[test]
    fn every_target_an_operator_types_is_understood() {
        assert_eq!(Destination::parse("").unwrap(), Destination::None);
        assert_eq!(Destination::parse("none").unwrap(), Destination::None);
        assert_eq!(Destination::parse("local").unwrap(), Destination::Local);
        assert_eq!(
            Destination::parse("session:ses_123").unwrap(),
            Destination::Session("ses_123".into())
        );
        assert_eq!(
            Destination::parse("webhook:https://example.test/hook").unwrap(),
            Destination::Webhook("https://example.test/hook".into())
        );
        assert_eq!(
            Destination::parse("https://example.test/hook").unwrap(),
            Destination::Webhook("https://example.test/hook".into())
        );
    }

    #[test]
    fn a_target_that_means_nothing_is_refused_rather_than_guessed() {
        assert!(Destination::parse("telegram").is_err());
        assert!(Destination::parse("session:").is_err());
    }

    #[test]
    fn a_label_says_where_the_result_went() {
        assert_eq!(Destination::Local.label(), "local");
        assert_eq!(
            Destination::Session("ses_1".into()).label(),
            "session:ses_1"
        );
    }

    #[tokio::test]
    async fn a_local_delivery_is_a_line_the_log_really_holds() {
        let home = tempfile::tempdir().unwrap();
        let report = deliver(&Destination::Local, &notice(), home.path(), None).await;
        assert!(report.delivered, "{report:?}");
        let written = std::fs::read_to_string(log_path(home.path())).unwrap();
        assert!(written.contains("nightly report"), "{written}");
        assert!(written.contains("3 findings"), "{written}");

        // A second delivery appends rather than replacing: the log is history, not a status slot.
        deliver(&Destination::Local, &notice(), home.path(), None).await;
        let written = std::fs::read_to_string(log_path(home.path())).unwrap();
        assert_eq!(written.lines().count(), 2, "{written}");
    }

    #[tokio::test]
    async fn a_session_delivery_without_a_runtime_is_reported_as_undelivered() {
        let home = tempfile::tempdir().unwrap();
        let report = deliver(
            &Destination::Session("ses_1".into()),
            &notice(),
            home.path(),
            None,
        )
        .await;
        assert!(!report.delivered);
        assert!(report.detail.contains("no runtime"), "{report:?}");
    }

    #[tokio::test]
    async fn a_session_delivery_that_the_runtime_refuses_is_not_a_success() {
        let home = tempfile::tempdir().unwrap();
        let sink = |_: &str, _: &str| Err("session is closed".to_string());
        let report = deliver(
            &Destination::Session("ses_1".into()),
            &notice(),
            home.path(),
            Some(&sink),
        )
        .await;
        assert!(!report.delivered);
        assert!(report.detail.contains("closed"), "{report:?}");
    }

    #[tokio::test]
    async fn a_log_that_cannot_be_written_says_so_instead_of_claiming_delivery() {
        // `home` is a file, so the daemon directory cannot be created under it.
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();
        let report = deliver(&Destination::Local, &notice(), &file, None).await;
        assert!(!report.delivered, "{report:?}");
    }

    #[tokio::test]
    async fn nothing_to_deliver_is_not_a_failure() {
        let home = tempfile::tempdir().unwrap();
        let report = deliver(&Destination::None, &notice(), home.path(), None).await;
        assert!(report.delivered);
    }
}
