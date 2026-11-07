//! Background activity: agents and the tasks they run.
//!
//! Both are owned by the runtime, never by a view. An interface may hide the panel or switch
//! sessions; the `CancellationToken` here is only cancelled by an explicit control action.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use vikram_contracts::CancellationToken;
use vikram_contracts::{AgentId, SessionId};
use vikram_store::AgentRow;

use crate::types::{
    ActivityEvent, AgentProfile, AgentSnapshot, AgentState, BackgroundTask, TaskState, now_ms,
};

#[derive(Debug)]
pub struct ManagedAgent {
    pub profile: AgentProfile,
    pub state: Mutex<AgentState>,
    pub current_task: Mutex<Option<String>>,
    pub progress: Mutex<Option<f32>>,
    pub session: Mutex<Option<SessionId>>,
    pub cancel: Mutex<CancellationToken>,
    /// Set by Pause and cleared by Resume; checked between steps and while streaming.
    pub paused: Mutex<bool>,
    pub unread: AtomicUsize,
    /// Per-agent activity history (in-memory; independent per agent).
    pub activities: Mutex<Vec<ActivityEvent>>,
}

impl ManagedAgent {
    pub fn from_profile(profile: AgentProfile) -> Self {
        Self {
            profile,
            state: Mutex::new(AgentState::Idle),
            current_task: Mutex::new(None),
            progress: Mutex::new(None),
            session: Mutex::new(None),
            cancel: Mutex::new(CancellationToken::new()),
            paused: Mutex::new(false),
            unread: AtomicUsize::new(0),
            activities: Mutex::new(Vec::new()),
        }
    }

    pub fn set_state(&self, state: AgentState) {
        *self.state.lock().unwrap_or_else(|e| {
            eprintln!("vikram: agent agent state mutex poisoned; recovering");
            e.into_inner()
        }) = state;
    }

    pub fn state(&self) -> AgentState {
        *self.state.lock().unwrap_or_else(|e| {
            eprintln!("vikram: agent agent state mutex poisoned; recovering");
            e.into_inner()
        })
    }

    pub fn set_task(&self, task: Option<String>) {
        *self.current_task.lock().unwrap_or_else(|e| {
            eprintln!("vikram: agent task lock mutex poisoned; recovering");
            e.into_inner()
        }) = task;
    }

    pub fn set_session(&self, session: Option<SessionId>) {
        *self.session.lock().unwrap_or_else(|e| {
            eprintln!("vikram: agent session lock mutex poisoned; recovering");
            e.into_inner()
        }) = session;
    }

    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: agent agent cancel mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.lock().unwrap_or_else(|e| {
            eprintln!("vikram: agent pause lock mutex poisoned; recovering");
            e.into_inner()
        })
    }

    pub fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            profile: self.profile.clone(),
            state: self.state(),
            current_task: self
                .current_task
                .lock()
                .unwrap_or_else(|e| {
                    eprintln!("vikram: agent task lock mutex poisoned; recovering");
                    e.into_inner()
                })
                .clone(),
            progress: *self.progress.lock().unwrap_or_else(|e| {
                eprintln!("vikram: agent progress lock mutex poisoned; recovering");
                e.into_inner()
            }),
            session: self
                .session
                .lock()
                .unwrap_or_else(|e| {
                    eprintln!("vikram: agent session lock mutex poisoned; recovering");
                    e.into_inner()
                })
                .clone(),
            unread: self.unread.load(Ordering::Relaxed),
        }
    }

    pub fn push_activity(&self, event: ActivityEvent) {
        let mut acts = self.activities.lock().unwrap_or_else(|e| {
            eprintln!("vikram: agent activities lock mutex poisoned; recovering");
            e.into_inner()
        });
        acts.push(event);
        const CAP: usize = 500;
        if acts.len() > CAP {
            let drop = acts.len() - CAP;
            acts.drain(0..drop);
        }
    }

    pub fn activities(&self) -> Vec<ActivityEvent> {
        self.activities
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: agent activities lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }
}

#[derive(Debug, Default)]
pub struct AgentManager {
    agents: Mutex<Vec<Arc<ManagedAgent>>>,
}

