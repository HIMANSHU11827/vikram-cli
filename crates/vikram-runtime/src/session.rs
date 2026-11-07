//! A live session.
//!
//! The session owns the mutable runtime state for one conversation. It is `Arc`-shared between the
//! interface and the turn task, which is why switching a view can never destroy it: dropping a
//! snapshot does not drop the session.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use vikram_contracts::{AgentId, Message, ProjectId, SessionId};
use vikram_contracts::{CancellationToken, TokenMeter};
use vikram_store::{SessionRow, TITLE_MANUAL};

use crate::context_engine::{ContextAnchor, TokenEstimator};

use crate::types::{ActivityEvent, SessionSnapshot, TurnState};

/// The per-session half of the context engine: the estimator that this conversation's provider has
/// trained, and how many times the transcript has been folded.
///
/// It lives on the session and not on the runtime because a budget belongs to one conversation.
/// Two sessions sharing one estimator would let a long tool-heavy transcript distort a short chat's
/// numbers, and one session's fold must never be visible in another's context.
#[derive(Debug, Default)]
pub struct ContextState {
    pub estimator: TokenEstimator,
    pub folds: u64,
    /// Message count at the last fold. Refolding a transcript no bigger than the last one produced
    /// is the duplicate-compaction case this stops.
    pub last_fold_len: Option<usize>,
    /// Set when the provider reported usage for the most recent request, so the meter can say
    /// whether the figure shown is measured or estimated.
    pub last_reported: Option<(u64, u64)>,
    /// The measured baseline for the transcript, if the provider has ever given one.
    pub anchor: Option<ContextAnchor>,
}

#[derive(Debug)]
pub struct SessionRuntime {
    pub id: SessionId,
    pub title: Mutex<String>,
    pub cwd: Mutex<PathBuf>,
    pub model: Mutex<Option<String>>,
    pub messages: Mutex<Vec<Message>>,
    pub turn: Mutex<TurnState>,
    pub tokens: Mutex<TokenMeter>,
    /// Token accounting and compaction state for this conversation only.
    pub context: Mutex<ContextState>,
    pub cancel: Mutex<CancellationToken>,
    /// An operator-requested pause on this conversation's turns, honored between steps.
    pub paused: Mutex<bool>,
    pub queue: Mutex<VecDeque<String>>,
    /// A pending steer instruction injected at a safe point during a running turn.
    pub steer: Mutex<Option<String>>,
    pub agent: Mutex<Option<AgentId>>,
    pub created_ms: u64,
    pub updated_ms: AtomicU64,
    /// Set when a turn starts; cleared when it finishes. Used for the elapsed-time metric.
    pub turn_started_ms: Mutex<Option<u64>>,
    /// Per-session activity history (in-memory; for the main agent and session-level activities).
    pub activities: Mutex<Vec<ActivityEvent>>,
    /// The project this session is associated with, if any. Restored on session switch.
    pub project_id: Mutex<Option<ProjectId>>,
    /// `active` or `archived`.
    pub status: Mutex<String>,
    /// The session that delegated this one, if any. Delegation is a lineage, not a shared brain:
    /// the child gets a bounded brief and returns a bounded answer, and neither sees the other's
    /// full transcript.
    pub parent: Mutex<Option<SessionId>>,
    /// The session this one was forked from, when it is a fork. Like `parent`, it is set from
    /// the stored row on rehydrate; unlike `parent`, a fork *does* share its source's transcript.
    pub forked_from: Mutex<Option<SessionId>>,
    /// How far from the operator's own session this one sits. Checked against
    /// `max_delegation_depth` before a child is allowed to spawn another child.
    pub depth: AtomicU32,
    /// Who named this session: `generic`, `auto` or `manual`. A `manual` title is never overwritten
    /// by derivation, and this is the in-memory half of that rule.
    pub title_source: Mutex<String>,
    /// `pending`, `ready` or `failed`.
    pub title_status: Mutex<String>,
    /// Derivation has already been started for this session in this process. In-memory on purpose:
    /// a session that restarted without a title should still be allowed to get one.
    pub titling_started: AtomicBool,
}

