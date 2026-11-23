//! Whether the assistant is awake, dozing or asleep.
//!
//! Quiet hours answer "not at this hour"; presence answers "not right now". The operator moves it
//! with `vikram daemon sleep`, `doze` or `wake`, and the state lives in `daemon_state`, so a running
//! daemon changes what it starts and what it says on its next tick without a reload.

use crate::store::{ScheduleStore, StoreError};
use crate::time::{Millis, Zone, format_ms};
use crate::triggers::URGENT_PRIORITY;

/// How live the assistant is. The set is closed on purpose: a state nobody can name is a state
/// nobody can enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Liveness {
    /// Starts work and speaks.
    #[default]
    Awake,
    /// Keeps working, but only urgent reports are delivered.
    Dozing,
    /// Starts no new work, and only urgent reports are delivered.
    Asleep,
}

impl Liveness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Awake => "awake",
            Self::Dozing => "dozing",
            Self::Asleep => "asleep",
        }
    }

    /// Read a state name, ignoring case and surrounding space. Anything else is `None`, so a
    /// damaged scalar can fall back to awake instead of inventing a fourth state.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "awake" => Some(Self::Awake),
            "dozing" => Some(Self::Dozing),
            "asleep" => Some(Self::Asleep),
            _ => None,
        }
    }
}

/// The daemon-wide liveness state, durable because it lives in `daemon_state` and auditable because
/// every change writes an audit row as well as the file log entry the CLI keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Presence {
    pub state: Liveness,
    /// When the state last changed, epoch ms. Zero when it was never written.
    pub since: Millis,
}

const PRESENCE_STATE: &str = "presence.state";
const PRESENCE_SINCE: &str = "presence.since";
const PRESENCE_REASON: &str = "presence.reason";

/// The reason recorded when the maintenance pass dozes the daemon over a full disk.
pub const DISK_REASON: &str = "disk";

impl Presence {
    /// The stored state, or awake when nothing was ever written. A value that does not parse is
    /// treated as awake too: refusing to work because a scalar was damaged is the worse failure.
    pub fn load(store: &ScheduleStore) -> Result<Self, StoreError> {
        let state = store
            .state(PRESENCE_STATE)?
            .as_deref()
            .and_then(Liveness::parse)
            .unwrap_or_default();
        let since = store.state_ms(PRESENCE_SINCE)?.unwrap_or(0);
        Ok(Self { state, since })
    }

    /// Write the state down and record the change in the audit trail.
    pub fn apply(&self, store: &ScheduleStore, now: Millis) -> Result<(), StoreError> {
        let since = if self.since == 0 { now } else { self.since };
        store.set_state(PRESENCE_STATE, self.state.as_str())?;
        store.set_state_ms(PRESENCE_SINCE, since)?;
        let described = Self {
            state: self.state,
            since,
        };
        store.audit("presence.set", "", &described.describe(), now)
    }

    /// Why the presence is where it is. `None` when the operator set it, or when nothing recorded
    /// the move: only a recorded reason lets an actuator take its own change back.
    pub fn reason(store: &ScheduleStore) -> Result<Option<String>, StoreError> {
        store.state(PRESENCE_REASON)
    }

    /// Record what moved the presence, so a later reversal cannot touch a hand-set state.
    pub fn record_reason(store: &ScheduleStore, reason: &str) -> Result<(), StoreError> {
        store.set_state(PRESENCE_REASON, reason)
    }

    /// Forget the reason, because the operator has taken over the state.
    pub fn clear_reason(store: &ScheduleStore) -> Result<(), StoreError> {
        store.delete_state(PRESENCE_REASON)
    }

    /// This presence moved to `state` at `now`. Asking for the state it is already in keeps `since`:
    /// that remains the moment the last real transition happened.
    pub fn moved(self, state: Liveness, now: Millis) -> Self {
        if self.state == state {
            self
        } else {
            Self { state, since: now }
        }
    }

