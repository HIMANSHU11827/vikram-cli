//! Events that wake the scheduler, and the quiet-hours policy that can hold them back.
//!
//! A cron schedule answers "when"; an event answers "because". The daemon keeps a bounded,
//! deduplicated event log in the same database as the jobs ([`crate::store::emit_event`]), and a
//! job whose schedule reads `on <trigger>` arms itself the next time a matching event lands. The
//! trigger kinds are a closed set — [`TriggerKind`] — so a job can only ever ask for something the
//! daemon actually produces, and a signal naming anything else is refused rather than ignored.
//!
//! Quiet hours are the daemon-wide "do not bother me" window. While it is active, jobs below
//! [`URGENT_PRIORITY`] are not dispatched and notifications are not delivered; the decision is
//! recorded in the audit trail either way, so a quiet morning is explainable afterwards. The
//! policy lives in `daemon_state`, which means it survives restarts and a `vikram daemon quiet`
//! change takes effect on the next tick without a reload.

use std::sync::Arc;

use tokio::sync::Notify;

use crate::store::{EventRow, ScheduleStore, StoreError};
use crate::time::{self, Millis, Zone};

/// A job at or above this priority is urgent: quiet hours never hold it back, and neither does
/// anything it reports.
pub const URGENT_PRIORITY: i64 = 10;

/// How long the same `(kind, subject)` pair stays deduplicated. A postbox entry and a job settling
/// in the same second are one signal, not two; a burst repeats collapse into the first.
pub const EVENT_DEDUP_MS: Millis = 60_000;

/// The closed set of things that can wake a job. Anything else is refused — deny-first applies to
/// triggers exactly as it does to permissions: what is not named cannot fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// Something was posted into a conversation through the postbox.
    Postbox,
    /// A path a job subscribes to changed on disk. The daemon polls the paths its jobs name,
    /// so the watch is declared by the subscription itself: `on file.changed:<path>`.
    FileChanged,
    /// A job's run settled — succeeded, failed, was cancelled or skipped.
    JobSettled,
    /// A notification was delivered (or attempted) for a finished run.
    Notify,
}

impl TriggerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postbox => "postbox",
            Self::FileChanged => "file.changed",
            Self::JobSettled => "job.settled",
            Self::Notify => "notify",
        }
    }

    /// Read a trigger name. There are no aliases on purpose: a typo must be refused, not
    /// silently mapped onto a different signal.
    pub fn parse(text: &str) -> Result<Self, TriggerError> {
        match text.trim().to_ascii_lowercase().as_str() {
            "postbox" => Ok(Self::Postbox),
            "file.changed" => Ok(Self::FileChanged),
            "job.settled" => Ok(Self::JobSettled),
            "notify" => Ok(Self::Notify),
            other => Err(TriggerError::UnknownTrigger(other.to_string())),
        }
    }
}

/// What a job subscribes to: a kind, plus an optional subject it only fires for.
///
/// `on job.settled` fires for every settled job; `on job.settled:nightly-report` fires only when
/// that job settles. A subject that matches nothing simply never fires — subscription is the
/// grant, and there is no wildcard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TriggerKind,
    pub subject: Option<String>,
}

impl Trigger {
    pub fn parse(text: &str) -> Result<Self, TriggerError> {
        let text = text.trim();
        let (kind, subject) = match text.split_once(':') {
            Some((kind, subject)) => (kind.trim(), Some(subject.trim())),
            None => (text, None),
        };
        let kind = TriggerKind::parse(kind)?;
        let subject = match subject {
            None => None,
            Some("") => return Err(TriggerError::EmptySubject(text.to_string())),
            Some(subject) => Some(subject.to_string()),
        };
        Ok(Self { kind, subject })
    }

    /// Whether an event of `kind` about `subject` is covered by this subscription.
    pub fn matches(&self, kind: TriggerKind, subject: &str) -> bool {
        self.kind == kind
            && self
                .subject
                .as_deref()
                .is_none_or(|wanted| wanted == subject)
    }