impl AgentManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&self, agents: Vec<Arc<ManagedAgent>>) {
        *self.agents.lock().unwrap_or_else(|e| {
            eprintln!("vikram: agent agents lock mutex poisoned; recovering");
            e.into_inner()
        }) = agents;
    }

    pub fn list(&self) -> Vec<Arc<ManagedAgent>> {
        self.agents
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: agent agents lock mutex poisoned; recovering");
                e.into_inner()
            })
            .clone()
    }

    pub fn snapshots(&self) -> Vec<AgentSnapshot> {
        self.list().iter().map(|a| a.snapshot()).collect()
    }

    pub fn get(&self, id: &AgentId) -> Option<Arc<ManagedAgent>> {
        self.list().into_iter().find(|a| a.profile.id == *id)
    }

    pub fn add(&self, agent: Arc<ManagedAgent>) {
        self.agents
            .lock()
            .unwrap_or_else(|e| {
                eprintln!("vikram: agent agents lock mutex poisoned; recovering");
                e.into_inner()
            })
            .push(agent);
    }

    pub fn remove(&self, id: &AgentId) -> bool {
        let mut agents = self.agents.lock().unwrap_or_else(|e| {
            eprintln!("vikram: agent agents lock mutex poisoned; recovering");
            e.into_inner()
        });
        let before = agents.len();
        agents.retain(|a| a.profile.id != *id);
        agents.len() != before
    }

    pub fn active_count(&self) -> usize {
        self.list().iter().filter(|a| a.state().is_active()).count()
    }
}

impl From<AgentRow> for AgentProfile {
    fn from(row: AgentRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            role: row.role,
            description: row.description,
            icon: row.icon,
            created_ms: row.created_ms,
        }
    }
}

/// A running background unit of work. Tasks are registered when an agent starts and updated when
/// it finishes, so the status bar reports what actually exists.
#[derive(Debug, Default)]
pub struct TaskManager {
    tasks: Mutex<HashMap<String, BackgroundTask>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(&self, name: impl Into<String>, session: SessionId) -> BackgroundTask {
        let task = BackgroundTask {
            id: format!("task-{}", now_ms()),
            name: name.into(),
            session,
            state: TaskState::Running,
            started_ms: now_ms(),
        };
        self.tasks
            .lock()
            .expect("tasks lock")
            .insert(task.id.clone(), task.clone());
        task
    }

    pub fn finish(&self, id: &str, state: TaskState) -> Option<BackgroundTask> {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| {
            eprintln!("vikram: agent tasks lock mutex poisoned; recovering");
            e.into_inner()
        });
        let task = tasks.get_mut(id)?;
        task.state = state;
        Some(task.clone())
    }

    pub fn list(&self) -> Vec<BackgroundTask> {
        let mut tasks: Vec<_> = self
            .tasks
            .lock()
            .expect("tasks lock")
            .values()
            .cloned()
            .collect();
        tasks.sort_by_key(|t| t.started_ms);
        tasks
    }

    pub fn running_count(&self) -> usize {
        self.list()
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_start_idle_and_report_their_own_state() {
        let agent = ManagedAgent::from_profile(AgentProfile {
            id: AgentId::generate(),
            name: "Rudra".into(),
            role: "Implementer".into(),
            description: String::new(),
            icon: None,
            created_ms: 0,
        });
        assert_eq!(agent.state(), AgentState::Idle);
        agent.set_state(AgentState::Running);
        assert_eq!(agent.snapshot().state, AgentState::Running);
    }

    #[test]
    fn progress_is_only_present_when_the_runtime_sets_it() {
        let agent = ManagedAgent::from_profile(AgentProfile {
            id: AgentId::generate(),
            name: "Karma".into(),
            role: "Reviewer".into(),
            description: String::new(),
            icon: None,
            created_ms: 0,
        });
        assert!(agent.snapshot().progress.is_none());
        *agent.progress.lock().unwrap() = Some(0.42);
        assert_eq!(agent.snapshot().progress, Some(0.42));
    }

    #[test]
    fn tasks_report_running_then_terminal_state() {
        let manager = TaskManager::new();
        let task = manager.start("agent: Rudra", SessionId::generate());
        assert_eq!(manager.running_count(), 1);
        manager.finish(&task.id, TaskState::Completed);
        assert_eq!(manager.running_count(), 0);
        assert_eq!(manager.list()[0].state, TaskState::Completed);
    }
}
