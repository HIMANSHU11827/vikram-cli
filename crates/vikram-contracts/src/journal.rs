//! The session journal. This is the durable substrate: an append-only, per-session sequence of
//! records whose `seq` doubles as the replay cursor a connected client resumes from, and whose
//! optional `effect` id is the idempotency key that stops a recovered task acting twice.

use crate::error::Result;
use crate::ids::{EffectId, SessionId};
use crate::version::ContractDecl;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    /// User or host message appended to the conversation.
    Message,
    /// A tool call the model asked for.
    ToolCall,
    /// A tool result, including denied and cancelled results.
    ToolResult,
    /// A policy decision, recorded whether it allowed or not.
    Policy,
    /// An approval request and its resolution.
    Approval,
    /// Runtime status transition.
    Status,
    /// Context compaction marker: the previous prefix is summarised from here.
    Compaction,
    /// Cancellation finalized the turn.
    Cancellation,
    /// A scheduled or background activity event.
    Activity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewRecord {
    pub session: SessionId,
    pub ts_ms: u64,
    pub kind: RecordKind,
    /// Set when applying this record has an outside effect, so a replay cannot duplicate it.
    pub effect: Option<EffectId>,
    pub payload: serde_json::Value,
}

impl NewRecord {
    pub fn new(session: SessionId, kind: RecordKind, payload: serde_json::Value) -> Self {
        Self {
            session,
            ts_ms: 0,
            kind,
            effect: None,
            payload,
        }
    }

    pub fn with_effect(mut self, effect: EffectId) -> Self {
        self.effect = Some(effect);
        self
    }

    pub fn at(mut self, ts_ms: u64) -> Self {
        self.ts_ms = ts_ms;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub seq: u64,
    pub session: SessionId,
    pub ts_ms: u64,
    pub kind: RecordKind,
    pub effect: Option<EffectId>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    /// Freshly written at this seq.
    Written(u64),
    /// This effect was already recorded; the caller must not repeat the outside action.
    AlreadyApplied(u64),
}

pub trait JournalSink {
    fn contract(&self) -> ContractDecl;

    /// Must be atomic: write the record and its idempotency marker together, or neither.
    fn append(&self, record: &NewRecord) -> Result<Appended>;

    /// Records with `seq > after`, ascending, at most `limit`. The replay path for a reconnecting
    /// client and for crash recovery.
    fn read_after(
        &self,
        session: &SessionId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<JournalRecord>>;

    fn last_seq(&self, session: &SessionId) -> Result<Option<u64>>;
}

/// Volatile journal. Used by ephemeral sessions and by every test that needs journal semantics
/// without touching a disk; the durable store in `vikram-store` implements the same contract.
#[derive(Debug, Default)]
pub struct InMemoryJournal {
    inner: std::sync::Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    records: Vec<JournalRecord>,
    by_effect: std::collections::HashMap<(SessionId, EffectId), u64>,
}

impl InMemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("journal not poisoned")
            .records
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl JournalSink for InMemoryJournal {
    fn contract(&self) -> ContractDecl {
        ContractDecl::new(crate::version::JOURNAL_SINK, 1, 0)
    }

    fn append(&self, record: &NewRecord) -> Result<Appended> {
        let mut state = self.inner.lock().expect("journal not poisoned");
        if let Some(effect) = &record.effect {
            let key = (record.session.clone(), effect.clone());
            if let Some(seq) = state.by_effect.get(&key) {
                return Ok(Appended::AlreadyApplied(*seq));
            }
        }
        let next = state
            .records
            .iter()
            .filter(|r| r.session == record.session)
            .map(|r| r.seq)
            .max()
            .map(|s| s + 1)
            .unwrap_or(1);
        let stored = JournalRecord {
            seq: next,
            session: record.session.clone(),
            ts_ms: record.ts_ms,
            kind: record.kind,
            effect: record.effect.clone(),
            payload: record.payload.clone(),
        };
        if let Some(effect) = &record.effect {
            state
                .by_effect
                .insert((record.session.clone(), effect.clone()), next);
        }
        state.records.push(stored);
        Ok(Appended::Written(next))
    }

    fn read_after(
        &self,
        session: &SessionId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<JournalRecord>> {
        let state = self.inner.lock().expect("journal not poisoned");
        Ok(state
            .records
            .iter()
            .filter(|r| r.session == *session && r.seq > after)
            .take(limit)
            .cloned()
            .collect())
    }

    fn last_seq(&self, session: &SessionId) -> Result<Option<u64>> {
        let state = self.inner.lock().expect("journal not poisoned");
        Ok(state
            .records
            .iter()
            .filter(|r| r.session == *session)
            .map(|r| r.seq)
            .max())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_records_the_effect_id_used_for_deduplication() {
        let effect = EffectId::generate();
        let rec = NewRecord::new(
            SessionId::generate(),
            RecordKind::ToolResult,
            serde_json::json!({"ok": true}),
        )
        .with_effect(effect.clone())
        .at(1_700_000_000_000);
        assert_eq!(rec.effect, Some(effect));
        assert_eq!(rec.ts_ms, 1_700_000_000_000);
    }

    #[test]
    fn record_kinds_are_stable_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&RecordKind::ToolResult).unwrap(),
            "\"tool_result\""
        );
        let rec = JournalRecord {
            seq: 4,
            session: SessionId::new("ses-1"),
            ts_ms: 9,
            kind: RecordKind::Compaction,
            effect: None,
            payload: serde_json::json!(null),
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert_eq!(serde_json::from_str::<JournalRecord>(&json).unwrap(), rec);
    }
}
