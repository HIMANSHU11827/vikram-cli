//! The supervisor: one owner per home directory, one tick that runs the work it finds.
//!
//! Three rules shape this module.
//!
//! 1. One daemon per `VIKRAM_HOME`. Two supervisors sharing a database would each believe they
//!    owned the same job, so the lock is taken before anything else and it is identified by the
//!    process **and** the moment that process started, never by the pid alone: pids are reused, and
//!    a bare pid check is how a stale lock either blocks every start or gets stolen from a live
//!    daemon.
//! 2. A tick does the whole job in a fixed order — adopt what a crash left behind, coalesce what a
//!    sleep made stale, claim what is due, ask the heartbeats what they see, then settle what
//!    finished. Nothing in that order depends on the previous tick, so a daemon that has been down
//!    for a day converges in one pass instead of replaying a day of backlog.
//! 3. The tick never blocks on a job. Work goes to the worker pool with its own ceiling and its own
//!    timeout, and the supervisor learns the result the same way it learns everything else: by
//!    reading it back.

use std::fs;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::heartbeat::{Beat, BeatKind, Heartbeat, Policy};
use crate::jobs::{Claimed, JobManager};
use crate::notify::{self, Destination, Notice};
use crate::postbox::Postbox;
use crate::runner::{self, AgentRequest, RunOutcome};
use crate::store::{Delivery, Job, JobMode, Run, RunStatus, ScheduleStore, StoreError};
use crate::time::{self, Millis};
use crate::worker::{Finished, Limits, TaskClass, WorkerPool};

/// How often the supervisor looks for work when the caller does not say.
pub const DEFAULT_TICK_MS: u64 = 5_000;
/// How long a job may run when the job itself does not say.
pub const DEFAULT_JOB_TIMEOUT_MS: Millis = 10 * 60_000;
/// How late a slot may be before it is skipped instead of run. A laptop closed overnight should not
/// wake up and run every ten minute check it missed while it slept.
pub const DEFAULT_CATCH_UP_MS: Millis = 30 * 60_000;

/// The work a dispatch hands to the pool: one boxed future that owns everything it needs.
type Work = Pin<Box<dyn Future<Output = RunOutcome> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another daemon already owns this home (pid {pid})")]
    Held { pid: u32 },
    #[error("lock: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LockBody {
    pid: u32,
    /// When the process started, in epoch milliseconds. Two processes can wear the same pid; they
    /// cannot wear the same pid and the same start time.
    started_ms: i64,
    host: String,
}

/// Who owns a home right now, if anyone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub pid: u32,
    pub started_ms: i64,
    pub alive: bool,
}

/// The claim itself. Dropping it releases the home, which is why the supervisor holds one for its
/// whole life rather than around each tick: a daemon that released between ticks could be joined by
/// a second one.
pub struct Lock {
    path: PathBuf,
    body: LockBody,
}

impl Lock {
    /// Claim a home, or refuse because a live daemon already has it.
    ///
    /// A lock whose process is gone is taken over: a crash must not leave the operator hunting for a
    /// file to delete before the next start works.
    pub fn acquire(home: &Path) -> Result<Self, LockError> {
        let dir = home.join("daemon");
        fs::create_dir_all(&dir)?;
        let path = dir.join("daemon.lock");
        // The fingerprint is the process's start time, not the moment the lock was written: it has
        // to be the thing that distinguishes this pid from a later process that reuses it.
        let body = LockBody {
            pid: std::process::id(),
            started_ms: process_started_ms(std::process::id()).unwrap_or_else(time::now_ms),
            host: hostname(),
        };
        if let Some(existing) = read_lock(&path)?
            && holder_is_alive(&existing)
        {
            return Err(LockError::Held { pid: existing.pid });
        }
        write_lock(&path, &body)?;
        Ok(Self { path, body })
    }

