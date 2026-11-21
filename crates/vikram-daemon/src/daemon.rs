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
use crate::triggers::{EventBus, QuietHours, TriggerKind, URGENT_PRIORITY};
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
    match process_started_ms(body.pid) {
        // /proc counts start time in whole clock ticks, so the reading can be a few milliseconds off
        // the one recorded; the comparison is about identity, not precision.
        Some(started) => (started - body.started_ms).abs() <= 2_000,
        // No /proc (macOS, Windows) means no start time, so a live pid is all the identity left to
        // check. A reused pid then looks like the holder — the safer mistake, because the
        // alternative is a second daemon writing the same SQLite file.
        None => process_alive(body.pid),
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 runs the existence and permission checks without delivering anything.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is null-checked and closed on every path. A process we cannot open is
    // either gone or not ours, and in both cases it is not holding this lock.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code) != 0;
        let _ = CloseHandle(handle);
        ok && code == STILL_ACTIVE
    }
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
    // Clock ticks per second is a kernel constant that varies by device (100/250/300/1000);
    // hardcoding 100 makes start times wrong on kernels that tick faster.
    let ticks_per_sec = clock_ticks_per_sec();
    Some(boot_ms + ticks * 1000 / ticks_per_sec)
}

#[cfg(unix)]
fn clock_ticks_per_sec() -> i64 {
    // SAFETY: sysconf has no preconditions and cannot fail for _SC_CLK_TCK on unix.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 { hz } else { 100 }
}

#[cfg(not(unix))]
fn clock_ticks_per_sec() -> i64 {
    100
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
    /// Jobs an event armed this pass.
    pub triggered: Vec<String>,
    /// Jobs and notifications quiet hours held back this pass.
    pub suppressed: Vec<String>,
}

impl TickReport {
    pub fn is_quiet(&self) -> bool {
        self.dispatched.is_empty()
            && self.finished.is_empty()
            && self.beats.is_empty()
            && self.refused.is_empty()
            && self.triggered.is_empty()
            && self.suppressed.is_empty()
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
    /// Woken by [`EventBus::emit`] so a signal does not wait out the tick interval.
    wake: Arc<Notify>,
    bus: EventBus,
}

impl Daemon {
    pub fn new(store: ScheduleStore, home: impl Into<PathBuf>) -> Self {
        let jobs = JobManager::new(store.clone());
        let pool = WorkerPool::new(Limits::default());
        let results = pool.take_results().expect("a fresh pool owns its results");
        let wake = Arc::new(Notify::new());
        let bus = EventBus::new(store.clone(), wake.clone());
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
            wake,
            bus,
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

    /// The bus internal emitters and external signals share. Emitting through it validates the
    /// trigger kind (deny-first) and wakes the run loop when it is waiting between ticks.
    pub fn bus(&self) -> EventBus {
        self.bus.clone()
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
        // so this is where it lands; the key is deleted once honoured so the next pass does not
        // repeat it and `daemon_state` does not keep every cancel ever asked for. A run that
        // already finished has nothing left to abort; its key is swept as litter instead.
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
            let _ = self.store.delete_state(&format!("cancel.{id}"));
        }

        for job in self.store.list_jobs()? {
            if job.enabled && self.stale_slot(&job, now)? {
                report.coalesced += 1;
            }
        }

        // Events land before the due scan so a job armed by one is claimed in this same pass.
        self.drain_events(now, &mut report)?;

        let quiet = QuietHours::load(&self.store)?;
        let silent = quiet.is_quiet(now);
        for job in self.store.due_jobs(now)? {
            // The slot to claim is the one the store says is due. Recomputing it from the schedule
            // would produce the *following* slot and the compare-and-set would never match.
            let Some(slot) = job.next_run_ms else {
                continue;
            };
            // Quiet hours suppress non-urgent work: the slot is advanced exactly like a stale one
            // — skipped, not replayed — and the decision is audited. Urgent jobs still fire.
            if silent && job.priority < URGENT_PRIORITY {
                let next = self.jobs.next_slot(&job, now);
                self.store.skip_slot(&job.id, next, now)?;
                self.store.audit(
                    "job.suppress",
                    &job.id,
                    &format!("quiet hours {}", quiet.describe()),
                    now,
                )?;
                report.suppressed.push(job.id);
                continue;
            }
            match self.jobs.claim(&job, slot, now) {
                Ok(Some(claimed)) => self.dispatch(claimed, now, &mut report)?,
                Ok(None) => {}
                Err(err) => report.refused.push(format!("{}: {err}", job.id)),
            }
        }

        self.collect(&mut report, now).await?;
        self.reconcile_lost(now)?;
        self.sweep_state()?;
        self.beat(now, &mut report).await?;
        self.maintenance(now, &mut report).await;
        Ok(report)
    }

