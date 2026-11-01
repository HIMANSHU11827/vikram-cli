//! A live session.
//!
//! The session owns the mutable runtime state for one conversation. It is `Arc`-shared between the
//! interface and the turn task, which is why switching a view can never destroy it: dropping a
//! snapshot does not drop the session.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use vikram_contracts::{AgentId, Message, SessionId};
use vikram_kernel::{CancellationToken, TokenMeter};
use vikram_store::SessionRow;

use crate::types::{ActivityEvent, SessionSnapshot, TurnState};

#[derive(Debug)]
pub struct SessionRuntime {
    pub id: SessionId,
    pub title: Mutex<String>,
    pub cwd: Mutex<PathBuf>,
    pub model: Mutex<Option<String>>,
    pub messages: Mutex<Vec<Message>>,
    pub turn: Mutex<TurnState>,
    pub tokens: Mutex<TokenMeter>,
    pub cancel: Mutex<CancellationToken>,
    /// An operator-requested pause on this conversation's turns, honored between steps.
    pub paused: Mutex<bool>,
    pub queue: Mutex<VecDeque<String>>,
    pub agent: Mutex<Option<AgentId>>,
    pub created_ms: u64,
    pub updated_ms: AtomicU64,
    /// Set when a turn starts; cleared when it finishes. Used for the elapsed-time metric.
    pub turn_started_ms: Mutex<Option<u64>>,
    /// Per-session activity history (in-memory; for the main agent and session-level activities).
    pub activities: Mutex<Vec<ActivityEvent>>,
}

impl SessionRuntime {
    pub fn from_row(row: SessionRow) -> Self {
        Self {
            id: row.id,
            title: Mutex::new(row.title),
            cwd: Mutex::new(row.cwd),
            model: Mutex::new(row.model),
            messages: Mutex::new(Vec::new()),
            turn: Mutex::new(TurnState::Idle),
            tokens: Mutex::new(TokenMeter::default()),
            cancel: Mutex::new(CancellationToken::new()),
            paused: Mutex::new(false),
            queue: Mutex::new(VecDeque::new()),
            agent: Mutex::new(row.agent),
            created_ms: row.created_ms,
            updated_ms: AtomicU64::new(row.updated_ms),
            turn_started_ms: Mutex::new(None),
            activities: Mutex::new(Vec::new()),
        }
    }

    pub fn title(&self) -> String {
        self.title.lock().expect("title lock").clone()
    }

    pub fn set_title(&self, title: &str, now_ms: u64) {
        *self.title.lock().expect("title lock") = title.to_owned();
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.lock().expect("cwd lock").clone()
    }

    pub fn set_model(&self, model: Option<String>, now_ms: u64) {
        *self.model.lock().expect("model lock") = model;
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn model(&self) -> Option<String> {
        self.model.lock().expect("model lock").clone()
    }

    pub fn turn(&self) -> TurnState {
        *self.turn.lock().expect("turn lock")
    }

    pub fn set_turn(&self, state: TurnState) {
        *self.turn.lock().expect("turn lock") = state;
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self.turn(),
            TurnState::Streaming | TurnState::Tooling | TurnState::WaitingApproval
        )
    }

    pub fn push_message(&self, message: Message, now_ms: u64) {
        self.messages.lock().expect("messages lock").push(message);
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn messages(&self) -> Vec<Message> {
        self.messages.lock().expect("messages lock").clone()
    }

    pub fn token_snapshot(&self) -> TokenMeter {
        self.tokens.lock().expect("tokens lock").clone()
    }

    pub fn record_usage(&self, input: Option<u64>, output: Option<u64>) {
        let mut tokens = self.tokens.lock().expect("tokens lock");
        if let (Some(i), Some(o)) = (input, output) {
            tokens.record_reported(i, o);
        } else {
            if let Some(i) = input {
                tokens.estimated_in += i;
            }
            if let Some(o) = output {
                tokens.estimated_out += o;
            }
        }
    }

    pub fn enqueue(&self, text: String) -> usize {
        let mut queue = self.queue.lock().expect("queue lock");
        queue.push_back(text);
        queue.len()
    }

    pub fn dequeue(&self) -> Option<String> {
        self.queue.lock().expect("queue lock").pop_front()
    }

    pub fn queue_len(&self) -> usize {
        self.queue.lock().expect("queue lock").len()
    }

    pub fn set_agent(&self, agent: Option<AgentId>, now_ms: u64) {
        *self.agent.lock().expect("agent lock") = agent;
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn agent(&self) -> Option<AgentId> {
        self.agent.lock().expect("agent lock").clone()
    }

    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.lock().expect("cancel lock").clone()
    }

    /// Requested by a HIVE team control; the turn loop honors it between steps.
    pub fn set_paused(&self, paused: bool) {
        *self.paused.lock().expect("pause lock") = paused;
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.lock().expect("pause lock")
    }

    pub fn begin_turn(&self, now_ms: u64) {
        let mut cancel = self.cancel.lock().expect("cancel lock");
        if cancel.is_cancelled() {
            *cancel = CancellationToken::new();
        }
        *self.turn_started_ms.lock().expect("started lock") = Some(now_ms);
        self.set_turn(TurnState::Streaming);
    }

    pub fn finish_turn(&self, state: TurnState, now_ms: u64) {
        self.set_turn(state);
        *self.turn_started_ms.lock().expect("started lock") = None;
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn turn_elapsed_ms(&self, now_ms: u64) -> Option<u64> {
        self.turn_started_ms
            .lock()
            .expect("started lock")
            .map(|start| now_ms.saturating_sub(start))
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            id: self.id.clone(),
            title: self.title(),
            cwd: self.cwd(),
            model: self.model(),
            messages: self.messages(),
            turn: self.turn(),
            tokens: self.token_snapshot(),
            created_ms: self.created_ms,
            updated_ms: self.updated_ms.load(Ordering::Relaxed),
            agent: self.agent(),
            queued: self.queue_len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::Role;

    fn session() -> SessionRuntime {
        SessionRuntime::from_row(SessionRow {
            id: SessionId::generate(),
            title: "Session 1".into(),
            cwd: "/work".into(),
            model: None,
            agent: None,
            created_ms: 1,
            updated_ms: 1,
        })
    }

    #[test]
    fn busy_only_covers_states_that_hold_a_turn_open() {
        let s = session();
        assert!(!s.is_busy());
        s.set_turn(TurnState::Streaming);
        assert!(s.is_busy());
        s.set_turn(TurnState::Failed);
        assert!(!s.is_busy());
    }

    #[test]
    fn queue_preserves_order_and_reports_depth() {
        let s = session();
        assert_eq!(s.enqueue("a".into()), 1);
        assert_eq!(s.enqueue("b".into()), 2);
        assert_eq!(s.dequeue().as_deref(), Some("a"));
        assert_eq!(s.queue_len(), 1);
    }

    #[test]
    fn cancellation_resets_at_the_start_of_the_next_turn() {
        let s = session();
        s.cancel_handle().cancel();
        assert!(s.cancel_handle().is_cancelled());
        s.begin_turn(10);
        assert!(!s.cancel_handle().is_cancelled());
        assert_eq!(s.turn(), TurnState::Streaming);
    }

    #[test]
    fn snapshot_does_not_alias_live_message_storage() {
        let s = session();
        s.push_message(Message::user("hi"), 5);
        let snap = s.snapshot();
        s.push_message(Message::user("later"), 7);
        assert_eq!(snap.messages.len(), 1);
        assert_eq!(snap.messages[0].role, Role::User);
        assert_eq!(s.snapshot().messages.len(), 2);
    }
}
