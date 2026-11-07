//! Outbound delivery ledger (module 7 of the foundation).
//!
//! Port of hermes-agent `gateway/delivery_ledger.py` (plus the small state machine the delivery
//! router in `gateway/platforms/delivery.py` drives). Every outbound message that is not already
//! acknowledged by a streaming frame becomes a row here before it is sent, so a crash, a flood
//! control window or a restart resumes the send instead of losing it — and never re-sends the part
//! the platform already delivered.
//!
//! State machine:
//!
//! ```text
//!            claim_due / claim            mark_delivered
//!   Pending ───────────────────▶ Attempting ───────────────▶ Delivered
//!      ▲                            │
//!      │  mark_partial (overflow     │ mark_failed           mark_failed on the
//!      │  tail only)                ▼                       last attempt
//!      └──────────────────────── Failed ───claim──▶ …             │
//!                                     │                          ▼
//!                                     └──────────────────────▶ Abandoned
//!   Attempting ──reclaim_stale (24h)──▶ Pending | Abandoned
//! ```
//!
//! Invariants carried over from hermes that this module enforces:
//!
//! - [`MAX_ATTEMPTS`] attempts, then [`LedgerState::Abandoned`]; an abandoned row is never claimed
//!   again, so a dead chat cannot spin the sender forever (`dead_targets.py` builds on this).
//! - An `Attempting` row untouched for [`STALE_RECLAIM_AFTER_SECS`] is reclaimed: a process that
//!   died mid-send does not strand the message.
//! - `flood_not_before` is an absolute deadline taken from the adapter's server-requested delay
//!   (`flood_control:<seconds>`), so the ledger owns the wait instead of the adapter sleeping.
//! - `delivered_chunks` / `total_chunks` survive a partial send: a resumed row starts at
//!   `delivered_chunks`, so a partial overflow never re-sends the whole message.
//! - A streamed final creates no row at all: the last streamed frame is the acknowledgement.
//!
//! Storage is SQLite through `rusqlite` directly. `vikram-store` is deliberately not a dependency:
//! the gateway keeps its own `gateway_state.db` (see docs/gateway-port-plan.md) so the ledger can
//! be opened, tested and migrated without the runtime crate.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::platform::SendResult;

/// Attempts a single message may make before it is abandoned.
pub const MAX_ATTEMPTS: u32 = 3;

/// An `Attempting` row older than this is reclaimed by the next sweep (24 hours).
pub const STALE_RECLAIM_AFTER_SECS: i64 = 24 * 60 * 60;

/// Backoff used when the adapter reported a failure without a server-requested delay.
pub const DEFAULT_RETRY_BACKOFF_SECS: i64 = 5;

/// Error kinds that never heal by retrying; the row is abandoned on first sight.
pub const PERMANENT_ERROR_KINDS: &[&str] = &["forbidden", "not_found"];

/// Schema created on open. Shipped as one batch so a fresh and an existing database converge.
const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS gateway_delivery_ledger (
    ledger_id        TEXT    PRIMARY KEY,
    platform         TEXT    NOT NULL,
    chat_id          TEXT    NOT NULL,
    reply_to         TEXT,
    body             TEXT    NOT NULL,
    total_chunks     INTEGER NOT NULL,
    delivered_chunks INTEGER NOT NULL,
    attempts         INTEGER NOT NULL,
    state            TEXT    NOT NULL,
    last_error       TEXT,
    error_kind       TEXT,
    message_id       TEXT,
    flood_not_before INTEGER NOT NULL,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS gateway_delivery_ledger_due
    ON gateway_delivery_ledger (state, flood_not_before, created_at);
";

/// Column list in the order [`row_to_entry`] reads it.
const COLUMNS: &str = "ledger_id, platform, chat_id, reply_to, body, total_chunks, \
                       delivered_chunks, attempts, state, last_error, error_kind, message_id, \
                       flood_not_before, created_at, updated_at";

/// Lifecycle of one outbound message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerState {
    /// Known, not yet attempted.
    Pending,
    /// A send is in flight (or a process died while it was).
    Attempting,
    /// The platform acknowledged every chunk.
    Delivered,
    /// The attempt failed but attempts remain.
    Failed,
    /// Out of attempts, or a permanent failure: never sent again.
    Abandoned,
}