    /// The one piece of runtime housekeeping the daemon can do without a model: memory
    /// consolidation, against the same `vikram.db` the scheduler store opened and with the same
    /// defaults the `memory.consolidate` tool uses. Hourly, stamped before the work starts so a
    /// slow pass cannot overlap the next tick's, and failures are reported rather than retried
    /// hot.
    async fn maintenance(&mut self, now: Millis, report: &mut TickReport) {
        const KEY: &str = "maintenance.consolidate.last_ms";
        const INTERVAL: Millis = 60 * 60_000;
        let last = self.store.state_ms(KEY).ok().flatten().unwrap_or(0);
        if now.saturating_sub(last) < INTERVAL {
            return;
        }
        if let Err(err) = self.store.set_state_ms(KEY, now) {
            report.refused.push(format!("maintenance: {err}"));
            return;
        }
        let database = self.home.join("vikram.db");
        let outcome = tokio::task::spawn_blocking(move || {
            let store = vikram_runtime::consolidation::ConsolidationStore::open(&database)?;
            store.run_consolidation(
                None,
                &vikram_runtime::consolidation::ConsolidationPolicy::default(),
            )
        })
        .await;
        match outcome {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                eprintln!("[vikram] memory consolidation failed: {err}");
                report.refused.push(format!("maintenance: {err}"));
            }
            Err(err) => {
                eprintln!("[vikram] memory consolidation task failed: {err}");
                report.refused.push(format!("maintenance: {err}"));
            }
        }
    }

    /// A `running` row older than its job's timeout is a lost Finished, not a slow job: the work
    /// cannot legitimately outlive its own deadline, so whatever is left is aborted and the run
    /// settled as unknown, which is what unblocks the job's next claim.
    fn reconcile_lost(&mut self, now: Millis) -> Result<(), StoreError> {
        for run in self.store.running_runs()? {
            let Ok(Some(job)) = self.store.get_job(&run.job_id) else {
                continue;
            };
            let timeout = job.timeout_ms.unwrap_or(DEFAULT_JOB_TIMEOUT_MS).max(1_000);
            // The grace covers a Finished already sent but not yet collected: it can only be a
            // tick old, never a whole timeout.
            if now.saturating_sub(run.started_ms) <= timeout.saturating_add(self.tick_ms as Millis)
            {
                continue;
            }
            self.pool.cancel(&run.id);
            self.store.expire_run(&run, now)?;
            self.running.retain(|entry| entry.run_id != run.id);
            if job.next_run_ms.is_none() {
                self.store.set_enabled(&job.id, false, now)?;
            }
        }
        Ok(())
    }

