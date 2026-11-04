//! Delivery into a conversation.
//!
//! A job can report into a session instead of a log file, which is what makes "send me a report
//! every morning" a message you can read next to the request that started it. The daemon has no
//! connection to the session you are typing in — that one belongs to the process you are typing in
//! — so it keeps a runtime and a conversation of its own, stored in the same database the terminal
//! reads. A delivery names that conversation, or a session this runtime already holds, and the
//! answer says which one it reached. Nothing here claims a delivery it could not make.

use crate::store::ScheduleStore;
use vikram_contracts::SessionId;
use vikram_runtime::config::Config;
use vikram_runtime::types::RuntimeStatus;
use vikram_runtime::{Runtime, RuntimeHandle};

/// The title of the conversation the daemon opens for reports that name no session of its own.
pub const REPORTS_TITLE: &str = "Background reports";

/// Where a scheduled report lands when it asks for a conversation.
pub struct Postbox {
    store: ScheduleStore,
    runtime: Option<Runtime>,
    handle: Option<RuntimeHandle>,
    reports: Option<SessionId>,
}

/// The key the reports conversation is remembered under, so a daemon restarted tomorrow writes
/// into the same conversation instead of opening a new one every morning.
const REPORTS_KEY: &str = "delivery.session";

impl Postbox {
    pub fn new(store: ScheduleStore) -> Self {
        Self {
            store,
            runtime: None,
            handle: None,
            reports: None,
        }
    }

    /// Whether a runtime is already open; opening one is the expensive part.
    pub fn is_open(&self) -> bool {
        self.handle.is_some()
    }

    /// Open the runtime, or hand back the one already open.
    ///
    /// A daemon whose machine has no model configured can still run scripts, so this failing is not
    /// fatal: it is reported per delivery, where the operator can see which report went missing.
    pub async fn open(&mut self) -> Result<RuntimeHandle, String> {
        if let Some(handle) = &self.handle {
            return Ok(handle.clone());
        }
        let config = Config::load().map_err(|err| format!("config: {err}"))?;
        let runtime = Runtime::start(config)
            .await
            .map_err(|err| format!("could not start a runtime: {err}"))?;
        let handle = runtime.handle();
        if let RuntimeStatus::Error(reason) = handle.status() {
            return Err(format!(
                "{reason}; run `vikram config set model <id>` and set the key it names"
            ));
        }
        self.runtime = Some(runtime);
        self.handle = Some(handle.clone());
        Ok(handle)
    }

    /// Deliver `text` into the conversation named by `wanted`.
    ///
    /// Returns the session it landed in and, when that is not the one asked for, a sentence saying
    /// so. A session id the runtime does not hold is the ordinary case — the terminal's session is
    /// in another process — so the report goes to the daemon's own conversation instead of being
    /// dropped, and the detail travels with it.
    pub async fn post(
        &mut self,
        wanted: &str,
        text: &str,
    ) -> Result<(String, Option<String>), String> {
        let handle = self.open().await?;
        let wanted = wanted.trim();
        let named = !wanted.is_empty() && !wanted.eq_ignore_ascii_case("reports");
        if named {
            let wanted_id = SessionId::new(wanted);
            if handle.post_message(&wanted_id, text.to_string()).is_ok() {
                return Ok((wanted.to_string(), None));
            }
        }
        let session = match &self.reports {
            Some(id) => id.clone(),
            None => {
                // A remembered conversation is reused when this runtime rehydrated it from the
                // database; when it did not, a new one is opened and the memory is corrected, so a
                // stale id cannot swallow the report.
                if let Ok(Some(remembered)) = self.store.state(REPORTS_KEY) {
                    let id = SessionId::new(remembered);
                    if handle.post_message(&id, text.to_string()).is_ok() {
                        self.reports = Some(id.clone());
                        return Ok((
                            id.to_string(),
                            named.then(|| format!("`{wanted}` is not open here, so the report went to {REPORTS_TITLE} ({id})")),
                        ));
                    }
                }
                let id = handle
                    .create_session(Some(REPORTS_TITLE.to_string()))
                    .map_err(|err| format!("could not open a conversation: {err}"))?;
                let _ = self.store.set_state(REPORTS_KEY, id.as_str());
                self.reports = Some(id.clone());
                id
            }
        };
        handle
            .post_message(&session, text.to_string())
            .map_err(|err| format!("{session}: {err}"))?;
        let note = named.then(|| {
            format!(
                "`{wanted}` is not open in the daemon's runtime, so the report went to {REPORTS_TITLE} ({session})"
            )
        });
        Ok((session.to_string(), note))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_postbox_nobody_opened_holds_no_conversation() {
        let postbox = Postbox::new(ScheduleStore::open_in_memory().expect("store"));
        assert!(!postbox.is_open());
        assert!(postbox.reports.is_none());
    }

    #[test]
    fn the_reports_conversation_is_named_the_way_the_operator_will_see_it() {
        assert_eq!(REPORTS_TITLE, "Background reports");
    }
}