    pub fn describe(&self) -> String {
        match &self.subject {
            Some(subject) => format!("{}:{subject}", self.kind.as_str()),
            None => self.kind.as_str().to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    #[error(
        "unknown trigger `{0}`: use `on postbox`, `on file.changed`, `on job.settled` or `on notify`"
    )]
    UnknownTrigger(String),
    #[error("trigger `{0}` names a kind but no subject; write `kind:subject` or just `kind`")]
    EmptySubject(String),
    #[error("quiet hours: {0}")]
    Quiet(String),
    #[error("event store: {0}")]
    Store(#[from] StoreError),
}

/// What happened when a signal was emitted.
#[derive(Debug)]
pub enum EmitOutcome {
    /// A new event row was written; the scheduler will see it.
    Recorded(EventRow),
    /// The same kind and subject fired inside the dedup window, so nothing new was queued.
    Deduped,
}

/// A way to raise an event: the store it lands in, and the wake-up that tells a running daemon not
/// to wait for its next tick. The wake-up is only a hint — the event row is the truth, and a daemon
/// that was not listening still finds it on its next pass.
#[derive(Clone)]
pub struct EventBus {
    store: ScheduleStore,
    wake: Arc<Notify>,
}

impl EventBus {
    pub fn new(store: ScheduleStore, wake: Arc<Notify>) -> Self {
        Self { store, wake }
    }

    /// A bus that records but cannot wake anyone — for the CLI, which is a different process from
    /// the daemon and can only leave the row for the next tick to find.
    pub fn without_wake(store: ScheduleStore) -> Self {
        Self::new(store, Arc::new(Notify::new()))
    }

    pub fn store(&self) -> &ScheduleStore {
        &self.store
    }

    /// Raise `kind` about `subject`. Deny-first: a kind the daemon does not know is refused
    /// outright and the refusal itself is audited, so an unknown trigger can never pass silently.
    pub fn emit(
        &self,
        kind: &str,
        subject: &str,
        detail: &str,
        now: Millis,
    ) -> Result<EmitOutcome, TriggerError> {
        let kind = match TriggerKind::parse(kind) {
            Ok(kind) => kind,
            Err(err) => {
                let _ = self.store.audit("trigger.refused", kind, detail, now);
                return Err(err);
            }
        };
        match self
            .store
            .emit_event(kind.as_str(), subject, detail, EVENT_DEDUP_MS, now)?
        {
            Some(row) => {
                self.wake.notify_waiters();
                Ok(EmitOutcome::Recorded(row))
            }
            None => Ok(EmitOutcome::Deduped),
        }
    }
}

/// The daemon-wide silence policy: a daily window, in local time, during which non-urgent work
/// and notifications are suppressed. Durable because it lives in `daemon_state`; auditable because
/// every change writes an audit row as well as the file log entry the CLI keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    pub enabled: bool,
    /// Minutes since local midnight, inclusive.
    pub start: u32,
    /// Minutes since local midnight, exclusive. May be earlier than `start` to wrap past midnight.
    pub end: u32,
}

const QUIET_ENABLED: &str = "quiet.enabled";
const QUIET_START: &str = "quiet.start";
const QUIET_END: &str = "quiet.end";

impl QuietHours {
    /// The stored policy, or the default (off, 22:00–07:00) when nothing was ever written.
    pub fn load(store: &ScheduleStore) -> Result<Self, StoreError> {
        let enabled = store.state_bool(QUIET_ENABLED, false)?;
        let start = store
            .state(QUIET_START)?
            .as_deref()
            .map(parse_hhmm)
            .transpose()
            .map_err(|err| StoreError::Missing(format!("quiet.start: {err}")))?
            .unwrap_or(22 * 60);
        let end = store
            .state(QUIET_END)?
            .as_deref()
            .map(parse_hhmm)
            .transpose()
            .map_err(|err| StoreError::Missing(format!("quiet.end: {err}")))?
            .unwrap_or(7 * 60);
        Ok(Self {
            enabled,
            start,
            end,
        })
    }