impl LedgerState {
    pub fn as_str(&self) -> &'static str {
        match self {
            LedgerState::Pending => "pending",
            LedgerState::Attempting => "attempting",
            LedgerState::Delivered => "delivered",
            LedgerState::Failed => "failed",
            LedgerState::Abandoned => "abandoned",
        }
    }

    /// Decode the stored form. Unknown values degrade to [`LedgerState::Pending`] so one bad row
    /// cannot make the whole sweep fail.
    pub fn from_stored(raw: &str) -> Self {
        match raw {
            "attempting" => LedgerState::Attempting,
            "delivered" => LedgerState::Delivered,
            "failed" => LedgerState::Failed,
            "abandoned" => LedgerState::Abandoned,
            _ => LedgerState::Pending,
        }
    }

    /// Can a row in this state be claimed for another attempt?
    pub fn is_claimable(&self) -> bool {
        matches!(self, LedgerState::Pending | LedgerState::Failed)
    }

    /// Terminal states never change again.
    pub fn is_terminal(&self) -> bool {
        matches!(self, LedgerState::Delivered | LedgerState::Abandoned)
    }
}

/// One ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub ledger_id: String,
    pub platform: String,
    pub chat_id: String,
    pub reply_to: Option<String>,
    pub body: String,
    /// Chunks the rendered body splits into.
    pub total_chunks: u32,
    /// Chunks the platform has already accepted; a resumed send starts here.
    pub delivered_chunks: u32,
    /// Attempts started so far (incremented on every claim).
    pub attempts: u32,
    pub state: LedgerState,
    pub last_error: Option<String>,
    pub error_kind: Option<String>,
    /// Id of the last chunk the platform acknowledged, when it reported one.
    pub message_id: Option<String>,
    /// Absolute unix second before which the row must not be claimed (flood control).
    pub flood_not_before: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl LedgerEntry {
    /// Chunks still owed to the platform. Zero once delivered.
    pub fn remaining_chunks(&self) -> u32 {
        self.total_chunks.saturating_sub(self.delivered_chunks)
    }

    /// Has every chunk been delivered?
    pub fn is_complete(&self) -> bool {
        self.delivered_chunks >= self.total_chunks
    }

    /// Earliest second this row may be claimed again (`None` when it never will be).
    pub fn next_attempt_at(&self) -> Option<i64> {
        if self.state.is_terminal() {
            None
        } else {
            Some(self.flood_not_before)
        }
    }
}

/// A message handed to the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDelivery {
    pub ledger_id: String,
    pub platform: String,
    pub chat_id: String,
    pub reply_to: Option<String>,
    pub body: String,
    pub total_chunks: u32,
    /// The message was already fully acknowledged by a streaming frame: no ledger obligation.
    pub streamed: bool,
}

impl NewDelivery {
    /// A normal outbound message. `ledger_id` is the caller's stable identity (hermes uses the
    /// platform message id of the turn that produced it).
    pub fn new(
        ledger_id: impl Into<String>,
        platform: impl Into<String>,
        chat_id: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            ledger_id: ledger_id.into(),
            platform: platform.into(),
            chat_id: chat_id.into(),
            reply_to: None,
            body: body.into(),
            total_chunks: 1,
            streamed: false,
        }
    }

    /// A message whose last streamed frame is the ACK: enqueueing it records nothing.
    pub fn streamed(
        ledger_id: impl Into<String>,
        platform: impl Into<String>,
        chat_id: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            streamed: true,
            ..Self::new(ledger_id, platform, chat_id, body)
        }
    }

    pub fn with_total_chunks(mut self, total_chunks: u32) -> Self {
        self.total_chunks = total_chunks.max(1);
        self
    }

    pub fn reply_to(mut self, reply_to: impl Into<String>) -> Self {
        self.reply_to = Some(reply_to.into());
        self
    }
}

/// Everything that can go wrong talking to the ledger database.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("delivery ledger database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("delivery ledger could not be opened at {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
}

/// The outbound delivery ledger.
///
/// All methods take `&self` (SQLite statements are `&self` too), so one ledger can be shared by the
/// delivery router and a background retry sweep. Times are passed in explicitly so tests are
/// deterministic; [`now_unix`] is the production clock.
#[derive(Debug)]
pub struct DeliveryLedger {
    conn: Connection,
}