impl SessionRuntime {
    pub fn from_row(row: SessionRow) -> Self {
        let forked_from = row
            .forked_from
            .clone()
            .or_else(|| row.parent_session.clone());
        Self {
            id: row.id,
            title: Mutex::new(row.title),
            cwd: Mutex::new(row.cwd),
            model: Mutex::new(row.model),
            messages: Mutex::new(Vec::new()),
            turn: Mutex::new(TurnState::Idle),
            tokens: Mutex::new(TokenMeter::default()),
            context: Mutex::new(ContextState::default()),
            cancel: Mutex::new(CancellationToken::new()),
            paused: Mutex::new(false),
            queue: Mutex::new(VecDeque::new()),
            steer: Mutex::new(None),
            agent: Mutex::new(row.agent),
            created_ms: row.created_ms,
            updated_ms: AtomicU64::new(row.updated_ms),
            turn_started_ms: Mutex::new(None),
            activities: Mutex::new(Vec::new()),
            project_id: Mutex::new(row.project_id),
            status: Mutex::new(row.status),
            parent: Mutex::new(None),
            forked_from: Mutex::new(forked_from),
            depth: AtomicU32::new(0),
            title_source: Mutex::new(row.title_source),
            title_status: Mutex::new(row.title_status),
            titling_started: AtomicBool::new(false),
        }
    }