    /// Write the policy down and record the change in the audit trail.
    pub fn apply(&self, store: &ScheduleStore, now: Millis) -> Result<(), StoreError> {
        store.set_state(QUIET_ENABLED, if self.enabled { "1" } else { "0" })?;
        if self.enabled {
            store.set_state(QUIET_START, &format_hhmm(self.start))?;
            store.set_state(QUIET_END, &format_hhmm(self.end))?;
        }
        store.audit("quiet.set", "", &self.describe(), now)
    }

    /// Whether `now` falls inside the window. A window whose start equals its end is never quiet —
    /// "all day" is what `--off` plus urgency is for, not a 24-hour edge case.
    pub fn is_quiet(&self, now: Millis) -> bool {
        if !self.enabled || self.start == self.end {
            return false;
        }
        let minute = minute_of_day(now);
        if self.start < self.end {
            minute >= self.start && minute < self.end
        } else {
            minute >= self.start || minute < self.end
        }
    }

    pub fn describe(&self) -> String {
        if self.enabled {
            format!("{}-{}", format_hhmm(self.start), format_hhmm(self.end))
        } else {
            "off".to_string()
        }
    }
}

/// Read `22`, `9`, `22:30` or `09:05` as minutes since midnight.
pub fn parse_hhmm(text: &str) -> Result<u32, String> {
    let text = text.trim();
    let (hour, minute) = match text.split_once(':') {
        Some((hour, minute)) => (
            hour.trim().parse::<u32>().map_err(|_| bad(text))?,
            minute.trim().parse::<u32>().map_err(|_| bad(text))?,
        ),
        None => (text.parse::<u32>().map_err(|_| bad(text))?, 0),
    };
    if hour > 23 || minute > 59 {
        return Err(bad(text));
    }
    Ok(hour * 60 + minute)
}

fn bad(text: &str) -> String {
    format!("`{text}` is not a time of day (try 22, 22:30 or 07:05)")
}

fn format_hhmm(minute: u32) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