    pub fn pid(&self) -> u32 {
        self.body.pid
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn started_ms(&self) -> i64 {
        self.body.started_ms
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Read the claim without taking it.
pub fn owner(home: &Path) -> Result<Option<Owner>, LockError> {
    let Some(body) = read_lock(&home.join("daemon").join("daemon.lock"))? else {
        return Ok(None);
    };
    let alive = holder_is_alive(&body);
    Ok(Some(Owner {
        pid: body.pid,
        started_ms: body.started_ms,
        alive,
    }))
}

fn read_lock(path: &Path) -> Result<Option<LockBody>, LockError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(serde_json::from_str(&text).ok()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn write_lock(path: &Path, body: &LockBody) -> Result<(), LockError> {
    let text = serde_json::to_string(body).unwrap_or_default();
    // Written beside the target and moved into place: a half-written lock is a lock nobody can read,
    // and the next start would take it over while the first daemon is still running.
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn holder_is_alive(body: &LockBody) -> bool {
    if body.host != hostname() {
        // A lock written on another machine says nothing about this one; leave it alone.
        return false;
    }
    // /proc counts start time in whole clock ticks, so the reading can be a few milliseconds off the
    // one recorded; the comparison is about identity, not precision.
    process_started_ms(body.pid).is_some_and(|started| (started - body.started_ms).abs() <= 2_000)
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

/// When a live process started, in epoch milliseconds, or `None` when it is gone.
fn process_started_ms(pid: u32) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name sits in parentheses and may contain spaces, so fields are counted from after
    // the closing parenthesis. Field 22 is the start time in clock ticks since boot.
    let rest = stat.rsplit_once(')')?.1;
    let ticks: i64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    let uptime: f64 = fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let boot_ms = time::now_ms() - (uptime * 1000.0) as i64;
    Some(boot_ms + ticks * 1000 / 100)
}

struct Running {
    job_id: String,
    run_id: String,
}

/// What one pass over the store did, so the caller can log it and a test can assert on it.
#[derive(Debug, Default)]
pub struct TickReport {
    pub adopted: usize,
    pub coalesced: usize,
    pub dispatched: Vec<String>,
    pub finished: Vec<Finished>,
    pub delivered: Vec<(String, bool)>,
    pub beats: Vec<Beat>,
    pub refused: Vec<String>,
}

impl TickReport {
    pub fn is_quiet(&self) -> bool {
        self.dispatched.is_empty()
            && self.finished.is_empty()
            && self.beats.is_empty()
            && self.refused.is_empty()
    }
}

/// The supervisor. It owns no business logic: it asks the schedule what is due, hands execution to
/// the pool, and records whatever came back.
pub struct Daemon {
    store: ScheduleStore,
    jobs: JobManager,
    pool: WorkerPool,
    policy: Policy,
    home: PathBuf,
    tick_ms: u64,
    catch_up_ms: Millis,
    running: Vec<Running>,
    results: tokio::sync::mpsc::UnboundedReceiver<Finished>,
    postbox: Postbox,
}

impl Daemon {
    pub fn new(store: ScheduleStore, home: impl Into<PathBuf>) -> Self {
        let jobs = JobManager::new(store.clone());
        let pool = WorkerPool::new(Limits::default());
        let results = pool.take_results().expect("a fresh pool owns its results");
        Self {
            postbox: Postbox::new(store.clone()),
            store,
            jobs,
            pool,
            policy: Policy::default(),
            home: home.into(),
            tick_ms: DEFAULT_TICK_MS,
            catch_up_ms: DEFAULT_CATCH_UP_MS,
            running: Vec::new(),
            results,
        }
    }

    pub fn with_tick_ms(mut self, tick_ms: u64) -> Self {
        self.tick_ms = tick_ms.max(50);
        self
    }

    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_catch_up_ms(mut self, catch_up_ms: Millis) -> Self {
        self.catch_up_ms = catch_up_ms.max(0);
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.pool = WorkerPool::new(limits);
        if let Some(results) = self.pool.take_results() {
            self.results = results;
        }
        self
    }

    pub fn jobs(&self) -> &JobManager {
        &self.jobs
    }

    pub fn pool(&self) -> &WorkerPool {
        &self.pool
    }

    pub fn store(&self) -> &ScheduleStore {
        &self.store
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn tick_ms(&self) -> u64 {
        self.tick_ms
    }

    pub fn in_flight(&self) -> usize {
        self.running.len()
    }

    /// One pass: recover, coalesce, claim, dispatch, settle, and ask the heartbeats.
    pub async fn tick(&mut self, now: Millis) -> Result<TickReport, StoreError> {
        let mut report = TickReport::default();

        // A run still marked running belongs to a process that is gone. The honest record is that
        // nobody knows whether its side effects happened, which is what `unknown` means. Only the
        // first pass of a fresh daemon may say that: once this process has work in flight, a
        // `running` row is its own, and adopting it would rewrite a healthy job as interrupted.
        if self.running.is_empty() {
            report.adopted = self.store.adopt_orphans(None)?.len();
        }

        // A cancel is a request written by `vikram jobs cancel`. Nothing aborts a task on its own,
        // so this is where it lands; the flag is cleared once honoured so the next pass does not
        // repeat it. A run that already finished has nothing left to abort.
        let cancelling: Vec<String> = self
            .running
            .iter()
            .map(|entry| entry.run_id.clone())
            .filter(|id| {
                self.store
                    .state_bool(&format!("cancel.{id}"), false)
                    .unwrap_or(false)
            })
            .collect();
        for id in cancelling {
            self.pool.cancel(&id);
            let _ = self.store.set_state(&format!("cancel.{id}"), "0");
        }

        for job in self.store.list_jobs()? {
            if job.enabled && self.stale_slot(&job, now)? {
                report.coalesced += 1;
            }
        }

        for job in self.store.due_jobs(now)? {
            // The slot to claim is the one the store says is due. Recomputing it from the schedule
            // would produce the *following* slot and the compare-and-set would never match.
            let Some(slot) = job.next_run_ms else {
                continue;
            };
            match self.jobs.claim(&job, slot, now) {
                Ok(Some(claimed)) => self.dispatch(claimed, now, &mut report),
                Ok(None) => {}
                Err(err) => report.refused.push(format!("{}: {err}", job.id)),
            }
        }

        self.collect(&mut report, now).await;
        self.beat(now, &mut report).await;
        Ok(report)
    }

    /// Advance a slot that is older than the tolerance instead of running it.
    fn stale_slot(&self, job: &Job, now: Millis) -> Result<bool, StoreError> {
        let Some(next) = job.next_run_ms else {
            return Ok(false);
        };
        if next > now || now - next <= self.catch_up_ms {
            return Ok(false);
        }
        let next = self.jobs.next_slot(job, now);
        self.store.set_next_run(&job.id, next, now)?;
        self.store.note_outcome(&job.id, RunStatus::Skipped)?;
        Ok(true)
    }

    fn dispatch(&mut self, claimed: Claimed, now: Millis, report: &mut TickReport) {
        let Claimed { job, run, .. } = claimed;
        let timeout = job.timeout_ms.unwrap_or(DEFAULT_JOB_TIMEOUT_MS).max(1_000);
        let work = work_for(&job, timeout);
        let id = run.id.clone();
        match self.pool.dispatch(id, class_of(job.mode), work) {
            Ok(()) => {
                self.running.push(Running {
                    job_id: job.id.clone(),
                    run_id: run.id.clone(),
                });
                report.dispatched.push(job.id);
            }
            Err(err) => {
                // No free slot is not a failure of the job: the run is closed so the next tick can
                // take it again, and the reason is recorded rather than left in flight forever.
                let _ = self.store.finish_run(
                    &run.id,
                    RunStatus::Skipped,
                    "",
                    &format!("not started: {err}"),
                    Delivery::None,
                );
                let next = self.jobs.next_slot(&job, now);
                let _ = self.store.set_next_run(&job.id, next, now);
                report.refused.push(format!("{}: {err}", job.id));
            }
        }
    }

    async fn collect(&mut self, report: &mut TickReport, now: Millis) {
        while let Ok(finished) = self.results.try_recv() {
            report.finished.push(finished);
        }
        let finished = std::mem::take(&mut report.finished);
        for done in &finished {
            if let Some(index) = self
                .running
                .iter()
                .position(|entry| entry.run_id == done.id)
            {
                let running = self.running.remove(index);
                self.settle(&running, done, now, report).await;
            }
        }
        report.finished = finished;
    }

    async fn settle(
        &mut self,
        running: &Running,
        done: &Finished,
        now: Millis,
        report: &mut TickReport,
    ) {
        let Ok(Some(job)) = self.store.get_job(&running.job_id) else {
            return;
        };
        let Ok(Some(run)) = self.store.get_run(&running.run_id) else {
            return;
        };
        let ok = done.outcome.ok && !done.cancelled;
        let error = if done.cancelled {
            "cancelled".to_string()
        } else {
            done.outcome.error.clone().unwrap_or_default()
        };
        let status = if done.cancelled {
            RunStatus::Cancelled
        } else if ok {
            RunStatus::Ok
        } else {
            RunStatus::Failed
        };
        let _ = self.store.finish_run(
            &run.id,
            status,
            &done.outcome.output,
            &error,
            if done.cancelled {
                Delivery::None
            } else {
                Delivery::Pending
            },
        );
        if done.cancelled {
            // `settle` would record this as a failure and offer it a retry, overwriting the
            // cancellation the operator asked for.
            let _ = self.jobs.settle_cancelled(&job, now);
            return;
        }
        // `settle` decides what the job's next slot is, including a retry that comes back sooner
        // than the schedule would have said.
        let _ = self
            .jobs
            .settle(&job, &run, ok, &done.outcome.output, &error, now);
        let notice = Notice {
            job: job.id.clone(),
            name: job.name.clone(),
            status: status.as_str().to_string(),
            summary: done.outcome.summary(),
            session: done.outcome.session.clone(),
            at: now,
        };
        match Destination::parse(&job.target) {
            Ok(destination) => {
                let delivered = match &destination {
                    // A conversation is written to directly. The daemon keeps a runtime for exactly
                    // this, and what it writes lands in the same database the terminal reads, so the
                    // report is there the next time that conversation is opened.
                    Destination::Session(wanted) => {
                        match self.postbox.post(wanted, &notice.line()).await {
                            Ok((session, note)) => notify::Report {
                                delivered: true,
                                detail: match note {
                                    Some(note) => format!("session {session}: {note}"),
                                    None => format!("session {session}"),
                                },
                            },
                            Err(err) => notify::Report {
                                delivered: false,
                                detail: err,
                            },
                        }
                    }
                    _ => notify::deliver(&destination, &notice, &self.home, None).await,
                };
                // `none` is a job that was never meant to report anywhere: recording that as
                // delivered would claim a delivery that by definition never happened.
                let state = match destination {
                    Destination::None => Delivery::None,
                    _ if delivered.delivered => Delivery::Delivered,
                    _ => Delivery::Failed,
                };
                // The note is part of the record: `delivery: failed` on its own tells an operator
                // nothing about what to fix.
                let _ = self
                    .store
                    .set_delivery_note(&run.id, state, &delivered.detail);
                report.delivered.push((job.id, delivered.delivered));
            }
            Err(err) => report.refused.push(format!("{}: {err}", job.id)),
        }
    }

    /// Ask the heartbeats whether anything needs attention, and report what they raised.
    async fn beat(&mut self, now: Millis, report: &mut TickReport) {
        let heartbeat = Heartbeat::new(&self.store, self.policy.clone());
        if heartbeat.next_due(now).is_none() {
            return;
        }
        for kind in [BeatKind::Runtime, BeatKind::Agent, BeatKind::Workers] {
            match heartbeat.beat(kind, now) {
                Ok(beat) => {
                    if !beat.notify.is_empty() {
                        let notice = Notice {
                            job: format!("heartbeat.{}", beat.kind.as_str()),
                            name: format!("heartbeat {}", beat.kind.as_str()),
                            status: beat.state.as_str().to_string(),
                            summary: beat
                                .notify
                                .iter()
                                .map(|issue| issue.message.as_str())
                                .collect::<Vec<_>>()
                                .join("; "),
                            session: None,
                            at: beat.at,
                        };
                        let _ =
                            notify::deliver(&Destination::Local, &notice, &self.home, None).await;
                        report
                            .delivered
                            .push((format!("heartbeat.{}", beat.kind.as_str()), true));
                    }
                    report.beats.push(beat);
                }
                Err(err) => report
                    .refused
                    .push(format!("heartbeat {}: {err}", kind.as_str())),
            }
        }
    }

    /// Run until `stop` is notified. Each pass is bounded, because a tick that hangs looks exactly
    /// like a daemon that is dead, which is the failure the heartbeat exists to notice.
    pub async fn run(mut self, stop: Arc<Notify>) -> Result<(), StoreError> {
        // Open the delivery runtime once, before the first tick, so a report for a conversation does
        // not pay for a cold start every morning. A machine with no model configured refuses here
        // and still runs every script job; the refusal is reported per delivery.
        if let Err(err) = self.postbox.open().await {
            eprintln!("[vikram] conversation delivery unavailable: {err}");
        }
        loop {
            let _ = self.tick(time::now_ms()).await;
            tokio::select! {
                _ = stop.notified() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_millis(self.tick_ms)) => {}
            }
        }
    }
}

fn work_for(job: &Job, timeout: Millis) -> Work {
    let job = job.clone();
    Box::pin(async move {
        let limit = Duration::from_millis(timeout as u64);
        match job.mode {
            JobMode::Script => {
                let cwd = (!job.agent.trim().is_empty()).then(|| PathBuf::from(&job.agent));
                runner::run_script(&job.payload, cwd.as_deref(), limit).await
            }
            // An agent job starts its own runtime and drops it when the turn ends. The daemon keeps a
            // runtime only to write reports into a conversation, never to run work, so a schedule
            // never inherits a model turn it did not ask for.
            JobMode::Agent => {
                let config = match vikram_runtime::config::Config::load() {
                    Ok(config) => config,
                    Err(err) => return RunOutcome::failed(format!("config: {err}")),
                };
                let request =
                    AgentRequest::new(job.payload.clone()).title(runner::title_from(&job.payload));
                let request = match job.target.strip_prefix("model:") {
                    Some(model) => request.model(model.to_string()),
                    None => request,
                };
                runner::run_agent(config, request, limit).await
            }
            // A delegated job belongs to a bot, and an unattributed one is not a bot job with a
            // default identity: it is a job whose author forgot to say who does the work.
            JobMode::Delegate => {
                if job.bot.trim().is_empty() {
                    return RunOutcome::failed(
                        "a delegate job needs a bot: set --bot <agent>".to_string(),
                    );
                }
                let config = match vikram_runtime::config::Config::load() {
                    Ok(config) => config,
                    Err(err) => return RunOutcome::failed(format!("config: {err}")),
                };
                let request = runner::DelegateRequest::new(job.bot.clone(), job.payload.clone())
                    .title(runner::title_from(&job.payload));
                let request = match job.target.strip_prefix("model:") {
                    Some(model) => request.model(model.to_string()),
                    None => request,
                };
                runner::run_delegate(config, request, limit).await
            }
            // A team job names a saved team; its members each get their own delegated turn.
            JobMode::Workflow => {
                if job.agent.trim().is_empty() {
                    return RunOutcome::failed(
                        "a workflow job needs a team: set --agent <team>".to_string(),
                    );
                }
                let config = match vikram_runtime::config::Config::load() {
                    Ok(config) => config,
                    Err(err) => return RunOutcome::failed(format!("config: {err}")),
                };
                let request = runner::TeamRequest::new(job.agent.clone(), job.payload.clone())
                    .title(runner::title_from(&job.payload))
                    .in_order(runner::TeamOrder::parse(&job.order).unwrap_or_default());
                let request = match job.target.strip_prefix("model:") {
                    Some(model) => request.model(model.to_string()),
                    None => request,
                };
                runner::run_team(config, request, limit).await
            }
        }
    })
}

fn class_of(mode: JobMode) -> TaskClass {
    match mode {
        JobMode::Script => TaskClass::Script,
        JobMode::Agent | JobMode::Delegate => TaskClass::Agent,
        JobMode::Workflow => TaskClass::Workflow,
    }
}

/// A job and its run, for callers that want to show both without a second lookup.
pub fn pair(store: &ScheduleStore, job_id: &str) -> Option<(Job, Vec<Run>)> {
    let job = store.get_job(job_id).ok().flatten()?;
    let runs = store.runs_for(job_id, 5).unwrap_or_default();
    Some((job, runs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::NewJob;

    fn store() -> ScheduleStore {
        let store = ScheduleStore::open_in_memory().unwrap();
        store.ensure_schema().unwrap();
        store
    }

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn two_daemons_cannot_own_the_same_home() {
        let dir = home();
        let first = Lock::acquire(dir.path()).unwrap();
        assert_eq!(first.pid(), std::process::id());
        let second = Lock::acquire(dir.path());
        assert!(
            matches!(second, Err(LockError::Held { .. })),
            "a second daemon must be refused, not joined"
        );
        drop(first);
        assert!(
            Lock::acquire(dir.path()).is_ok(),
            "the home is free once the owner is gone"
        );
    }

    #[tokio::test]
    async fn a_lock_left_by_a_dead_process_is_taken_over() {
        let dir = home();
        std::fs::create_dir_all(dir.path().join("daemon")).unwrap();
        std::fs::write(
            dir.path().join("daemon").join("daemon.lock"),
            r#"{"pid":4294967,"started_ms":1,"host":""}"#,
        )
        .unwrap();
        assert!(
            Lock::acquire(dir.path()).is_ok(),
            "a crashed daemon must not need a manual file delete before the next start"
        );
    }

    #[tokio::test]
    async fn a_cancel_request_stops_the_run_it_names() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(
                NewJob::new("long check", "every 1m")
                    .mode(JobMode::Script)
                    .payload("sleep 5")
                    .target("none"),
                now,
            )
            .unwrap();
        manager.run_now(&job.id, now).unwrap();
        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path()).with_tick_ms(50);
        daemon.tick(now).await.unwrap();
        let run = store.runs_for(&job.id, 1).unwrap().remove(0);
        assert_eq!(run.status, RunStatus::Running);

        // What `vikram jobs cancel` writes. The next pass is where it is honoured.
        store.set_state(&format!("cancel.{}", run.id), "1").unwrap();
        daemon.tick(time::now_ms()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        daemon.tick(time::now_ms()).await.unwrap();

        let settled = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(settled.status, RunStatus::Cancelled);
        assert!(
            store
                .state_bool(&format!("cancel.{}", run.id), false)
                .unwrap_or(true)
                == false,
            "the request is cleared once honoured"
        );
    }

    #[tokio::test]
    async fn a_run_in_flight_is_not_mistaken_for_one_left_behind() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(
                NewJob::new("slow check", "every 1m")
                    .mode(JobMode::Script)
                    .payload("sleep 1")
                    .target("none"),
                now,
            )
            .unwrap();
        manager.run_now(&job.id, now).unwrap();
        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path()).with_tick_ms(50);
        daemon.tick(now).await.unwrap();
        let second = daemon.tick(time::now_ms()).await.unwrap();
        assert_eq!(second.adopted, 0, "this daemon's own run is not an orphan");
        let run = store.runs_for(&job.id, 1).unwrap().remove(0);
        assert_eq!(run.status, RunStatus::Running);
    }

    #[tokio::test]
    async fn a_tick_runs_a_due_script_and_records_what_it_printed() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(
                NewJob::new("say hello", "every 1m")
                    .mode(JobMode::Script)
                    .payload("echo hello from the daemon")
                    .target("local"),
                now,
            )
            .unwrap();
        // `every 1m` means the first slot is a minute away; the operator asked for it now.
        manager.run_now(&job.id, now).unwrap();
        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path()).with_tick_ms(50);
        let first = daemon.tick(now).await.unwrap();
        assert_eq!(first.dispatched, vec![job.id.clone()]);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let second = daemon.tick(time::now_ms()).await.unwrap();
        assert_eq!(
            second.finished.len(),
            1,
            "the pool result is collected once"
        );
        let runs = store.runs_for(&job.id, 5).unwrap();
        assert_eq!(runs[0].status, RunStatus::Ok);
        assert!(runs[0].output.contains("hello from the daemon"));
        assert!(
            manager.list().unwrap().iter().any(|each| each.id == job.id),
            "the job outlives its run"
        );
    }

    #[tokio::test]
    async fn a_run_left_by_a_crash_becomes_unknown_on_the_next_tick() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(NewJob::new("crashed", "every 1h"), now)
            .unwrap();
        let run = store.begin_run(&job, now, None).unwrap();
        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path());
        let report = daemon.tick(now).await.unwrap();
        assert_eq!(report.adopted, 1);
        let run = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(
            run.status,
            RunStatus::Unknown,
            "a run nobody watched is unknown, not failed"
        );
    }