impl DeliveryLedger {
    /// Open (or create) a ledger database file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DeliveryError> {
        let path = path.as_ref();
        let conn = Connection::open(path).map_err(|source| DeliveryError::Open {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_connection(conn)
    }

    /// An in-memory ledger, for tests and for a gateway running without persistence.
    pub fn in_memory() -> Result<Self, DeliveryError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, DeliveryError> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Record an outbound message as [`LedgerState::Pending`].
    ///
    /// Returns `None` for a streamed final (no obligation is created) and for a ledger id that is
    /// already known (the existing row wins, a duplicate send is never enqueued twice).
    pub fn enqueue(
        &self,
        delivery: NewDelivery,
        now: i64,
    ) -> Result<Option<LedgerEntry>, DeliveryError> {
        if delivery.streamed {
            return Ok(None);
        }
        let total_chunks = delivery.total_chunks.max(1);
        self.conn.execute(
            "INSERT INTO gateway_delivery_ledger
                 (ledger_id, platform, chat_id, reply_to, body, total_chunks, delivered_chunks,
                  attempts, state, last_error, error_kind, message_id, flood_not_before,
                  created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, 'pending', NULL, NULL, NULL, 0, ?7, ?7)
             ON CONFLICT(ledger_id) DO NOTHING",
            params![
                delivery.ledger_id,
                delivery.platform,
                delivery.chat_id,
                delivery.reply_to,
                delivery.body,
                total_chunks,
                now,
            ],
        )?;
        self.get(&delivery.ledger_id)
    }

    /// Read one row.
    pub fn get(&self, ledger_id: &str) -> Result<Option<LedgerEntry>, DeliveryError> {
        let sql = format!("SELECT {COLUMNS} FROM gateway_delivery_ledger WHERE ledger_id = ?1");
        let entry = self
            .conn
            .query_row(&sql, params![ledger_id], row_to_entry)
            .optional()?;
        Ok(entry)
    }

    /// Rows in the ledger (delivered ones included).
    pub fn len(&self) -> Result<usize, DeliveryError> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM gateway_delivery_ledger", [], |row| {
                    row.get(0)
                })?;
        Ok(count as usize)
    }

    pub fn is_empty(&self) -> Result<bool, DeliveryError> {
        Ok(self.len()? == 0)
    }

    /// Rows ready to send at `now`, oldest first.
    pub fn due(&self, now: i64, limit: usize) -> Result<Vec<LedgerEntry>, DeliveryError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM gateway_delivery_ledger
              WHERE (state = 'pending' OR state = 'failed')
                AND attempts < ?1
                AND flood_not_before <= ?2
              ORDER BY created_at ASC, rowid ASC
              LIMIT ?3"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params![MAX_ATTEMPTS, now, limit as i64], row_to_entry)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// How many rows would be claimed by [`Self::claim_due`] at `now`.
    pub fn due_count(&self, now: i64) -> Result<usize, DeliveryError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM gateway_delivery_ledger
              WHERE (state = 'pending' OR state = 'failed')
                AND attempts < ?1
                AND flood_not_before <= ?2",
            params![MAX_ATTEMPTS, now],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Claim up to `limit` due rows: `Pending | Failed -> Attempting`, one more attempt each.
    pub fn claim_due(&self, now: i64, limit: usize) -> Result<Vec<LedgerEntry>, DeliveryError> {
        let due = self.due(now, limit)?;
        let mut claimed = Vec::with_capacity(due.len());
        for entry in due {
            if let Some(entry) = self.claim(&entry.ledger_id, now)? {
                claimed.push(entry);
            }
        }
        Ok(claimed)
    }

    /// Claim one row. `None` when the id is unknown, terminal, out of attempts or still waiting out
    /// a flood-control delay.
    pub fn claim(&self, ledger_id: &str, now: i64) -> Result<Option<LedgerEntry>, DeliveryError> {
        let changed = self.conn.execute(
            "UPDATE gateway_delivery_ledger
                SET state = 'attempting',
                    attempts = attempts + 1,
                    updated_at = ?2
              WHERE ledger_id = ?1
                AND (state = 'pending' OR state = 'failed')
                AND attempts < ?3
                AND flood_not_before <= ?2",
            params![ledger_id, now, MAX_ATTEMPTS],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get(ledger_id)
    }

    /// Mark the row delivered. The whole message counts as delivered unless a chunk count says
    /// otherwise.
    pub fn mark_delivered(
        &self,
        ledger_id: &str,
        message_id: Option<&str>,
        delivered_chunks: u32,
        now: i64,
    ) -> Result<Option<LedgerEntry>, DeliveryError> {
        let changed = self.conn.execute(
            "UPDATE gateway_delivery_ledger
                SET state = 'delivered',
                    delivered_chunks = MIN(MAX(?2, 0), total_chunks),
                    message_id = COALESCE(?3, message_id),
                    last_error = NULL,
                    error_kind = NULL,
                    flood_not_before = 0,
                    updated_at = ?4
              WHERE ledger_id = ?1 AND state <> 'delivered'",
            params![ledger_id, delivered_chunks, message_id, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get(ledger_id)
    }

    /// Record a partial overflow: `delivered_chunks` of `total_chunks` reached the platform and the
    /// remainder goes back to [`LedgerState::Pending`], so the next claim resumes at the tail.
    pub fn mark_partial(
        &self,
        ledger_id: &str,
        delivered_chunks: u32,
        total_chunks: u32,
        now: i64,
    ) -> Result<Option<LedgerEntry>, DeliveryError> {
        let total_chunks = total_chunks.max(1);
        let delivered = delivered_chunks.min(total_chunks);
        if delivered >= total_chunks {
            return self.mark_delivered(ledger_id, None, total_chunks, now);
        }
        let changed = self.conn.execute(
            "UPDATE gateway_delivery_ledger
                SET state = 'pending',
                    total_chunks = ?2,
                    delivered_chunks = ?3,
                    last_error = NULL,
                    error_kind = NULL,
                    flood_not_before = 0,
                    updated_at = ?4
              WHERE ledger_id = ?1 AND state <> 'abandoned'",
            params![ledger_id, total_chunks, delivered, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get(ledger_id)
    }

    /// Record a failed attempt: `Failed` with a retry deadline, or `Abandoned` when this was the
    /// last attempt (or the failure is permanent).
    ///
    /// `retry_after_secs` is the server-requested delay (adapter `flood_control:<seconds>`); it is
    /// honoured exactly, and only counts as whole seconds rounded up. Without it the ledger uses
    /// [`DEFAULT_RETRY_BACKOFF_SECS`].
    pub fn mark_failed(
        &self,
        ledger_id: &str,
        error: impl Into<String>,
        error_kind: Option<&str>,
        retry_after_secs: Option<f64>,
        now: i64,
    ) -> Result<Option<LedgerEntry>, DeliveryError> {
        let Some(entry) = self.get(ledger_id)? else {
            return Ok(None);
        };
        if entry.state.is_terminal() {
            return Ok(None);
        }
        let error = error.into();
        let permanent = error_kind.is_some_and(|kind| PERMANENT_ERROR_KINDS.contains(&kind));
        let exhausted = entry.attempts >= MAX_ATTEMPTS;
        let state = if permanent || exhausted {
            LedgerState::Abandoned
        } else {
            LedgerState::Failed
        };
        let delay = retry_after_secs
            .filter(|secs| secs.is_finite() && *secs > 0.0)
            .map(|secs| secs.ceil() as i64)
            .unwrap_or(DEFAULT_RETRY_BACKOFF_SECS);
        let flood_not_before = if state == LedgerState::Abandoned {
            now
        } else {
            now.saturating_add(delay)
        };
        self.conn.execute(
            "UPDATE gateway_delivery_ledger
                SET state = ?2,
                    last_error = ?3,
                    error_kind = ?4,
                    flood_not_before = ?5,
                    updated_at = ?6
              WHERE ledger_id = ?1",
            params![
                ledger_id,
                state.as_str(),
                error,
                error_kind,
                flood_not_before,
                now
            ],
        )?;
        self.get(ledger_id)
    }

    /// Abandon a row without another attempt (operator action, dead target).
    pub fn abandon(
        &self,
        ledger_id: &str,
        reason: Option<&str>,
        now: i64,
    ) -> Result<Option<LedgerEntry>, DeliveryError> {
        let changed = self.conn.execute(
            "UPDATE gateway_delivery_ledger
                SET state = 'abandoned',
                    last_error = COALESCE(?2, last_error),
                    flood_not_before = 0,
                    updated_at = ?3
              WHERE ledger_id = ?1 AND state NOT IN ('delivered', 'abandoned')",
            params![ledger_id, reason, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get(ledger_id)
    }

    /// Fold an adapter's [`SendResult`] into the ledger — the single place the partial-overflow
    /// contract is interpreted.
    ///
    /// - success with `partial_overflow` → [`Self::mark_partial`] (tail only on the next attempt);
    /// - success → [`Self::mark_delivered`];
    /// - failure → [`Self::mark_failed`] with the adapter's `error_kind` and `retry_after`.
    pub fn record_send_result(
        &self,
        ledger_id: &str,
        result: &SendResult,
        now: i64,
    ) -> Result<Option<LedgerEntry>, DeliveryError> {
        let Some(entry) = self.get(ledger_id)? else {
            return Ok(None);
        };
        if result.success {
            if let Some((delivered, total)) = result.partial_overflow() {
                return self.mark_partial(ledger_id, delivered, total.max(entry.total_chunks), now);
            }
            return self.mark_delivered(
                ledger_id,
                result.message_id.as_deref(),
                entry.total_chunks,
                now,
            );
        }
        self.mark_failed(
            ledger_id,
            result
                .error
                .clone()
                .unwrap_or_else(|| "send failed".to_string()),
            result.error_kind.as_deref(),
            if result.retryable {
                result.retry_after
            } else {
                None
            },
            now,
        )
    }

    /// Recover rows a dead process left in [`LedgerState::Attempting`].
    ///
    /// Rows older than [`STALE_RECLAIM_AFTER_SECS`] go back to `Pending` (their delivered chunks
    /// intact) unless the attempt budget is already spent, in which case they are abandoned.
    pub fn reclaim_stale(&self, now: i64) -> Result<Vec<LedgerEntry>, DeliveryError> {
        let stale_before = now.saturating_sub(STALE_RECLAIM_AFTER_SECS);
        let sql = format!(
            "SELECT {COLUMNS} FROM gateway_delivery_ledger
              WHERE state = 'attempting' AND updated_at <= ?1
              ORDER BY created_at ASC, rowid ASC"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params![stale_before], row_to_entry)?;
        let mut stale = Vec::new();
        for row in rows {
            stale.push(row?);
        }
        drop(statement);

        let mut reclaimed = Vec::new();
        for entry in stale {
            let next = if entry.attempts >= MAX_ATTEMPTS {
                LedgerState::Abandoned
            } else {
                LedgerState::Pending
            };
            let changed = self.conn.execute(
                "UPDATE gateway_delivery_ledger
                    SET state = ?2, flood_not_before = 0, updated_at = ?3
                  WHERE ledger_id = ?1 AND state = 'attempting'",
                params![entry.ledger_id, next.as_str(), now],
            )?;
            if changed > 0
                && let Some(entry) = self.get(&entry.ledger_id)?
            {
                reclaimed.push(entry);
            }
        }
        Ok(reclaimed)
    }
}

/// Current wall-clock time in whole seconds since the epoch.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<LedgerEntry> {
    let state: String = row.get(8)?;
    Ok(LedgerEntry {
        ledger_id: row.get(0)?,
        platform: row.get(1)?,
        chat_id: row.get(2)?,
        reply_to: row.get(3)?,
        body: row.get(4)?,
        total_chunks: row.get::<_, i64>(5)?.max(1) as u32,
        delivered_chunks: row.get::<_, i64>(6)?.max(0) as u32,
        attempts: row.get::<_, i64>(7)?.max(0) as u32,
        state: LedgerState::from_stored(&state),
        last_error: row.get(9)?,
        error_kind: row.get(10)?,
        message_id: row.get(11)?,
        flood_not_before: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn ledger() -> DeliveryLedger {
        DeliveryLedger::in_memory().expect("in-memory ledger")
    }

    fn enqueued(ledger: &DeliveryLedger, id: &str) -> LedgerEntry {
        ledger
            .enqueue(NewDelivery::new(id, "telegram", "chat-1", "hello"), NOW)
            .expect("enqueue")
            .expect("has obligation")
    }

    #[test]
    fn enqueue_starts_pending() {
        let ledger = ledger();
        let entry = enqueued(&ledger, "m1");
        assert_eq!(entry.state, LedgerState::Pending);
        assert_eq!(entry.attempts, 0);
        assert_eq!(entry.delivered_chunks, 0);
        assert_eq!(entry.total_chunks, 1);
        assert_eq!(entry.remaining_chunks(), 1);
        assert_eq!(entry.created_at, NOW);
        assert_eq!(entry.flood_not_before, 0);
        assert_eq!(ledger.due_count(NOW).expect("due"), 1);
        assert!(!ledger.is_empty().expect("len"));
    }

    #[test]
    fn duplicate_enqueue_keeps_one_row() {
        let ledger = ledger();
        enqueued(&ledger, "m1");
        let again = ledger
            .enqueue(
                NewDelivery::new("m1", "telegram", "chat-1", "ignored"),
                NOW + 5,
            )
            .expect("enqueue again")
            .expect("entry");
        assert_eq!(again.body, "hello");
        assert_eq!(again.created_at, NOW);
        assert_eq!(ledger.len().expect("len"), 1);
    }

    #[test]
    fn pending_to_attempting_to_delivered() {
        let ledger = ledger();
        enqueued(&ledger, "m1");
        let claimed = ledger.claim("m1", NOW).expect("claim").expect("claimed");
        assert_eq!(claimed.state, LedgerState::Attempting);
        assert_eq!(claimed.attempts, 1);
        // Attempting is not claimable again until it is reclaimed or finished.
        assert!(ledger.claim("m1", NOW + 1).expect("claim").is_none());
        assert_eq!(ledger.due_count(NOW + 1).expect("due"), 0);

        let delivered = ledger
            .mark_delivered("m1", Some("platform-9"), 1, NOW + 2)
            .expect("deliver")
            .expect("entry");
        assert_eq!(delivered.state, LedgerState::Delivered);
        assert!(delivered.is_complete());
        assert_eq!(delivered.message_id.as_deref(), Some("platform-9"));
        assert!(delivered.state.is_terminal());
        assert_eq!(delivered.next_attempt_at(), None);
        // Terminal rows are never claimed and never re-marked.
        assert!(ledger.claim("m1", NOW + 3).expect("claim").is_none());
        assert!(
            ledger
                .mark_delivered("m1", None, 1, NOW + 4)
                .expect("deliver twice")
                .is_none()
        );
    }

    #[test]
    fn retries_until_max_attempts_then_abandons() {
        let ledger = ledger();
        enqueued(&ledger, "m1");
        let mut now = NOW;

        for attempt in 1..=MAX_ATTEMPTS {
            let claimed = ledger
                .claim("m1", now)
                .expect("claim")
                .unwrap_or_else(|| panic!("attempt {attempt} must be claimable"));
            assert_eq!(claimed.attempts, attempt);
            assert_eq!(claimed.state, LedgerState::Attempting);
            now += DEFAULT_RETRY_BACKOFF_SECS;
            let failed = ledger
                .mark_failed("m1", "boom", Some("transient"), None, now)
                .expect("fail")
                .expect("entry");
            if attempt < MAX_ATTEMPTS {
                assert_eq!(failed.state, LedgerState::Failed);
                assert_eq!(
                    failed.flood_not_before,
                    now + DEFAULT_RETRY_BACKOFF_SECS,
                    "the ledger owns the wait"
                );
                assert_eq!(
                    failed.next_attempt_at(),
                    Some(now + DEFAULT_RETRY_BACKOFF_SECS)
                );
            } else {
                assert_eq!(failed.state, LedgerState::Abandoned);
                assert_eq!(failed.last_error.as_deref(), Some("boom"));
            }
            now += DEFAULT_RETRY_BACKOFF_SECS;
        }

        assert!(
            ledger.claim("m1", now).expect("claim").is_none(),
            "abandoned rows are never retried"
        );
        assert_eq!(ledger.due_count(now).expect("due"), 0);
        assert_eq!(ledger.len().expect("len"), 1);
    }

    #[test]
    fn failed_row_waits_out_the_backoff() {
        let ledger = ledger();
        enqueued(&ledger, "m1");
        ledger.claim("m1", NOW).expect("claim");
        let failed = ledger
            .mark_failed("m1", "transient", Some("transient"), None, NOW)
            .expect("fail")
            .expect("entry");
        assert_eq!(failed.state, LedgerState::Failed);
        assert_eq!(failed.flood_not_before, NOW + DEFAULT_RETRY_BACKOFF_SECS);
        assert_eq!(ledger.due_count(NOW).expect("due"), 0);
        assert!(
            ledger
                .claim("m1", NOW + DEFAULT_RETRY_BACKOFF_SECS - 1)
                .expect("claim early")
                .is_none()
        );
        let claimed = ledger
            .claim("m1", NOW + DEFAULT_RETRY_BACKOFF_SECS)
            .expect("claim due")
            .expect("claimed");
        assert_eq!(claimed.attempts, 2);
    }

    #[test]
    fn server_retry_after_orders_the_next_attempt() {
        let ledger = ledger();
        enqueued(&ledger, "m1");
        ledger.claim("m1", NOW).expect("claim");
        // Telegram FloodWait: 30.4 seconds, rounded up to 31.
        let failed = ledger
            .mark_failed("m1", "flood", Some("rate_limited"), Some(30.4), NOW)
            .expect("fail")
            .expect("entry");
        assert_eq!(failed.flood_not_before, NOW + 31);
        assert_eq!(ledger.due_count(NOW + 30).expect("due"), 0);
        assert_eq!(ledger.due_count(NOW + 31).expect("due"), 1);
        let claimed = ledger.claim_due(NOW + 31, 10).expect("claim due");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].attempts, 2);
        assert_eq!(claimed[0].state, LedgerState::Attempting);
    }

    #[test]
    fn permanent_failure_is_abandoned_immediately() {
        let ledger = ledger();
        enqueued(&ledger, "m1");
        ledger.claim("m1", NOW).expect("claim");
        let failed = ledger
            .mark_failed("m1", "chat not found", Some("not_found"), None, NOW)
            .expect("fail")
            .expect("entry");
        assert_eq!(failed.state, LedgerState::Abandoned);
        assert_eq!(failed.attempts, 1);
        assert!(ledger.claim("m1", NOW + 10_000).expect("claim").is_none());
    }

    #[test]
    fn partial_overflow_resends_only_the_tail() {
        let ledger = ledger();
        ledger
            .enqueue(
                NewDelivery::new("m1", "telegram", "chat-1", "long body").with_total_chunks(4),
                NOW,
            )
            .expect("enqueue")
            .expect("entry");

        let claimed = ledger.claim("m1", NOW).expect("claim").expect("claimed");
        assert_eq!(claimed.remaining_chunks(), 4);

        // Adapter delivered 2 of 4 chunks before it overflowed the edit cap.
        let partial = SendResult::ok_split("chunk-2", vec!["chunk-1".into(), "chunk-2".into()])
            .with_partial_overflow(2, 4);
        let entry = ledger
            .record_send_result("m1", &partial, NOW + 1)
            .expect("record")
            .expect("entry");
        assert_eq!(entry.state, LedgerState::Pending);
        assert_eq!(entry.delivered_chunks, 2);
        assert_eq!(entry.total_chunks, 4);
        assert_eq!(entry.remaining_chunks(), 2, "only the tail is owed");
        assert_eq!(entry.attempts, 1, "partial progress costs no extra attempt");
        assert_eq!(entry.next_attempt_at(), Some(0));

        // The tail resend keeps the progress: no chunk is delivered twice.
        let resumed = ledger
            .claim("m1", NOW + 2)
            .expect("claim")
            .expect("claimed");
        assert_eq!(resumed.delivered_chunks, 2);
        assert_eq!(resumed.attempts, 2);

        let tail = SendResult::ok("chunk-4");
        let done = ledger
            .record_send_result("m1", &tail, NOW + 3)
            .expect("record")
            .expect("entry");
        assert_eq!(done.state, LedgerState::Delivered);
        assert!(done.is_complete());
    }

    #[test]
    fn partial_overflow_that_completes_marks_delivered() {
        let ledger = ledger();
        ledger
            .enqueue(
                NewDelivery::new("m1", "telegram", "chat-1", "body").with_total_chunks(2),
                NOW,
            )
            .expect("enqueue");
        ledger.claim("m1", NOW).expect("claim");
        let entry = ledger
            .mark_partial("m1", 2, 2, NOW + 1)
            .expect("partial")
            .expect("entry");
        assert_eq!(entry.state, LedgerState::Delivered);
        assert!(entry.is_complete());
    }

    #[test]
    fn streamed_final_creates_no_obligation() {
        let ledger = ledger();
        let created = ledger
            .enqueue(
                NewDelivery::streamed("m-stream", "telegram", "chat-1", "streamed answer"),
                NOW,
            )
            .expect("enqueue streamed");
        assert!(
            created.is_none(),
            "a streamed final is already acknowledged"
        );
        assert_eq!(ledger.len().expect("len"), 0);
        assert!(ledger.is_empty().expect("empty"));
        assert_eq!(ledger.due_count(NOW).expect("due"), 0);
        assert!(ledger.get("m-stream").expect("get").is_none());
    }

    #[test]
    fn stale_attempting_rows_are_reclaimed() {
        let ledger = ledger();
        enqueued(&ledger, "m1");
        ledger.claim("m1", NOW).expect("claim");

        // Nothing to reclaim before the 24h window elapses.
        assert!(
            ledger
                .reclaim_stale(NOW + STALE_RECLAIM_AFTER_SECS - 1)
                .expect("sweep")
                .is_empty()
        );

        let reclaimed = ledger
            .reclaim_stale(NOW + STALE_RECLAIM_AFTER_SECS)
            .expect("sweep");
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].state, LedgerState::Pending);
        assert_eq!(reclaimed[0].attempts, 1, "the attempt still counts");
        assert_eq!(reclaimed[0].flood_not_before, 0);
        // …and it is claimable again.
        let claimed = ledger
            .claim("m1", NOW + STALE_RECLAIM_AFTER_SECS + 1)
            .expect("claim")
            .expect("claimed");
        assert_eq!(claimed.attempts, 2);
    }

    #[test]
    fn stale_reclaim_abandons_when_out_of_attempts() {
        let ledger = ledger();
        enqueued(&ledger, "m1");
        // Burn the attempt budget: fail twice, then claim the last attempt and leave the row in
        // `Attempting` as a process crash mid-send would.
        let mut now = NOW;
        for attempt in 1..MAX_ATTEMPTS {
            let claimed = ledger
                .claim("m1", now)
                .expect("claim")
                .unwrap_or_else(|| panic!("attempt {attempt} must be claimable"));
            assert_eq!(claimed.attempts, attempt);
            now += 10;
            ledger
                .mark_failed("m1", "boom", Some("transient"), None, now)
                .expect("fail");
            now += 10;
        }
        let claimed = ledger
            .claim("m1", now)
            .expect("claim")
            .expect("last attempt");
        assert_eq!(claimed.attempts, MAX_ATTEMPTS);
        assert_eq!(claimed.state, LedgerState::Attempting);

        let reclaimed = ledger
            .reclaim_stale(now + STALE_RECLAIM_AFTER_SECS)
            .expect("sweep");
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].state, LedgerState::Abandoned);
        assert!(
            ledger
                .claim("m1", now + STALE_RECLAIM_AFTER_SECS + 1)
                .expect("claim")
                .is_none()
        );
    }

    #[test]
    fn unknown_ids_are_no_ops() {
        let ledger = ledger();
        assert!(ledger.get("nope").expect("get").is_none());
        assert!(ledger.claim("nope", NOW).expect("claim").is_none());
        assert!(
            ledger
                .mark_delivered("nope", None, 1, NOW)
                .expect("deliver")
                .is_none()
        );
        assert!(
            ledger
                .mark_failed("nope", "x", None, None, NOW)
                .expect("fail")
                .is_none()
        );
        assert!(
            ledger
                .mark_partial("nope", 1, 2, NOW)
                .expect("partial")
                .is_none()
        );
        assert!(
            ledger
                .abandon("nope", None, NOW)
                .expect("abandon")
                .is_none()
        );
        assert!(
            ledger
                .record_send_result("nope", &SendResult::ok("x"), NOW)
                .expect("record")
                .is_none()
        );
    }

    #[test]
    fn failed_send_result_is_folded_in() {
        let ledger = ledger();
        enqueued(&ledger, "m1");
        ledger.claim("m1", NOW).expect("claim");
        let failure = SendResult::failed("flood wait", Some("rate_limited")).retryable(Some(12.0));
        let entry = ledger
            .record_send_result("m1", &failure, NOW)
            .expect("record")
            .expect("entry");
        assert_eq!(entry.state, LedgerState::Failed);
        assert_eq!(entry.error_kind.as_deref(), Some("rate_limited"));
        assert_eq!(entry.last_error.as_deref(), Some("flood wait"));
        assert_eq!(entry.flood_not_before, NOW + 12);
    }

    #[test]
    fn operator_can_abandon_a_pending_row() {
        let ledger = ledger();
        enqueued(&ledger, "m1");
        let entry = ledger
            .abandon("m1", Some("dead chat"), NOW)
            .expect("abandon")
            .expect("entry");
        assert_eq!(entry.state, LedgerState::Abandoned);
        assert_eq!(entry.last_error.as_deref(), Some("dead chat"));
        assert!(
            ledger
                .abandon("m1", Some("again"), NOW + 1)
                .expect("abandon twice")
                .is_none()
        );
    }

    #[test]
    fn due_orders_oldest_first_and_respects_limit() {
        let ledger = ledger();
        for (idx, offset) in [30i64, 10, 20].into_iter().enumerate() {
            ledger
                .enqueue(
                    NewDelivery::new(format!("m{idx}"), "discord", "c", "body"),
                    NOW - offset,
                )
                .expect("enqueue");
        }
        let due = ledger.due(NOW, 10).expect("due");
        assert_eq!(
            due.iter()
                .map(|entry| entry.ledger_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m0", "m2", "m1"],
            "oldest created_at first"
        );
        assert_eq!(ledger.due(NOW, 2).expect("due").len(), 2);
        let claimed = ledger.claim_due(NOW, 2).expect("claim");
        assert_eq!(claimed.len(), 2);
        assert_eq!(ledger.due_count(NOW).expect("due"), 1);
    }

    #[test]
    fn state_round_trips_through_storage() {
        for state in [
            LedgerState::Pending,
            LedgerState::Attempting,
            LedgerState::Delivered,
            LedgerState::Failed,
            LedgerState::Abandoned,
        ] {
            assert_eq!(LedgerState::from_stored(state.as_str()), state);
        }
        assert_eq!(LedgerState::from_stored("garbage"), LedgerState::Pending);
        assert!(LedgerState::Pending.is_claimable());
        assert!(LedgerState::Failed.is_claimable());
        assert!(!LedgerState::Attempting.is_claimable());
        assert!(!LedgerState::Delivered.is_claimable());
        assert!(LedgerState::Delivered.is_terminal());
        assert!(LedgerState::Abandoned.is_terminal());
        assert!(!LedgerState::Failed.is_terminal());
    }

    #[test]
    fn ledger_persists_across_reopen() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vikram-gateway-ledger-{}-{}.db",
            std::process::id(),
            NOW
        ));
        let _ = std::fs::remove_file(&path);
        {
            let ledger = DeliveryLedger::open(&path).expect("open file ledger");
            enqueued(&ledger, "m1");
            ledger.claim("m1", NOW).expect("claim");
        }
        let reopened = DeliveryLedger::open(&path).expect("reopen");
        let entry = reopened.get("m1").expect("get").expect("entry");
        assert_eq!(entry.state, LedgerState::Attempting);
        assert_eq!(entry.attempts, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn now_unix_is_plausible() {
        let now = now_unix();
        assert!(now > 1_600_000_000, "clock looks wrong: {now}");
    }
}