    pub fn title_source(&self) -> String {
        self.title_source
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session title source lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn set_title_source(&self, source: &str) {
        *self.title_source.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session title source lock mutex poisoned; recovering");
            e.into_inner()
        }) = source.to_owned();
    }

    pub fn title_status(&self) -> String {
        self.title_status
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session title status lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn set_title_status(&self, status: &str) {
        *self.title_status.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session title status lock mutex poisoned; recovering");
            e.into_inner()
        }) = status.to_owned();
    }

    /// Whether a derived title may replace this session's name. A human chose a `manual` title, and
    /// that choice outranks anything a model comes up with later.
    pub fn accepts_derived_title(&self) -> bool {
        self.title_source() != TITLE_MANUAL
    }

    /// Claim the one shot this process gets at naming this session. A second caller finds the flag
    /// already set and does nothing, so messages after the first cannot re-run naming.
    pub fn claim_titling(&self) -> bool {
        !self
            .titling_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
    }

    /// Give the claim back when naming could not even start (no provider, no meaningful message),
    /// so a later message still gets a chance.
    pub fn release_titling(&self) {
        self.titling_started
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn parent(&self) -> Option<SessionId> {
        self.parent
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session parent lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn set_parent(&self, parent: Option<SessionId>) {
        *self.parent.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session parent lock mutex poisoned; recovering");
            e.into_inner()
        }) = parent;
    }

    /// The session this one was forked from, when `fork_session` created it.
    pub fn forked_from(&self) -> Option<SessionId> {
        self.forked_from
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session forked_from lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn set_forked_from(&self, forked_from: Option<SessionId>) {
        *self.forked_from.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session forked_from lock mutex poisoned; recovering");
            e.into_inner()
        }) = forked_from;
    }

    pub fn depth(&self) -> u32 {
        self.depth.load(Ordering::Relaxed)
    }

    pub fn set_depth(&self, depth: u32) {
        self.depth.store(depth, Ordering::Relaxed);
    }

    pub fn project_id(&self) -> Option<ProjectId> {
        self.project_id
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session project lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn set_cwd(&self, cwd: &Path, now_ms: u64) {
        *self.cwd.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session cwd lock mutex poisoned; recovering");
            e.into_inner()
        }) = cwd.to_path_buf();
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn set_project(&self, project: Option<ProjectId>, now_ms: u64) {
        *self.project_id.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session project lock mutex poisoned; recovering");
            e.into_inner()
        }) = project;
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn status(&self) -> String {
        self.status
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session status lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn set_status(&self, status: &str, now_ms: u64) {
        *self.status.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session status lock mutex poisoned; recovering");
            e.into_inner()
        }) = status.to_owned();
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn title(&self) -> String {
        self.title
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session title lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn set_title(&self, title: &str, now_ms: u64) {
        *self.title.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session title lock mutex poisoned; recovering");
            e.into_inner()
        }) = title.to_owned();
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session cwd lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn set_model(&self, model: Option<String>, now_ms: u64) {
        *self.model.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session model lock mutex poisoned; recovering");
            e.into_inner()
        }) = model;
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn model(&self) -> Option<String> {
        self.model
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session model lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn turn(&self) -> TurnState {
        *self.turn.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session turn lock mutex poisoned; recovering");
            e.into_inner()
        })
    }

    pub fn set_turn(&self, state: TurnState) {
        *self.turn.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session turn lock mutex poisoned; recovering");
            e.into_inner()
        }) = state;
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self.turn(),
            TurnState::Streaming | TurnState::Tooling | TurnState::WaitingApproval
        )
    }

    pub fn push_message(&self, message: Message, now_ms: u64) {
        {
            let mut context = self.context.lock().unwrap_or_else(|e| {
                eprintln!("vikram: session context lock mutex poisoned; recovering");
                e.into_inner()
            });
            context.last_fold_len = None;
        }
        self.messages
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session messages lock mutex poisoned; recovering");
                e.into_inner()
            })
            .push(message);
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn messages(&self) -> Vec<Message> {
        self.messages
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session messages lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    /// Swap the whole transcript, as a compaction does. The journal keeps every message this
    /// replaces, so a fold is a change of view rather than a deletion.
    pub fn replace_messages(&self, messages: Vec<Message>, now_ms: u64) {
        *self.messages.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session messages lock mutex poisoned; recovering");
            e.into_inner()
        }) = messages;
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Estimate the transcript with this session's calibrated estimator.
    pub fn transcript_tokens(&self) -> u64 {
        let estimator = self
            .context
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session context lock mutex poisoned; recovering");
                e.into_inner()
            })
            .estimator
            .clone();
        estimator.estimate_messages(&self.messages())
    }

    pub fn estimator(&self) -> TokenEstimator {
        self.context
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session context lock mutex poisoned; recovering");
                e.into_inner()
            })
            .estimator
            .clone()
    }

    pub fn folds(&self) -> u64 {
        self.context
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session context lock mutex poisoned; recovering");
                e.into_inner()
            })
            .folds
    }

    pub fn last_reported_usage(&self) -> Option<(u64, u64)> {
        self.context
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session context lock mutex poisoned; recovering");
                e.into_inner()
            })
            .last_reported
    }

    /// Record what the provider charged and let the estimator learn from it. The estimate used for
    /// the comparison is passed in rather than recomputed, because the transcript the request was
    /// built from is the only honest basis for the ratio.
    pub fn observe_reported_usage(&self, estimated_input: u64, input: u64, output: u64) {
        let mut context = self.context.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session context lock mutex poisoned; recovering");
            e.into_inner()
        });
        context.estimator.observe(estimated_input, input);
        context.last_reported = Some((input, output));
        // The provider just told us what the transcript up to here costs. That is the baseline the
        // next step measures against, so it is remembered with the length it describes.
        context.anchor = Some(ContextAnchor {
            input_total: input,
            messages: self
                .messages
                .lock()
                .unwrap_or_else(|e| {
                    eprintln!("vikram: session messages lock mutex poisoned; recovering");
                    e.into_inner()
                })
                .len(),
        });
    }

    pub fn anchor(&self) -> Option<ContextAnchor> {
        self.context
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session context lock mutex poisoned; recovering");
                e.into_inner()
            })
            .anchor
    }

    /// Remove the oldest droppable messages after a provider rejected the request for size.
    /// Returns how many were dropped; zero means there was nothing safe to drop.
    pub fn drop_oldest_for_limit(&self, now_ms: u64) -> usize {
        let messages = self.messages();
        let Some(offset) = crate::compaction::drop_oldest_consistent(&messages) else {
            return 0;
        };
        let kept = messages[offset..].to_vec();
        self.replace_messages(kept, now_ms);
        // The measured baseline described the transcript that no longer exists.
        self.context
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session context lock mutex poisoned; recovering");
                e.into_inner()
            })
            .anchor = None;
        offset
    }

    /// Claim a fold for this session and remember its size, which is what stops the next step from
    /// refolding a transcript that has not grown.
    pub fn note_compaction(&self, folded_len: usize) -> bool {
        let mut context = self.context.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session context lock mutex poisoned; recovering");
            e.into_inner()
        });
        if context.last_fold_len == Some(folded_len) {
            return false;
        }
        context.folds += 1;
        context.last_fold_len = Some(folded_len);
        // A fold changes what the measured baseline describes, so it goes with it.
        context.anchor = None;
        true
    }

    pub fn token_snapshot(&self) -> TokenMeter {
        self.tokens
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session tokens lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn record_usage(&self, input: Option<u64>, output: Option<u64>) {
        let mut tokens = self.tokens.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session tokens lock mutex poisoned; recovering");
            e.into_inner()
        });
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
        let mut queue = self.queue.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session queue lock mutex poisoned; recovering");
            e.into_inner()
        });
        queue.push_back(text);
        queue.len()
    }

    pub fn dequeue(&self) -> Option<String> {
        self.queue
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session queue lock mutex poisoned; recovering");
                e.into_inner()
            })
            .pop_front()
    }

    pub fn queue_len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session queue lock mutex poisoned; recovering");
                e.into_inner()
            })
            .len()
    }

    /// Set a steer instruction to be incorporated at the next safe point.
    pub fn steer(&self, text: String) {
        *self.steer.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session steer lock mutex poisoned; recovering");
            e.into_inner()
        }) = Some(text);
    }

    /// Take the pending steer instruction, if any.
    pub fn take_steer(&self) -> Option<String> {
        self.steer
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session steer lock mutex poisoned; recovering");
                e.into_inner()
            })
            .take()
    }

    pub fn has_steer(&self) -> bool {
        self.steer
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session steer lock mutex poisoned; recovering");
                e.into_inner()
            })
            .is_some()
    }

    pub fn set_agent(&self, agent: Option<AgentId>, now_ms: u64) {
        *self.agent.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session agent lock mutex poisoned; recovering");
            e.into_inner()
        }) = agent;
        self.updated_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn agent(&self) -> Option<AgentId> {
        self.agent
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session agent lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: session cancel lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    /// Requested by a HIVE team control; the turn loop honors it between steps.
    pub fn set_paused(&self, paused: bool) {
        *self.paused.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session pause lock mutex poisoned; recovering");
            e.into_inner()
        }) = paused;
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session pause lock mutex poisoned; recovering");
            e.into_inner()
        })
    }

    pub fn begin_turn(&self, now_ms: u64) {
        let mut cancel = self.cancel.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session cancel lock mutex poisoned; recovering");
            e.into_inner()
        });
        if cancel.is_cancelled() {
            *cancel = CancellationToken::new();
        }
        *self.turn_started_ms.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session started lock mutex poisoned; recovering");
            e.into_inner()
        }) = Some(now_ms);
        self.set_turn(TurnState::Streaming);
    }

    pub fn finish_turn(&self, state: TurnState, now_ms: u64) {
        self.set_turn(state);
        *self.turn_started_ms.lock().unwrap_or_else(|e| {
            eprintln!("vikram: session started lock mutex poisoned; recovering");
            e.into_inner()
        }) = None;
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
            project_id: self.project_id(),
            status: self.status(),
            title_source: self.title_source(),
            title_status: self.title_status(),
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
            status: "active".into(),
            project_id: None,
            title_source: vikram_store::TITLE_GENERIC.into(),
            title_status: vikram_store::TITLE_PENDING.into(),
            parent_session: None,
            forked_from: None,
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

    #[test]
    fn a_fork_carries_its_source_and_a_plain_session_does_not() {
        let parent = SessionId::generate();
        let fork = SessionRuntime::from_row(SessionRow {
            id: SessionId::generate(),
            title: "Fork".into(),
            cwd: "/work".into(),
            model: None,
            agent: None,
            created_ms: 1,
            updated_ms: 1,
            status: "active".into(),
            project_id: None,
            title_source: vikram_store::TITLE_MANUAL.into(),
            title_status: vikram_store::TITLE_READY.into(),
            parent_session: Some(parent.clone()),
            forked_from: Some(parent.clone()),
        });
        assert_eq!(fork.forked_from(), Some(parent.clone()));

        assert_eq!(session().forked_from(), None);
        let reparented = session();
        reparented.set_forked_from(Some(parent.clone()));
        assert_eq!(reparented.forked_from(), Some(parent));
    }
}