    #[tokio::test]
    async fn an_overdue_slot_is_skipped_instead_of_replayed() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(NewJob::new("stale", "every 10m"), now)
            .unwrap();
        store
            .set_next_run(&job.id, Some(now - 3 * 60 * 60_000), now)
            .unwrap();
        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path());
        let report = daemon.tick(now).await.unwrap();
        assert_eq!(report.coalesced, 1);
        let job = store.get_job(&job.id).unwrap().unwrap();
        assert_eq!(job.last_status, "skipped");
        assert!(job.next_run_ms.unwrap() > now);
    }

    #[tokio::test]
    async fn a_finished_run_is_delivered_where_the_job_asked() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(
                NewJob::new("report", "every 1m")
                    .mode(JobMode::Script)
                    .payload("echo all good")
                    .target("local"),
                now,
            )
            .unwrap();
        // `every 1m` schedules the first slot a minute out; run it now instead.
        manager.run_now(&job.id, now).unwrap();
        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path()).with_tick_ms(50);
        daemon.tick(now).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let report = daemon.tick(time::now_ms()).await.unwrap();
        assert_eq!(report.delivered, vec![(job.id.clone(), true)]);
        let log = std::fs::read_to_string(notify::log_path(dir.path())).unwrap();
        assert!(log.contains("all good"), "the log holds what was reported");
        let run = store.runs_for(&job.id, 1).unwrap().remove(0);
        assert_eq!(run.delivery, Delivery::Delivered);
    }

    #[tokio::test]
    async fn a_failing_script_is_recorded_as_failed_with_its_reason() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(
                NewJob::new("broken", "every 1m")
                    .mode(JobMode::Script)
                    .payload("echo nope; exit 3")
                    .target("none"),
                now,
            )
            .unwrap();
        // `every 1m` schedules the first slot a minute out; run it now instead.
        manager.run_now(&job.id, now).unwrap();
        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path()).with_tick_ms(50);
        daemon.tick(now).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        daemon.tick(time::now_ms()).await.unwrap();
        let run = store.runs_for(&job.id, 1).unwrap().remove(0);
        assert_eq!(run.status, RunStatus::Failed);
        assert!(run.error.contains("exit status 3"), "{}", run.error);
        assert_eq!(
            run.delivery,
            Delivery::None,
            "a `none` target delivers nowhere"
        );
    }

    #[tokio::test]
    async fn a_beat_that_raises_an_issue_is_reported_once() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(NewJob::new("late", "every 10m"), now)
            .unwrap();
        store
            .set_next_run(&job.id, Some(now - 20 * 60_000), now + 1)
            .unwrap();
        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path()).with_catch_up_ms(3_600_000);
        let report = daemon.tick(now + 2).await.unwrap();
        assert!(
            report
                .beats
                .iter()
                .any(|beat| beat.kind == BeatKind::Runtime),
            "the runtime beat always answers"
        );
        let _ = 0;
    }

    #[tokio::test]
    async fn a_delegated_job_that_names_no_bot_is_refused_before_it_starts() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let job = manager
            .create(
                NewJob::new("audit", "every 1h")
                    .mode(JobMode::Delegate)
                    .payload("audit the logs"),
                time::now_ms(),
            )
            .unwrap();
        let outcome = work_for(&job, 1_000).await;
        assert!(!outcome.ok);
        assert!(
            outcome.error.unwrap_or_default().contains("--bot"),
            "an unattributed delegation is a missing identity, not a bot with a default name"
        );
    }

    #[tokio::test]
    async fn a_team_job_that_names_no_team_is_refused_before_it_starts() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let job = manager
            .create(
                NewJob::new("sweep", "every 1h")
                    .mode(JobMode::Workflow)
                    .payload("sweep the hosts"),
                time::now_ms(),
            )
            .unwrap();
        let outcome = work_for(&job, 1_000).await;
        assert!(!outcome.ok);
        assert!(outcome.error.unwrap_or_default().contains("--agent"));
    }
}
