//! Running the work a schedule produced, without letting it run away.
//!
//! Three things are enforced here and nowhere else: a job class has a ceiling, a task can be
//! cancelled by the name the operator knows it by, and every task ends by reporting itself. The
//! pool owns no runtime and no database — it is handed a future, and it either runs it or says it
//! has no room.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::AbortHandle;

use crate::runner::RunOutcome;
use crate::time::{self, Millis};

/// What kind of work a task is. The classes exist because an AI run and a shell check must not
/// compete for the same slot: one costs seconds, the other costs money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskClass {
    /// A cheap local check: reading a file, asking a socket whether it is up.
    Light,
    /// An approved command with no model involved.
    Script,
    /// A full agent turn.
    Agent,
    /// A long agent turn that is expected to take minutes.
    Research,
    /// A multi-step workflow.
    Workflow,
}

impl TaskClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Script => "script",
            Self::Agent => "agent",
            Self::Research => "research",
            Self::Workflow => "workflow",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "light" | "check" | "probe" => Some(Self::Light),
            "script" | "shell" | "command" | "cmd" => Some(Self::Script),
            "agent" | "ai" | "prompt" => Some(Self::Agent),
            "research" | "deep" => Some(Self::Research),
            "workflow" | "team" | "hive" => Some(Self::Workflow),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How many tasks of each class may be in flight at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub light: usize,
    pub script: usize,
    pub agent: usize,
    pub research: usize,
    pub workflow: usize,
}

impl Default for Limits {
    fn default() -> Self {
        // Deliberately small: this runs on an ordinary laptop next to the editor the operator is
        // using. Two agent turns at once is already a lot of API spend.
        Self {
            light: 8,
            script: 4,
            agent: 2,
            research: 1,
            workflow: 2,
        }
    }
}

impl Limits {
    pub fn for_class(&self, class: TaskClass) -> usize {
        match class {
            TaskClass::Light => self.light,
            TaskClass::Script => self.script,
            TaskClass::Agent => self.agent,
            TaskClass::Research => self.research,
            TaskClass::Workflow => self.workflow,
        }
    }

    /// A minimum of one slot per class: a class configured to zero would silently never run, which
    /// looks exactly like a scheduler that is broken.
    fn normalized(self) -> Self {
        Self {
            light: self.light.max(1),
            script: self.script.max(1),
            agent: self.agent.max(1),
            research: self.research.max(1),
            workflow: self.workflow.max(1),
        }
    }
}

/// A task that has ended, with what it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub id: String,
    pub class: TaskClass,
    pub outcome: RunOutcome,
    /// True when the end was a cancellation, so the run record can say `cancelled` rather than
    /// pretending the work failed on its own.
    pub cancelled: bool,
    pub started_ms: Millis,
    pub elapsed_ms: Millis,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PoolError {
    #[error("no free {0} slot; the task waits for the next tick")]
    Busy(TaskClass),
    #[error("task {0} is already running")]
    Duplicate(String),
}

struct Running {
    class: TaskClass,
    started_ms: Millis,
    abort: AbortHandle,
}

/// A bounded executor. Dispatch never blocks: a job with no free slot stays queued in the database
/// and is offered again on the next tick, which is cheaper and more honest than a growing in-memory
/// queue that dies with the process.
#[derive(Clone)]
pub struct WorkerPool {
    limits: Limits,
    slots: HashMap<TaskClass, Arc<Semaphore>>,
    running: Arc<Mutex<HashMap<String, Running>>>,
    results: UnboundedSender<Finished>,
    receiver: Arc<Mutex<Option<UnboundedReceiver<Finished>>>>,
}

impl WorkerPool {
    pub fn new(limits: Limits) -> Self {
        let limits = limits.normalized();
        let mut slots = HashMap::new();
        for class in [
            TaskClass::Light,
            TaskClass::Script,
            TaskClass::Agent,
            TaskClass::Research,
            TaskClass::Workflow,
        ] {
            slots.insert(class, Arc::new(Semaphore::new(limits.for_class(class))));
        }
        let (results, receiver) = unbounded_channel();
        Self {
            limits,
            slots,
            running: Arc::new(Mutex::new(HashMap::new())),
            results,
            receiver: Arc::new(Mutex::new(Some(receiver))),
        }
    }