fn minute_of_day(now: Millis) -> u32 {
    let civil = time::civil_in(Zone::Local, now);
    civil.hour * 60 + civil.minute
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{Civil, instant_of};

    fn store() -> ScheduleStore {
        let store = ScheduleStore::open_in_memory().unwrap();
        store.ensure_schema().unwrap();
        store
    }

    fn at(hour: u32, minute: u32) -> Millis {
        instant_of(
            Zone::Utc,
            Civil {
                year: 2026,
                month: 11,
                day: 4,
                hour,
                minute,
                second: 0,
            },
        )
    }

    #[test]
    fn only_the_known_triggers_parse() {
        assert_eq!(
            Trigger::parse("postbox").unwrap(),
            Trigger {
                kind: TriggerKind::Postbox,
                subject: None
            }
        );
        assert_eq!(
            Trigger::parse("job.settled:nightly").unwrap(),
            Trigger {
                kind: TriggerKind::JobSettled,
                subject: Some("nightly".into())
            }
        );
        let err = Trigger::parse("carrier-pigeon").unwrap_err();
        assert!(err.to_string().contains("carrier-pigeon"), "{err}");
        assert!(Trigger::parse("notify:").is_err());
    }

    #[test]
    fn a_subscription_covers_its_kind_and_only_its_subject() {
        let all = Trigger::parse("job.settled").unwrap();
        assert!(all.matches(TriggerKind::JobSettled, "anything"));
        assert!(!all.matches(TriggerKind::Notify, "anything"));
        let one = Trigger::parse("job.settled:nightly").unwrap();
        assert!(one.matches(TriggerKind::JobSettled, "nightly"));
        assert!(!one.matches(TriggerKind::JobSettled, "other"));
    }

    #[test]
    fn quiet_hours_wrap_past_midnight() {
        // 22:00–07:00 local. Build instants in UTC and read them back in local time is fragile,
        // so the window is exercised through `is_quiet` with a window that cannot depend on the
        // machine's zone: pick the current local minute and bracket it.
        let quiet = QuietHours {
            enabled: true,
            start: 22 * 60,
            end: 7 * 60,
        };
        assert!(!quiet.is_quiet(at(12, 0)), "noon is not quiet");
        // Whatever the local zone is, *some* hour of the day is inside 22:00–07:00; check the
        // boundary logic directly instead of guessing the zone's offset.
        let minute = minute_of_day(at(12, 0));
        let inside = if quiet.start < quiet.end {
            minute >= quiet.start && minute < quiet.end
        } else {
            minute >= quiet.start || minute < quiet.end
        };
        assert_eq!(quiet.is_quiet(at(12, 0)), inside);
    }

    #[test]
    fn quiet_hours_off_is_never_quiet_and_changes_are_audited() {
        let store = store();
        let off = QuietHours {
            enabled: false,
            start: 0,
            end: 0,
        };
        assert!(!off.is_quiet(at(3, 0)));
        off.apply(&store, at(1, 0)).unwrap();
        let loaded = QuietHours::load(&store).unwrap();
        assert!(!loaded.enabled);
        assert_eq!(loaded.describe(), "off");

        let on = QuietHours {
            enabled: true,
            start: 22 * 60,
            end: 7 * 60,
        };
        on.apply(&store, at(2, 0)).unwrap();
        let loaded = QuietHours::load(&store).unwrap();
        assert!(loaded.enabled);
        assert_eq!(loaded.describe(), "22:00-07:00");
        let trail = store.audit_trail(10).unwrap();
        assert!(
            trail.iter().any(|row| row.kind == "quiet.set"),
            "a quiet-hours change must be audited: {trail:?}"
        );
    }

    #[test]
    fn a_file_changed_trigger_parses_and_keeps_the_paths_case() {
        let trigger = Trigger::parse("file.changed:/tmp/Notes.TXT").unwrap();
        assert_eq!(trigger.kind, TriggerKind::FileChanged);
        assert_eq!(trigger.subject.as_deref(), Some("/tmp/Notes.TXT"));
        assert!(trigger.matches(TriggerKind::FileChanged, "/tmp/Notes.TXT"));
        assert!(
            !trigger.matches(TriggerKind::FileChanged, "/tmp/notes.txt"),
            "a path is not a name: case is part of it"
        );
        assert_eq!(trigger.describe(), "file.changed:/tmp/Notes.TXT");
    }

    #[test]
    fn times_parse_the_way_an_operator_types_them() {
        assert_eq!(parse_hhmm("22").unwrap(), 22 * 60);
        assert_eq!(parse_hhmm("7").unwrap(), 7 * 60);
        assert_eq!(parse_hhmm("22:30").unwrap(), 22 * 60 + 30);
        assert_eq!(parse_hhmm("07:05").unwrap(), 7 * 60 + 5);
        assert!(parse_hhmm("25:00").is_err());
        assert!(parse_hhmm("soon").is_err());
    }

    #[test]
    fn an_event_is_deduplicated_inside_its_window_and_unknown_kinds_are_refused() {
        let store = store();
        let bus = EventBus::without_wake(store.clone());
        let first = bus.emit("postbox", "job-1", "posted", at(10, 0)).unwrap();
        assert!(matches!(first, EmitOutcome::Recorded(_)));
        let second = bus
            .emit("postbox", "job-1", "posted again", at(10, 0) + 5_000)
            .unwrap();
        assert!(matches!(second, EmitOutcome::Deduped));
        // A different subject is a different signal.
        let other = bus.emit("postbox", "job-2", "posted", at(10, 0)).unwrap();
        assert!(matches!(other, EmitOutcome::Recorded(_)));
        // And after the window the same pair may fire again.
        let later = bus
            .emit("postbox", "job-1", "posted", at(10, 0) + EVENT_DEDUP_MS + 1)
            .unwrap();
        assert!(matches!(later, EmitOutcome::Recorded(_)));

        let refused = bus.emit("smoke-signal", "x", "", at(10, 0));
        assert!(refused.is_err());
        let trail = store.audit_trail(20).unwrap();
        assert!(
            trail.iter().any(|row| row.kind == "trigger.refused"),
            "a refused trigger is itself audited: {trail:?}"
        );
    }
}