    /// `cancel.` and `member.` keys belong to a run. Honoured cancels are deleted where they are
    /// honoured, but a request that lands after its run ended — or a member stop on a hive that
    /// crashed — would sit in `daemon_state` forever, so anything whose run is no longer live is
    /// swept.
    fn sweep_state(&self) -> Result<(), StoreError> {
        let mut live: std::collections::HashSet<String> = self
            .running
            .iter()
            .map(|entry| entry.run_id.clone())
            .collect();
        live.extend(self.store.running_runs()?.into_iter().map(|run| run.id));
        for prefix in ["cancel.", "member."] {
            for key in self.store.state_keys(prefix)? {
                // Run ids never contain a dot, so the first segment after the prefix is the run;
                // for `member.` keys the rest is the agent that was asked to stop.
                let run = key
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.split('.').next())
                    .unwrap_or("");
                if !run.is_empty() && !live.contains(run) {
                    self.store.delete_state(&key)?;
                }
            }
        }
        Ok(())
    }

    /// Offer every event the scheduler has not seen to the jobs subscribed to it.
    ///
    /// An event arms a job by moving its `next_run_ms` to now; the ordinary claim path then runs
    /// it, which is what gives event fires the same once-only and retry semantics as cron slots.
    /// Events older than the catch-up window are passed over — a signal from before the daemon
    /// was away is history, not work — and the cursor still advances past them.
    fn drain_events(&self, now: Millis, report: &mut TickReport) -> Result<(), StoreError> {
        const SEEN_KEY: &str = "events.seen";
        let seen = self.store.state_ms(SEEN_KEY).ok().flatten().unwrap_or(0);
        let mut latest = seen;
        for row in self.store.events_after(seen)? {
            latest = row.seq;
            if now.saturating_sub(row.at_ms) > self.catch_up_ms {
                continue;
            }
            let Ok(kind) = TriggerKind::parse(&row.kind) else {
                self.store
                    .audit("trigger.refused", &row.kind, &row.detail, now)?;
                continue;
            };
            let mut armed = Vec::new();
            for job in self.store.list_jobs()? {
                if !job.enabled {
                    continue;
                }
                let Some(trigger) = self.jobs.trigger_of(&job) else {
                    continue;
                };
                if !trigger.matches(kind, &row.subject) {
                    continue;
                }
                // Deny-first: a job may not wake itself. `on job.settled` matching its own id
                // would feed every run into the next one forever.
                if kind == TriggerKind::JobSettled && row.subject == job.id {
                    continue;
                }
                if self.store.set_next_run(&job.id, Some(now), now)? {
                    armed.push(job.id.clone());
                }
            }
            if !armed.is_empty() {
                self.store.audit(
                    "trigger.fire",
                    &format!("{}:{}", kind.as_str(), row.subject),
                    &armed.join(", "),
                    now,
                )?;
                report.triggered.extend(armed);
            }
        }
        if latest != seen {
            self.store.set_state_ms(SEEN_KEY, latest)?;
        }
        Ok(())
    }

    /// Record an internal event and wake the run loop. Emission is best-effort on the wake side —
    /// the row is the truth, and the next tick finds it even if nobody was listening.
    fn emit(
        &self,
        kind: TriggerKind,
        subject: &str,
        detail: &str,
        now: Millis,
    ) -> Result<(), StoreError> {
        if self
            .store
            .emit_event(
                kind.as_str(),
                subject,
                detail,
                crate::triggers::EVENT_DEDUP_MS,
                now,
            )?
            .is_some()
        {
            self.wake.notify_waiters();
        }
        Ok(())
    }

    /// Advance a slot that is older than the tolerance instead of running it.
    fn stale_slot(&self, job: &Job, now: Millis) -> Result<bool, StoreError> {
        // An event-armed job is not a missed schedule slot: its wake-up is a signal, and skipping
        // it would silently drop the event the job was waiting for.
        if self.jobs.event_driven(job) {
            return Ok(false);
        }
        let Some(next) = job.next_run_ms else {
            return Ok(false);
        };
        if next > now || now - next <= self.catch_up_ms {
            return Ok(false);
        }
        let next = self.jobs.next_slot(job, now);
        self.store.skip_slot(&job.id, next, now)?;
        Ok(true)
    }

    fn dispatch(
        &mut self,
        claimed: Claimed,
        now: Millis,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let Claimed { job, run, slot } = claimed;
        let timeout = job.timeout_ms.unwrap_or(DEFAULT_JOB_TIMEOUT_MS).max(1_000);
        let stop = Some(member_probe(self.store.clone(), &run.id));
        let work = work_for(&job, timeout, stop);
        let id = run.id.clone();
        match self.pool.dispatch(id, class_of(job.mode), work) {
            Ok(()) => {
                self.running.push(Running {
                    job_id: job.id.clone(),
                    run_id: run.id.clone(),
                });
                self.store.audit(
                    "job.dispatch",
                    &job.id,
                    &format!("run {} slot {}", run.id, slot),
                    now,
                )?;
                report.dispatched.push(job.id);
                Ok(())
            }
            Err(err) => {
                // No free slot is not a failure of the job: the run is closed so the next tick can
                // take it again, and the reason is recorded rather than left in flight forever.
                // `skip_run` also switches the job off when it has no next slot — a one-time job
                // whose dispatch was refused would otherwise sit enabled forever, never firing.
                let next = self.jobs.next_slot(&job, now);
                self.store
                    .skip_run(&run.id, &job.id, &format!("not started: {err}"), next, now)?;
                report.refused.push(format!("{}: {err}", job.id));
                Ok(())
            }
        }
    }

    async fn collect(&mut self, report: &mut TickReport, now: Millis) -> Result<(), StoreError> {
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
                self.settle(&running, done, now, report).await?;
            }
        }
        report.finished = finished;
        Ok(())
    }

    async fn settle(
        &mut self,
        running: &Running,
        done: &Finished,
        now: Millis,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let Ok(Some(job)) = self.store.get_job(&running.job_id) else {
            return Ok(());
        };
        let Ok(Some(run)) = self.store.get_run(&running.run_id) else {
            return Ok(());
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
        if done.cancelled {
            // `settle` would record this as a failure and offer it a retry, overwriting the
            // cancellation the operator asked for.
            self.jobs
                .settle_cancelled(&job, &run, &done.outcome.output, &error, now)?;
            self.emit(TriggerKind::JobSettled, &job.id, "cancelled", now)?;
            return Ok(());
        }
        // `settle` decides what the job's next slot is, including a retry that comes back sooner
        // than the schedule would have said.
        self.jobs
            .settle(&job, &run, ok, &done.outcome.output, &error, now)?;
        // The state change is itself a signal: jobs subscribed to `on job.settled` wake on it.
        self.emit(TriggerKind::JobSettled, &job.id, status.as_str(), now)?;
        // Bound history: without this every run row (with its output blob) is kept forever.
        let _ = self.store.prune_runs(&job.id, 200);
        let notice = Notice {
            job: job.id.clone(),
            name: job.name.clone(),
            status: status.as_str().to_string(),
            summary: done.outcome.summary(),
            session: done.outcome.session.clone(),
            at: now,
        };
        // Quiet hours hold back reports from non-urgent jobs. The run itself already settled —
        // suppression is a delivery decision, recorded as `suppressed` rather than as a failure.
        let urgent = job.priority >= URGENT_PRIORITY;
        let silent = QuietHours::load(&self.store)
            .map(|quiet| quiet.is_quiet(now))
            .unwrap_or(false);
        match Destination::parse(&job.target) {
            Ok(destination) => {
                if silent && !urgent && destination != Destination::None {
                    self.store
                        .set_delivery_note(&run.id, Delivery::Suppressed, "quiet hours")?;
                    self.store
                        .audit("notify.suppress", &job.id, "quiet hours", now)?;
                    report.suppressed.push(format!("notify:{}", job.id));
                    return Ok(());
                }
                // Set when a session delivery landed somewhere other than the session the job
                // named, so the record can say `rerouted` instead of claiming the asked-for
                // destination was reached.
                let mut rerouted = false;
                let delivered = match &destination {
                    // A conversation is written to directly. The daemon keeps a runtime for exactly
                    // this, and what it writes lands in the same database the terminal reads, so the
                    // report is there the next time that conversation is opened.
                    Destination::Session(wanted) => {
                        match self.postbox.post(wanted, &notice.line()).await {
                            Ok((session, note)) => {
                                rerouted = note.is_some();
                                // A postbox entry is itself an event other jobs can subscribe to.
                                self.emit(TriggerKind::Postbox, &job.id, &session, now)?;
                                notify::Report {
                                    delivered: true,
                                    detail: match note {
                                        Some(note) => format!("session {session}: {note}"),
                                        None => format!("session {session}"),
                                    },
                                }
                            }
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
                    _ if !delivered.delivered => Delivery::Failed,
                    _ if rerouted => Delivery::Rerouted,
                    _ => Delivery::Delivered,
                };
                // The note is part of the record: `delivery: failed` on its own tells an operator
                // nothing about what to fix.
                self.store
                    .set_delivery_note(&run.id, state, &delivered.detail)?;
                // A delivered notification is a signal too: `on notify` jobs wake on it.
                if delivered.delivered && destination != Destination::None {
                    self.emit(TriggerKind::Notify, &job.id, &destination.label(), now)?;
                }
                report.delivered.push((job.id, delivered.delivered));
            }
            Err(err) => report.refused.push(format!("{}: {err}", job.id)),
        }
        Ok(())
    }

    /// Ask the heartbeats whether anything needs attention, and report what they raised.
    async fn beat(&mut self, now: Millis, report: &mut TickReport) -> Result<(), StoreError> {
        let heartbeat = Heartbeat::new(&self.store, self.policy.clone());
        if heartbeat.next_due(now).is_none() {
            return Ok(());
        }
        // Heartbeat notices are non-urgent by definition: quiet hours hold them like any other
        // report, and the suppression is audited rather than silently dropped.
        let silent = QuietHours::load(&self.store)
            .map(|quiet| quiet.is_quiet(now))
            .unwrap_or(false);
        for kind in [BeatKind::Runtime, BeatKind::Agent, BeatKind::Workers] {
            match heartbeat.beat(kind, now) {
                Ok(beat) => {
                    if !beat.notify.is_empty() {
                        let subject = format!("heartbeat.{}", beat.kind.as_str());
                        if silent {
                            self.store
                                .audit("notify.suppress", &subject, "quiet hours", now)?;
                            report.suppressed.push(format!("notify:{subject}"));
                        } else {
                            let notice = Notice {
                                job: subject.clone(),
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
                            let delivered =
                                notify::deliver(&Destination::Local, &notice, &self.home, None)
                                    .await;
                            report
                                .delivered
                                .push((subject.clone(), delivered.delivered));
                            if delivered.delivered {
                                self.emit(TriggerKind::Notify, &subject, "local", now)?;
                            }
                        }
                    }
                    report.beats.push(beat);
                }
                Err(err) => report
                    .refused
                    .push(format!("heartbeat {}: {err}", kind.as_str())),
            }
        }
        Ok(())
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
        let wake = self.wake.clone();
        loop {
            if let Err(err) = self.tick(time::now_ms()).await {
                eprintln!("[vikram] tick failed: {err}");
            }
            tokio::select! {
                _ = stop.notified() => return Ok(()),
                // An event landed while the tick ran (or between ticks): go straight back
                // around instead of sleeping out the interval.
                _ = wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(self.tick_ms)) => {}
            }
        }
    }
}

/// Answer, for one running hive, whether the operator has stopped a member. The request travels
/// through the scheduler database because the CLI and the daemon are different processes.
fn member_probe(store: ScheduleStore, run: &str) -> runner::StopProbe {
    let run = run.to_string();
    std::sync::Arc::new(move |agent: &str| {
        store
            .state_bool(&format!("member.{run}.{agent}"), false)
            .unwrap_or(false)
    })
}

fn work_for(job: &Job, timeout: Millis, stop: Option<runner::StopProbe>) -> Work {
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
            // A hive job names a saved hive; its members each get their own delegated turn.
            JobMode::Workflow => {
                if job.agent.trim().is_empty() {
                    return RunOutcome::failed(
                        "a workflow job needs a hive: set --agent <hive>".to_string(),
                    );
                }
                let config = match vikram_runtime::config::Config::load() {
                    Ok(config) => config,
                    Err(err) => return RunOutcome::failed(format!("config: {err}")),
                };
                let request =
                    runner::HiveRosterRequest::new(job.agent.clone(), job.payload.clone())
                        .title(runner::title_from(&job.payload))
                        .in_order(runner::HiveRosterOrder::parse(&job.order).unwrap_or_default())
                        // A member stopped from the CLI is asked about between events, so the rest of
                        // the hive keeps working instead of being cancelled with it.
                        .stopping_when(stop);
                let request = match job.target.strip_prefix("model:") {
                    Some(model) => request.model(model.to_string()),
                    None => request,
                };
                runner::run_hive_roster(config, request, limit).await
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
            !store
                .state_bool(&format!("cancel.{}", run.id), false)
                .unwrap_or(true),
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
        let outcome = work_for(&job, 1_000, None).await;
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
        let outcome = work_for(&job, 1_000, None).await;
        assert!(!outcome.ok);
        assert!(outcome.error.unwrap_or_default().contains("--agent"));
    }

    #[tokio::test]
    async fn state_keys_for_runs_that_are_over_are_swept() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(
                NewJob::new("sweep me", "every 1m")
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
        let run = store.runs_for(&job.id, 1).unwrap().remove(0);
        assert_eq!(run.status, RunStatus::Running);

        // Keys for a run that does not exist are litter; keys for the live run are not.
        store.set_state("cancel.run_dead_1_2_3", "1").unwrap();
        store
            .set_state(
                &crate::store::member_stop_key("run_dead_1_2_3", "mira"),
                "1",
            )
            .unwrap();
        store
            .set_state(&crate::store::member_stop_key(&run.id, "mira"), "1")
            .unwrap();
        daemon.tick(time::now_ms()).await.unwrap();

        assert!(store.state("cancel.run_dead_1_2_3").unwrap().is_none());
        assert!(
            store
                .state(&crate::store::member_stop_key("run_dead_1_2_3", "mira"))
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .state(&crate::store::member_stop_key(&run.id, "mira"))
                .unwrap()
                .is_some(),
            "a live run keeps the keys the operator may still need"
        );

        tokio::time::sleep(Duration::from_millis(1_400)).await;
        daemon.tick(time::now_ms()).await.unwrap();
        assert!(
            store
                .state(&crate::store::member_stop_key(&run.id, "mira"))
                .unwrap()
                .is_none(),
            "once the run is over its keys go too"
        );
    }

    #[tokio::test]
    async fn a_run_that_never_reported_back_stops_wedging_its_job() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let anchor = manager
            .create(
                NewJob::new("anchor", "every 1m")
                    .mode(JobMode::Script)
                    .payload("sleep 30")
                    .target("none"),
                now,
            )
            .unwrap();
        manager.run_now(&anchor.id, now).unwrap();
        let stuck_job = manager
            .create(
                NewJob::new("stuck", "every 1m")
                    .mode(JobMode::Script)
                    .payload("true")
                    .target("none")
                    .timeout_ms(500),
                now,
            )
            .unwrap();

        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path()).with_tick_ms(50);
        daemon.tick(now).await.unwrap();
        // The stuck job's run row exists and its Finished was lost: the daemon still lists it as
        // in flight even though nothing is running it.
        let stuck_run = store.begin_run(&stuck_job, now, None).unwrap();
        daemon.running.push(Running {
            job_id: stuck_job.id.clone(),
            run_id: stuck_run.id.clone(),
        });

        tokio::time::sleep(Duration::from_millis(1_300)).await;
        daemon.tick(time::now_ms()).await.unwrap();

        let run = store.get_run(&stuck_run.id).unwrap().unwrap();
        assert_eq!(
            run.status,
            RunStatus::Unknown,
            "a lost Finished is settled as unknown, not left running"
        );
        assert_eq!(daemon.in_flight(), 1, "only the anchor is still in flight");

        // The in-flight check that wedged the job now lets it be claimed again.
        let slot = time::now_ms();
        store.set_next_run(&stuck_job.id, Some(slot), slot).unwrap();
        let fresh = store.get_job(&stuck_job.id).unwrap().unwrap();
        let claimed = manager.claim(&fresh, slot, slot).unwrap();
        assert!(claimed.is_some(), "the job can be claimed again");
    }

    /// A quiet window that certainly contains `now`, whatever zone the machine is in: five hours
    /// either side of the current local minute, wrapped the same way the policy wraps.
    fn quiet_covering(now: Millis) -> QuietHours {
        let civil = time::civil_in(crate::time::Zone::Local, now);
        let minute = civil.hour * 60 + civil.minute;
        QuietHours {
            enabled: true,
            start: (minute + 1440 - 300) % 1440,
            end: (minute + 300) % 1440,
        }
    }

    #[tokio::test]
    async fn an_event_wakes_a_subscribed_job_and_the_fire_is_audited() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(
                NewJob::new("on mail", "on postbox")
                    .mode(JobMode::Script)
                    .payload("echo woke up")
                    .target("none"),
                now,
            )
            .unwrap();
        assert!(job.next_run_ms.is_none(), "an event job has no clock slot");
        assert!(job.enabled, "it is armed, just not scheduled");

        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path()).with_tick_ms(50);
        // Nothing has happened yet: the job must not fire on a clock that does not exist.
        let idle = daemon.tick(now).await.unwrap();
        assert!(idle.dispatched.is_empty());
        assert!(store.runs_for(&job.id, 5).unwrap().is_empty());

        daemon
            .bus()
            .emit("postbox", "job-1", "a report landed", now)
            .unwrap();
        let woke = daemon.tick(time::now_ms()).await.unwrap();
        assert_eq!(woke.triggered, vec![job.id.clone()]);
        assert_eq!(woke.dispatched, vec![job.id.clone()]);

        tokio::time::sleep(Duration::from_millis(300)).await;
        daemon.tick(time::now_ms()).await.unwrap();
        let runs = store.runs_for(&job.id, 5).unwrap();
        assert_eq!(runs.len(), 1, "the event fired the job exactly once");
        assert_eq!(runs[0].status, RunStatus::Ok);
        // And the job is armed again: settling did not switch it off.
        let after = store.get_job(&job.id).unwrap().unwrap();
        assert!(after.enabled);
        assert!(after.next_run_ms.is_none());

        let trail = store.audit_trail(50).unwrap();
        let kinds: Vec<&str> = trail.iter().map(|row| row.kind.as_str()).collect();
        for expected in ["trigger.emit", "trigger.fire", "job.dispatch"] {
            assert!(
                kinds.contains(&expected),
                "audit holds {expected}: {kinds:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_same_event_inside_the_dedup_window_fires_once() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(
                NewJob::new("on mail", "on postbox")
                    .mode(JobMode::Script)
                    .payload("echo once")
                    .target("none"),
                now,
            )
            .unwrap();
        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path());
        let bus = daemon.bus();
        assert!(matches!(
            bus.emit("postbox", "job-1", "first", now).unwrap(),
            crate::triggers::EmitOutcome::Recorded(_)
        ));
        assert!(matches!(
            bus.emit("postbox", "job-1", "second", now + 1_000).unwrap(),
            crate::triggers::EmitOutcome::Deduped
        ));
        daemon.tick(time::now_ms()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        daemon.tick(time::now_ms()).await.unwrap();
        assert_eq!(
            store.runs_for(&job.id, 5).unwrap().len(),
            1,
            "a deduplicated event is one fire, not two"
        );
    }

    #[tokio::test]
    async fn quiet_hours_skip_ordinary_jobs_but_urgent_ones_still_fire() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let ordinary = manager
            .create(
                NewJob::new("ordinary", "every 1m")
                    .mode(JobMode::Script)
                    .payload("echo ordinary")
                    .target("none"),
                now,
            )
            .unwrap();
        let urgent = manager
            .create(
                NewJob::new("urgent", "every 1m")
                    .mode(JobMode::Script)
                    .payload("echo urgent")
                    .target("none")
                    .priority(URGENT_PRIORITY),
                now,
            )
            .unwrap();
        manager.run_now(&ordinary.id, now).unwrap();
        manager.run_now(&urgent.id, now).unwrap();
        quiet_covering(now).apply(&store, now).unwrap();

        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path());
        let report = daemon.tick(time::now_ms()).await.unwrap();
        assert!(
            report.suppressed.contains(&ordinary.id),
            "{:?}",
            report.suppressed
        );
        assert!(
            report.dispatched.contains(&urgent.id),
            "{:?}",
            report.dispatched
        );
        assert!(store.runs_for(&ordinary.id, 5).unwrap().is_empty());
        let ordinary_job = store.get_job(&ordinary.id).unwrap().unwrap();
        assert_eq!(ordinary_job.last_status, "skipped");

        let trail = store.audit_trail(50).unwrap();
        assert!(
            trail
                .iter()
                .any(|row| row.kind == "job.suppress" && row.subject == ordinary.id),
            "the suppression decision is audited: {trail:?}"
        );

        // Off again, the same job runs.
        QuietHours {
            enabled: false,
            ..quiet_covering(now)
        }
        .apply(&store, now)
        .unwrap();
        manager.run_now(&ordinary.id, now).unwrap();
        let report = daemon.tick(time::now_ms()).await.unwrap();
        assert!(report.dispatched.contains(&ordinary.id));
    }

    #[tokio::test]
    async fn quiet_hours_hold_back_the_report_not_the_run() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let job = manager
            .create(
                NewJob::new("report", "every 1m")
                    .mode(JobMode::Script)
                    .payload("echo quiet report")
                    .target("local"),
                now,
            )
            .unwrap();
        manager.run_now(&job.id, now).unwrap();

        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path()).with_tick_ms(50);
        // The run starts before quiet hours; it settles inside them, which is the case where a
        // report is held back while the work itself is allowed to finish.
        daemon.tick(time::now_ms()).await.unwrap();
        quiet_covering(time::now_ms())
            .apply(&store, time::now_ms())
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let report = daemon.tick(time::now_ms()).await.unwrap();
        assert!(
            report
                .suppressed
                .iter()
                .any(|s| s == &format!("notify:{}", job.id)),
            "{:?}",
            report.suppressed
        );

        let run = store.runs_for(&job.id, 1).unwrap().remove(0);
        assert_eq!(run.status, RunStatus::Ok, "the work still ran");
        assert_eq!(run.delivery, Delivery::Suppressed);
        assert!(run.delivery_note.contains("quiet"));
        assert!(
            !notify::log_path(dir.path()).exists(),
            "nothing was written to the notification log"
        );
        let trail = store.audit_trail(50).unwrap();
        assert!(trail.iter().any(|row| row.kind == "notify.suppress"));
    }

    #[tokio::test]
    async fn a_settled_job_is_itself_an_event_for_other_jobs() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        let source = manager
            .create(
                NewJob::new("source", "every 1m")
                    .mode(JobMode::Script)
                    .payload("true")
                    .target("none"),
                now,
            )
            .unwrap();
        let listener = manager
            .create(
                NewJob::new("listener", format!("on job.settled:{}", source.id))
                    .mode(JobMode::Script)
                    .payload("echo heard")
                    .target("none"),
                now,
            )
            .unwrap();
        manager.run_now(&source.id, now).unwrap();

        let dir = home();
        let mut daemon = Daemon::new(store.clone(), dir.path()).with_tick_ms(50);
        daemon.tick(now).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        // This pass settles the source and emits job.settled.
        daemon.tick(time::now_ms()).await.unwrap();
        // The next pass drains the event and dispatches the listener.
        let report = daemon.tick(time::now_ms()).await.unwrap();
        assert!(
            report.triggered.contains(&listener.id),
            "{:?}",
            report.triggered
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        daemon.tick(time::now_ms()).await.unwrap();
        let runs = store.runs_for(&listener.id, 5).unwrap();
        assert_eq!(runs.len(), 1, "the listener ran once for the settlement");
        assert_eq!(runs[0].status, RunStatus::Ok);
    }

    #[tokio::test]
    async fn an_unknown_trigger_is_refused_at_the_door() {
        let store = store();
        let manager = JobManager::new(store.clone());
        let now = time::now_ms();
        // A job cannot subscribe to a signal the daemon does not produce.
        let err = manager
            .create(NewJob::new("bad", "on smoke-signal"), now)
            .unwrap_err();
        assert!(err.to_string().contains("smoke-signal"), "{err}");
        assert!(manager.list().unwrap().is_empty());

        // And a hand-raised signal with an unknown kind is refused and audited.
        let dir = home();
        let daemon = Daemon::new(store.clone(), dir.path());
        let refused = daemon.bus().emit("smoke-signal", "", "", now).unwrap_err();
        assert!(refused.to_string().contains("smoke-signal"));
        let trail = store.audit_trail(20).unwrap();
        assert!(trail.iter().any(|row| row.kind == "trigger.refused"));
    }
}