    /// The one receiver of finished tasks. Taken once by whoever owns the loop.
    pub fn take_results(&self) -> Option<UnboundedReceiver<Finished>> {
        self.receiver.lock().expect("pool lock").take()
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn capacity(&self, class: TaskClass) -> usize {
        self.limits.for_class(class)
    }

    pub fn running_count(&self) -> usize {
        self.running.lock().expect("pool lock").len()
    }

    pub fn running_ids(&self) -> Vec<String> {
        let running = self.running.lock().expect("pool lock");
        let mut ids: Vec<String> = running.keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn running_of(&self, class: TaskClass) -> usize {
        self.running
            .lock()
            .expect("pool lock")
            .values()
            .filter(|task| task.class == class)
            .count()
    }

    /// Whether the pool could take another task of this class right now.
    pub fn has_room(&self, class: TaskClass) -> bool {
        self.slots
            .get(&class)
            .map(|semaphore| semaphore.available_permits() > 0)
            .unwrap_or(false)
    }

    /// Start `work` under `id`. The future is spawned, so the caller returns to its loop immediately.
    pub fn dispatch<F>(
        &self,
        id: impl Into<String>,
        class: TaskClass,
        work: F,
    ) -> Result<(), PoolError>
    where
        F: std::future::Future<Output = RunOutcome> + Send + 'static,
    {
        let id = id.into();
        {
            let running = self.running.lock().expect("pool lock");
            if running.contains_key(&id) {
                return Err(PoolError::Duplicate(id));
            }
        }
        let semaphore = self
            .slots
            .get(&class)
            .cloned()
            .unwrap_or_else(|| Arc::new(Semaphore::new(1)));
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return Err(PoolError::Busy(class)),
        };

        let started_ms = time::now_ms();
        let running = self.running.clone();
        let results = self.results.clone();
        let task_id = id.clone();

        let handle = tokio::spawn(async move {
            let outcome = work.await;
            // Hold the permit until the work is done, then drop it so the class frees a slot.
            drop(permit);
            running.lock().expect("pool lock").remove(&task_id);
            let finished = Finished {
                id: task_id.clone(),
                class,
                outcome,
                cancelled: false,
                started_ms,
                elapsed_ms: time::now_ms().saturating_sub(started_ms),
            };
            let _ = results.send(finished);
        });

        self.running.lock().expect("pool lock").insert(
            id,
            Running {
                class,
                started_ms,
                abort: handle.abort_handle(),
            },
        );
        Ok(())
    }

    /// Stop `id`, if it is running. The task being aborted cannot report itself, so the pool reports
    /// the cancellation on its behalf.
    pub fn cancel(&self, id: &str) -> bool {
        let removed = self.running.lock().expect("pool lock").remove(id);
        let Some(running) = removed else {
            return false;
        };
        running.abort.abort();
        let _ = self.results.send(Finished {
            id: id.to_string(),
            class: running.class,
            outcome: RunOutcome::failed("cancelled"),
            cancelled: true,
            started_ms: running.started_ms,
            elapsed_ms: time::now_ms().saturating_sub(running.started_ms),
        });
        true
    }

    /// Wait for one task to end, or for `wait` to pass. Used by the supervisor so it can notice a
    /// task that never ends instead of blocking on it.
    pub async fn next_finished(
        receiver: &mut UnboundedReceiver<Finished>,
        wait: Duration,
    ) -> Option<Finished> {
        tokio::time::timeout(wait, receiver.recv())
            .await
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(text: &str) -> RunOutcome {
        RunOutcome::finished(text, None)
    }

    #[tokio::test]
    async fn a_task_runs_and_reports_what_it_produced() {
        let pool = WorkerPool::new(Limits::default());
        let mut results = pool.take_results().expect("receiver");
        pool.dispatch("job-1", TaskClass::Light, async { done("all quiet") })
            .expect("dispatch");
        let finished = WorkerPool::next_finished(&mut results, Duration::from_secs(5))
            .await
            .expect("a finished task");
        assert_eq!(finished.id, "job-1");
        assert_eq!(finished.outcome.output, "all quiet");
        assert!(!finished.cancelled);
        assert_eq!(pool.running_count(), 0);
    }

    #[tokio::test]
    async fn a_class_never_runs_more_than_its_ceiling() {
        let pool = WorkerPool::new(Limits {
            agent: 1,
            ..Limits::default()
        });
        let mut results = pool.take_results().expect("receiver");
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        pool.dispatch("a", TaskClass::Agent, async move {
            let _ = wait.await;
            done("a")
        })
        .expect("first");
        assert_eq!(
            pool.dispatch("b", TaskClass::Agent, async { done("b") }),
            Err(PoolError::Busy(TaskClass::Agent)),
            "a second agent turn must wait for a slot"
        );
        // A different class has its own slots, so a blocked agent does not block a shell check.
        pool.dispatch("c", TaskClass::Script, async { done("c") })
            .expect("script");

        release.send(()).unwrap();
        let mut seen = Vec::new();
        while seen.len() < 2 {
            seen.push(
                WorkerPool::next_finished(&mut results, Duration::from_secs(5))
                    .await
                    .expect("finished")
                    .id,
            );
        }
        seen.sort();
        assert_eq!(seen, vec!["a".to_string(), "c".to_string()]);
    }

    #[tokio::test]
    async fn a_cancelled_task_is_reported_as_cancelled_not_failed() {
        let pool = WorkerPool::new(Limits::default());
        let mut results = pool.take_results().expect("receiver");
        pool.dispatch("long", TaskClass::Agent, async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            done("never")
        })
        .expect("dispatch");
        assert!(pool.cancel("long"));
        let finished = WorkerPool::next_finished(&mut results, Duration::from_secs(5))
            .await
            .expect("finished");
        assert!(finished.cancelled, "{finished:?}");
        assert_eq!(finished.outcome.summary(), "cancelled");
        assert!(!pool.cancel("long"), "cancelling twice is not an error");
    }

    #[tokio::test]
    async fn a_finished_task_gives_its_slot_back() {
        let pool = WorkerPool::new(Limits {
            script: 1,
            ..Limits::default()
        });
        let mut results = pool.take_results().expect("receiver");
        pool.dispatch("one", TaskClass::Script, async { done("one") })
            .expect("first");
        assert!(!pool.has_room(TaskClass::Script));
        WorkerPool::next_finished(&mut results, Duration::from_secs(5)).await;
        // The slot is released by the spawned task itself, so give the runtime a tick to finish the
        // bookkeeping rather than assuming it happened the instant the result arrived.
        for _ in 0..50 {
            if pool.has_room(TaskClass::Script) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pool.dispatch("two", TaskClass::Script, async { done("two") })
            .expect("the slot came back");
    }

    #[test]
    fn a_work_class_is_named_the_way_the_cli_names_it() {
        assert_eq!(TaskClass::parse("script"), Some(TaskClass::Script));
        assert_eq!(TaskClass::parse("SHELL"), Some(TaskClass::Script));
        assert_eq!(TaskClass::parse("ai"), Some(TaskClass::Agent));
        assert_eq!(TaskClass::parse("nonsense"), None);
        assert_eq!(TaskClass::Research.as_str(), "research");
    }

    #[test]
    fn a_zero_ceiling_would_look_like_a_broken_scheduler_so_it_is_raised_to_one() {
        let limits = Limits {
            light: 0,
            script: 0,
            agent: 0,
            research: 0,
            workflow: 0,
        }
        .normalized();
        assert_eq!(limits.for_class(TaskClass::Agent), 1);
    }
}