    /// Whether new work may be started. Only `Asleep` stops it; `Dozing` keeps working.
    pub fn starts_work(&self) -> bool {
        self.state != Liveness::Asleep
    }

    /// Whether a report at `priority` may be delivered. Awake always may; Dozing and Asleep only
    /// let [`URGENT_PRIORITY`] through.
    pub fn speaks(&self, priority: i64) -> bool {
        match self.state {
            Liveness::Awake => true,
            Liveness::Dozing | Liveness::Asleep => priority >= URGENT_PRIORITY,
        }
    }

    /// Why a report at `priority` is being held back, or `None` when it may speak.
    pub fn silence_reason(&self, priority: i64) -> Option<&'static str> {
        (!self.speaks(priority)).then_some(self.silence())
    }

    /// The audit wording for this state, naming what it holds back.
    pub fn silence(&self) -> &'static str {
        match self.state {
            Liveness::Awake => "awake",
            Liveness::Dozing => "the assistant is dozing",
            Liveness::Asleep => "the assistant is asleep",
        }
    }

    pub fn describe(&self) -> String {
        if self.since == 0 {
            format!("{} (never changed)", self.state.as_str())
        } else {
            format!(
                "{} since {}",
                self.state.as_str(),
                format_ms(Zone::Local, self.since)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ScheduleStore {
        let store = ScheduleStore::open_in_memory().unwrap();
        store.ensure_schema().unwrap();
        store
    }

    #[test]
    fn the_default_is_awake() {
        assert_eq!(Liveness::default(), Liveness::Awake);
        let presence = Presence::default();
        assert_eq!(presence.state, Liveness::Awake);
        assert!(presence.starts_work());
        assert!(presence.speaks(0));
        assert_eq!(presence.describe(), "awake (never changed)");
    }

    #[test]
    fn a_state_round_trips_through_the_store_with_its_transition_time() {
        let store = store();
        let at = 1_700_000_000_000;
        Presence::default()
            .moved(Liveness::Asleep, at)
            .apply(&store, at)
            .unwrap();

        let loaded = Presence::load(&store).unwrap();
        assert_eq!(loaded.state, Liveness::Asleep);
        assert_eq!(loaded.since, at);
        assert_eq!(Liveness::parse("ASLEEP"), Some(Liveness::Asleep));

        let again = loaded.moved(Liveness::Asleep, at + 60_000);
        again.apply(&store, at + 60_000).unwrap();
        assert_eq!(
            Presence::load(&store).unwrap().since,
            at,
            "moving to the state it is already in is not a transition"
        );

        let trail = store.audit_trail(10).unwrap();
        assert_eq!(
            trail
                .iter()
                .filter(|row| row.kind == "presence.set")
                .count(),
            2,
            "{trail:?}"
        );
    }

    #[test]
    fn asleep_suppresses_ordinary_but_not_urgent() {
        let asleep = Presence {
            state: Liveness::Asleep,
            since: 1,
        };
        assert!(!asleep.starts_work());
        assert!(!asleep.speaks(0));
        assert!(!asleep.speaks(URGENT_PRIORITY - 1));
        assert!(asleep.speaks(URGENT_PRIORITY));
        assert_eq!(asleep.silence_reason(0), Some("the assistant is asleep"));
        assert_eq!(asleep.silence_reason(URGENT_PRIORITY), None);
    }

    #[test]
    fn dozing_holds_the_report_but_not_the_work() {
        let dozing = Presence {
            state: Liveness::Dozing,
            since: 1,
        };
        assert!(dozing.starts_work());
        assert!(!dozing.speaks(URGENT_PRIORITY - 1));
        assert!(dozing.speaks(URGENT_PRIORITY));
    }

    #[test]
    fn a_stored_value_that_does_not_parse_falls_back_to_awake() {
        let store = store();
        store.set_state(PRESENCE_STATE, "sleepwalking").unwrap();
        let loaded = Presence::load(&store).unwrap();
        assert_eq!(loaded.state, Liveness::Awake);
        assert!(loaded.starts_work());
        assert!(loaded.speaks(0));
    }
}
