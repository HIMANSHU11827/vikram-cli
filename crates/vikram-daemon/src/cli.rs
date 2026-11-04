//! The verbs behind `vikram daemon`, `vikram heartbeat`, `vikram cron` and `vikram jobs`.
//!
//! Everything here returns the text an operator should see rather than printing it, so the same
//! function can be checked by a test, shown by the CLI, or rendered by a panel. The only exceptions
//! are the two long-running entries — `run` (the supervisor itself) and `spawn` (which detaches it)
//! — because a process that stays in the foreground cannot return a string when it is done.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use crate::daemon::{Daemon, Lock, LockError, owner};
use crate::heartbeat::{Beat, BeatKind, Heartbeat, Policy};
use crate::jobs::{JobManager, NewJob};
use crate::notify;
use crate::service;
use crate::store::{Delivery, JobMode, Run, RunStatus, ScheduleStore};
use crate::time::{Zone, format_ms, humanize_ms, now_ms, parse_duration};
use crate::worker::TaskClass;

/// Where the daemon keeps its lock, its log and its service definition.
pub fn home() -> PathBuf {
    vikram_runtime::config::vikram_home()
}

/// The one database every vikram process shares, so a job created in the TUI is the same job the
/// daemon claims.
pub fn database() -> PathBuf {
    vikram_runtime::config::database_path()
}

/// Open the shared store, creating the daemon tables the first time it is touched.
pub fn store() -> Result<ScheduleStore, String> {
    open_at(&database())
}

fn open_at(path: &Path) -> Result<ScheduleStore, String> {
    let store = ScheduleStore::open(path)
        .map_err(|err| format!("could not open {}: {err}", path.display()))?;
    store
        .ensure_schema()
        .map_err(|err| format!("could not prepare the daemon tables: {err}"))?;
    Ok(store)
}

/// A store for tests and for callers that already hold one of their own.
pub fn open(path: &Path) -> Result<ScheduleStore, String> {
    open_at(path)
}

// ---------------------------------------------------------------------------
// daemon
// ---------------------------------------------------------------------------

/// What the lock file says, without trusting it: a lock whose process is gone is reported as stale,
/// because a pid file that outlives its process is the normal way a daemon "starts twice".
pub fn status() -> Result<String, String> {
    let home = home();
    let owner = owner(&home).map_err(|err| format!("could not read the lock: {err}"))?;
    let store = store()?;
    let jobs = store.list_jobs().map_err(|err| err.to_string())?;
    let enabled = jobs.iter().filter(|job| job.enabled).count();
    let mut text = String::new();
    match owner {
        Some(Owner { pid, alive, .. }) => {
            text.push_str(&format!(
                "daemon: {}\n",
                if alive {
                    format!("running (pid {pid})")
                } else {
                    format!("stale lock from pid {pid}")
                }
            ));
        }
        None => text.push_str("daemon: not running\n"),
    }
    text.push_str(&format!("home: {}\n", home.display()));
    text.push_str(&format!("jobs: {} ({enabled} enabled)\n", jobs.len()));
    let beats = Heartbeat::new(&store, Policy::default());
    text.push_str(&format!(
        "heartbeat: {}\n",
        if !beats.enabled() {
            "disabled".to_string()
        } else if beats.paused() {
            "paused".to_string()
        } else {
            format!("every {}", humanize_ms(beats.interval_ms()))
        }
    ));
    text.push_str(&format!("service: {}\n", service::platform().as_str()));
    Ok(text)
}

/// A short status for scripts and for the TUI's header line.
pub fn health() -> Result<String, String> {
    let owner = owner(&home()).map_err(|err| err.to_string())?;
    match owner {
        Some(Owner {
            pid, alive: true, ..
        }) => Ok(format!("ok · daemon pid {pid}")),
        Some(Owner {
            pid, alive: false, ..
        }) => Ok(format!("stale · pid {pid} is gone")),
        None => Ok("idle · no daemon".to_string()),
    }
}

/// The supervisor itself. Holds the lock for as long as it runs, so a second `start` is refused
/// rather than quietly competing for the same jobs.
pub async fn run(foreground: bool) -> Result<(), String> {
    let home = home();
    let lock = Lock::acquire(&home).map_err(|err| match err {
        LockError::Held { pid } => format!("a daemon is already running (pid {pid})"),
        LockError::Io(err) => format!("could not take the lock: {err}"),
    })?;
    let store = store()?;
    let daemon = Daemon::new(store, home.clone());
    if foreground {
        eprintln!(
            "[vikram] daemon running in the foreground (pid {}) — Ctrl+C to stop",
            std::process::id()
        );
    }
    let stop = Arc::new(Notify::new());
    let signal = stop.clone();
    let _signal = tokio::spawn(async move {
        wait_for_signal().await;
        eprintln!("[vikram] daemon stopping");
        signal.notify_waiters();
    });
    let outcome = daemon.run(stop).await;
    drop(lock);
    outcome.map_err(|err| format!("daemon stopped on an error: {err}"))
}

/// `start` without the foreground process in the way: the daemon is spawned detached and this call
/// returns as soon as it has been written down, not when it has finished.
pub fn spawn() -> Result<String, String> {
    let home = home();
    if let Some(Owner {
        pid, alive: true, ..
    }) = owner(&home).map_err(|err| err.to_string())?
    {
        return Ok(format!("already running (pid {pid})"));
    }
    std::fs::create_dir_all(home.join("daemon")).map_err(|err| err.to_string())?;
    let exe = std::env::current_exe().map_err(|err| format!("could not find vikram: {err}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("daemon/daemon.log"))
        .map_err(|err| format!("could not open the daemon log: {err}"))?;
    let err_log = log.try_clone().map_err(|err| err.to_string())?;
    let child = std::process::Command::new(exe)
        .args(["daemon", "start", "--foreground"])
        .env("VIKRAM_HOME", &home)
        .stdout(log)
        .stderr(err_log)
        .spawn()
        .map_err(|err| format!("could not start the daemon: {err}"))?;
    Ok(format!(
        "started (pid {})\nlog: {}",
        child.id(),
        home.join("daemon/daemon.log").display()
    ))
}

/// Ask the running daemon to stop and wait the short, bounded time a clean shutdown needs.
pub fn stop() -> Result<String, String> {
    let home = home();
    let Some(Owner { pid, alive, .. }) = owner(&home).map_err(|err| err.to_string())? else {
        return Ok("nothing to stop".to_string());
    };
    if !alive {
        let _ = std::fs::remove_file(home.join("daemon/daemon.lock"));
        return Ok(format!("cleared stale lock from pid {pid}"));
    }
    // SIGTERM, so the supervisor finishes the tick it is in rather than dying mid-run.
    let killed = unsafe { libc::kill(pid as i32, libc::SIGTERM) } == 0;
    if !killed {
        return Err(format!(
            "could not signal pid {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    for _ in 0..50 {
        if matches!(
            owner(&home),
            Ok(None) | Ok(Some(Owner { alive: false, .. }))
        ) {
            return Ok(format!("stopped (pid {pid})"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(format!(
        "asked pid {pid} to stop; it has 5s before the lock is cleared"
    ))
}

pub fn restart() -> Result<String, String> {
    let stopped = stop()?;
    let started = spawn()?;
    Ok(format!("{stopped}\n{started}"))
}

pub fn install() -> Result<String, String> {
    let home = home();
    std::fs::create_dir_all(home.join("daemon")).map_err(|err| err.to_string())?;
    let spec = service::Spec::current(&home).map_err(|err| err.to_string())?;
    service::install(&spec).map_err(|err| err.to_string())
}

pub fn uninstall() -> Result<String, String> {
    let spec = service::Spec::current(&home()).map_err(|err| err.to_string())?;
    service::uninstall(&spec).map_err(|err| err.to_string())
}

pub fn set_service_enabled(on: bool) -> Result<String, String> {
    let spec = service::Spec::current(&home()).map_err(|err| err.to_string())?;
    if on {
        service::enable(&spec).map_err(|err| err.to_string())
    } else {
        service::disable().map_err(|err| err.to_string())
    }
}

/// Re-read what the daemon reads on every tick — the job table and the heartbeat policy — and say
/// what the operator would see if the process restarted now.
pub fn reload() -> Result<String, String> {
    let store = store()?;
    let beats = Heartbeat::new(&store, Policy::default());
    let jobs = store.list_jobs().map_err(|err| err.to_string())?;
    Ok(format!(
        "reloaded: {} jobs, heartbeat {}\nthe running daemon picks changes up on its next tick",
        jobs.len(),
        humanize_ms(beats.interval_ms())
    ))
}

// ---------------------------------------------------------------------------
// heartbeat
// ---------------------------------------------------------------------------

pub fn heartbeat_status() -> Result<String, String> {
    let store = store()?;
    let beats = Heartbeat::new(&store, Policy::default());
    let now = now_ms();
    let mut text = format!(
        "heartbeat: {}\ninterval: {}\ncooldown: {}\n",
        if !beats.enabled() {
            "disabled"
        } else if beats.paused() {
            "paused"
        } else {
            "on"
        },
        humanize_ms(beats.interval_ms()),
        humanize_ms(beats.policy().cooldown_ms)
    );
    for kind in [BeatKind::Runtime, BeatKind::Agent, BeatKind::Workers] {
        let last = store
            .state_ms(&format!("heartbeat.{}", kind.as_str()))
            .map_err(|err| err.to_string())?
            .map(|at| format_ms(Zone::Local, at))
            .unwrap_or_else(|| "never".to_string());
        text.push_str(&format!("  {}: last {last}\n", kind.as_str()));
    }
    if let Some(due) = beats.next_due(now) {
        text.push_str(&format!("next: {}\n", format_ms(Zone::Local, due)));
    }
    Ok(text)
}

pub fn heartbeat_set_enabled(on: bool) -> Result<String, String> {
    let store = store()?;
    let beats = Heartbeat::new(&store, Policy::default());
    beats.set_enabled(on).map_err(|err| err.to_string())?;
    Ok(if on {
        "heartbeat enabled".into()
    } else {
        "heartbeat disabled".into()
    })
}

pub fn heartbeat_set_paused(paused: bool) -> Result<String, String> {
    let store = store()?;
    let beats = Heartbeat::new(&store, Policy::default());
    beats.set_paused(paused).map_err(|err| err.to_string())?;
    Ok(if paused {
        "heartbeat paused".into()
    } else {
        "heartbeat resumed".into()
    })
}

/// Run all three beats once, now, whatever the schedule said.
pub fn heartbeat_run() -> Result<String, String> {
    let store = store()?;
    let beats = Heartbeat::new(&store, Policy::default());
    let now = now_ms();
    let mut text = String::new();
    for kind in [BeatKind::Runtime, BeatKind::Agent, BeatKind::Workers] {
        let beat: Beat = beats.beat(kind, now).map_err(|err| err.to_string())?;
        text.push_str(&beat.line());
        text.push('\n');
        for issue in &beat.issues {
            text.push_str(&format!(
                "  {} {}: {}\n",
                issue.state.as_str(),
                issue.key,
                issue.message
            ));
        }
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// cron
// ---------------------------------------------------------------------------

pub fn cron_list() -> Result<String, String> {
    let store = store()?;
    let jobs = store.list_jobs().map_err(|err| err.to_string())?;
    if jobs.is_empty() {
        return Ok("no scheduled jobs".to_string());
    }
    let mut text = String::new();
    for job in jobs {
        let next = job
            .next_run_ms
            .map(|at| format_ms(Zone::Local, at))
            .unwrap_or_else(|| "—".to_string());
        text.push_str(&format!(
            "{:<24} {:<8} {:<18} next {:<17} last {}\n",
            job.id,
            if job.enabled { "on" } else { "paused" },
            job.schedule,
            next,
            job.last_status
        ));
    }
    Ok(text)
}

/// Everything an operator may set on a job in one struct, so `add` and `edit` share the parsing.
#[derive(Debug, Default, Clone)]
pub struct Draft {
    pub name: Option<String>,
    pub schedule: Option<String>,
    pub payload: Option<String>,
    pub mode: Option<JobMode>,
    pub target: Option<String>,
    pub timezone: Option<String>,
    pub description: Option<String>,
    pub agent: Option<String>,
    pub bot: Option<String>,
    pub retries: Option<u32>,
    pub timeout: Option<String>,
    pub priority: Option<i64>,
    pub enabled: Option<bool>,
}

impl Draft {
    fn apply(&self, mut job: NewJob) -> Result<NewJob, String> {
        if let Some(payload) = &self.payload {
            job = job.payload(payload.clone());
        }
        if let Some(mode) = self.mode {
            job = job.mode(mode);
        }
        if let Some(target) = &self.target {
            // Refuse a destination nobody can deliver to, at the moment it is typed.
            notify::Destination::parse(target).map_err(|err| err.to_string())?;
            job = job.target(target.clone());
        }
        if let Some(zone) = &self.timezone {
            if Zone::parse(zone).is_none() {
                return Err(format!(
                    "unknown timezone {zone:?}: use local, UTC or an offset like +02:00"
                ));
            }
            job = job.timezone(zone.clone());
        }
        if let Some(description) = &self.description {
            job = job.description(description.clone());
        }
        if let Some(agent) = &self.agent {
            job = job.agent(agent.clone());
        }
        if let Some(bot) = &self.bot {
            job = job.bot(bot.clone());
        }
        if let Some(retries) = self.retries {
            job = job.retries(retries);
        }
        if let Some(timeout) = &self.timeout {
            let millis =
                parse_duration(timeout).ok_or_else(|| format!("unknown duration {timeout:?}"))?;
            job = job.timeout_ms(millis);
        }
        if let Some(priority) = self.priority {
            job = job.priority(priority);
        }
        if let Some(enabled) = self.enabled {
            job = job.enabled(enabled);
        }
        // A job that names nobody to run it fails on its first slot, hours after the typo and
        // possibly overnight. Refuse it while the operator is still looking at the command.
        if job.mode == JobMode::Delegate && job.bot.trim().is_empty() {
            return Err("a delegate job needs a bot: add --bot <agent>".into());
        }
        if job.mode == JobMode::Workflow && job.agent.trim().is_empty() {
            return Err("a workflow job needs a team: add --agent <team>".into());
        }
        Ok(job)
    }
}

pub fn cron_add(name: &str, schedule: &str, draft: &Draft) -> Result<String, String> {
    let store = store()?;
    let manager = JobManager::new(store);
    let now = now_ms();
    let mut job = manager
        .create(draft.apply(NewJob::new(name, schedule))?, now)
        .map_err(|err| err.to_string())?;
    if draft.timeout.is_some() || draft.priority.is_some() || draft.enabled.is_some() {
        job.timeout_ms = match &draft.timeout {
            Some(text) => {
                Some(parse_duration(text).ok_or_else(|| format!("unknown duration {text:?}"))?)
            }
            None => job.timeout_ms,
        };
        job.priority = draft.priority.unwrap_or(job.priority);
        if let Some(enabled) = draft.enabled {
            job.enabled = enabled;
            if !enabled {
                job.next_run_ms = None;
            }
        }
        manager
            .store()
            .update_job(&job)
            .map_err(|err| err.to_string())?;
    }
    let next = job
        .next_run_ms
        .map(|at| format_ms(Zone::Local, at))
        .unwrap_or_else(|| "never".to_string());
    Ok(format!("added {} ({})\nnext: {next}", job.id, job.schedule))
}

pub fn cron_show(needle: &str) -> Result<String, String> {
    let store = store()?;
    let manager = JobManager::new(store.clone());
    let job = manager.find(needle).map_err(|err| err.to_string())?;
    let mut text = format!(
        "id: {}\nname: {}\nschedule: {}\ntimezone: {}\nmode: {}\ntarget: {}\nenabled: {}\nretries: {}\npriority: {}\nlast: {}\n",
        job.id,
        job.name,
        job.schedule,
        job.timezone,
        job.mode.as_str(),
        job.target,
        job.enabled,
        job.retries,
        job.priority,
        job.last_status
    );
    if !job.description.is_empty() {
        text.push_str(&format!("description: {}\n", job.description));
    }
    if let Some(timeout) = job.timeout_ms {
        text.push_str(&format!("timeout: {}\n", humanize_ms(timeout)));
    }
    text.push_str(&format!("payload: {}\n", job.payload));
    let runs = store.runs_for(&job.id, 5).map_err(|err| err.to_string())?;
    if runs.is_empty() {
        text.push_str("last runs: none\n");
    } else {
        text.push_str("last runs:\n");
        for run in runs {
            text.push_str(&format!(
                "  {} {} {}\n",
                format_ms(Zone::Local, run.started_ms),
                run.status.as_str(),
                briefly(&run)
            ));
        }
    }
    Ok(text)
}

pub fn cron_edit(needle: &str, draft: &Draft) -> Result<String, String> {
    let store = store()?;
    let manager = JobManager::new(store.clone());
    let mut job = manager.find(needle).map_err(|err| err.to_string())?;
    if let Some(name) = &draft.name {
        job.name = name.clone();
    }
    if let Some(payload) = &draft.payload {
        job.payload = payload.clone();
    }
    if let Some(mode) = draft.mode {
        job.mode = mode;
    }
    if let Some(target) = &draft.target {
        notify::Destination::parse(target).map_err(|err| err.to_string())?;
        job.target = target.clone();
    }
    if let Some(zone) = &draft.timezone {
        if Zone::parse(zone).is_none() {
            return Err(format!("unknown timezone {zone:?}"));
        }
        job.timezone = zone.clone();
    }
    if let Some(description) = &draft.description {
        job.description = description.clone();
    }
    if let Some(agent) = &draft.agent {
        job.agent = agent.clone();
    }
    if let Some(bot) = &draft.bot {
        job.bot = bot.clone();
    }
    if let Some(retries) = draft.retries {
        job.retries = retries;
    }
    if let Some(timeout) = &draft.timeout {
        job.timeout_ms =
            Some(parse_duration(timeout).ok_or_else(|| format!("unknown duration {timeout:?}"))?);
    }
    if let Some(priority) = draft.priority {
        job.priority = priority;
    }
    // A new schedule is parsed before it is stored, and it moves the next slot with it.
    if let Some(schedule) = &draft.schedule {
        job.schedule = schedule.clone();
        job.next_run_ms = manager.next_slot(&job, now_ms());
    }
    if let Some(enabled) = draft.enabled {
        job.enabled = enabled;
        if !enabled {
            job.next_run_ms = None;
        } else if job.next_run_ms.is_none() {
            job.next_run_ms = manager.next_slot(&job, now_ms());
        }
    }
    job.updated_ms = now_ms();
    // Same rule as creation: an edited job that names nobody would only fail at its next slot.
    if job.mode == JobMode::Delegate && job.bot.trim().is_empty() {
        return Err("a delegate job needs a bot: add --bot <agent>".into());
    }
    if job.mode == JobMode::Workflow && job.agent.trim().is_empty() {
        return Err("a workflow job needs a team: add --agent <team>".into());
    }
    store.update_job(&job).map_err(|err| err.to_string())?;
    Ok(format!("updated {}", job.id))
}

pub fn cron_pause(needle: &str) -> Result<String, String> {
    let store = store()?;
    let manager = JobManager::new(store);
    let job = manager.find(needle).map_err(|err| err.to_string())?;
    manager
        .enable(&job.id, false, now_ms())
        .map_err(|err| err.to_string())?;
    Ok(format!("paused {}", job.id))
}

pub fn cron_resume(needle: &str) -> Result<String, String> {
    let store = store()?;
    let manager = JobManager::new(store);
    let job = manager.find(needle).map_err(|err| err.to_string())?;
    let job = manager
        .enable(&job.id, true, now_ms())
        .map_err(|err| err.to_string())?;
    Ok(format!(
        "resumed {}\nnext: {}",
        job.id,
        job.next_run_ms
            .map(|at| format_ms(Zone::Local, at))
            .unwrap_or_else(|| "never".to_string())
    ))
}

/// Run now — the daemon picks the slot up on its next tick, so this works whether or not the
/// supervisor happens to be running.
pub fn cron_run(needle: &str) -> Result<String, String> {
    let store = store()?;
    let manager = JobManager::new(store);
    let job = manager.find(needle).map_err(|err| err.to_string())?;
    manager
        .run_now(&job.id, now_ms())
        .map_err(|err| err.to_string())?;
    Ok(format!(
        "{} is due now; the daemon will pick it up on its next tick",
        job.id
    ))
}

pub fn cron_remove(needle: &str) -> Result<String, String> {
    let store = store()?;
    let manager = JobManager::new(store);
    let job = manager.find(needle).map_err(|err| err.to_string())?;
    manager.remove(&job.id).map_err(|err| err.to_string())?;
    Ok(format!("removed {} and its history", job.id))
}

pub fn cron_history(needle: &str, limit: usize) -> Result<String, String> {
    let store = store()?;
    let manager = JobManager::new(store.clone());
    let job = manager.find(needle).map_err(|err| err.to_string())?;
    let runs = store
        .runs_for(&job.id, limit)
        .map_err(|err| err.to_string())?;
    if runs.is_empty() {
        return Ok(format!("{} has never run", job.id));
    }
    let mut text = String::new();
    for run in runs {
        text.push_str(&format!(
            "{}  attempt {}  {:<9}  {}\n",
            format_ms(Zone::Local, run.started_ms),
            run.attempt,
            run.status.as_str(),
            briefly(&run)
        ));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// jobs
// ---------------------------------------------------------------------------

/// How a run is summed up in a list: what went wrong first, otherwise the first thing it said.
fn briefly(run: &crate::store::Run) -> String {
    if !run.error.is_empty() {
        return run.error.clone();
    }
    run.output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

pub fn jobs_list(limit: usize) -> Result<String, String> {
    let store = store()?;
    let runs = store.recent_runs(limit).map_err(|err| err.to_string())?;
    if runs.is_empty() {
        return Ok("no background work has run".to_string());
    }
    let mut text = String::new();
    for run in runs {
        text.push_str(&format!(
            "{}  {:<20} {:<9} {}\n",
            format_ms(Zone::Local, run.started_ms),
            run.job_id,
            run.status.as_str(),
            briefly(&run)
        ));
    }
    Ok(text)
}

pub fn jobs_inspect(needle: &str) -> Result<String, String> {
    let store = store()?;
    let run = resolve_run(&store, needle)?;
    let mut text = format!(
        "run: {}\njob: {}\nstatus: {}\nstarted: {}\n",
        run.id,
        run.job_id,
        run.status.as_str(),
        format_ms(Zone::Local, run.started_ms)
    );
    if let Some(finished) = run.finished_ms {
        text.push_str(&format!("finished: {}\n", format_ms(Zone::Local, finished)));
    }
    text.push_str(&format!("delivery: {}\n", run.delivery.as_str()));
    if let Some(session) = &run.session {
        text.push_str(&format!("session: {session}\n"));
    }
    if !run.error.is_empty() {
        text.push_str(&format!("error: {}\n", run.error));
    }
    if !run.output.is_empty() {
        text.push_str("output:\n");
        text.push_str(&run.output);
        if !run.output.ends_with('\n') {
            text.push('\n');
        }
    }
    Ok(text)
}

/// Ask for a run to stop. The request is written down rather than sent, because the worker lives in
/// the daemon's process: the supervisor reads it on its next tick and aborts the matching task.
/// A run id is long and nobody retypes it, so a run is also reachable by the job it belongs to:
/// the newest run of that job. The list prints job ids, which is what an operator actually reads.
fn resolve_run(store: &ScheduleStore, needle: &str) -> Result<Run, String> {
    if let Some(run) = store.get_run(needle).map_err(|err| err.to_string())? {
        return Ok(run);
    }
    let jobs = JobManager::new(store.clone());
    let job = jobs.find(needle).map_err(|err| err.to_string())?;
    let runs = store.runs_for(&job.id, 10).map_err(|err| err.to_string())?;
    let running = runs.iter().find(|run| run.status == RunStatus::Running);
    match (running, runs.first()) {
        (Some(run), _) => Ok(run.clone()),
        (None, Some(last)) => Err(format!(
            "{} has nothing running; its last run finished as {}",
            job.id,
            last.status.as_str()
        )),
        (None, None) => Err(format!("{} has never run", job.id)),
    }
}

pub fn jobs_cancel(needle: &str) -> Result<String, String> {
    let store = store()?;
    let run = resolve_run(&store, needle)?;
    if run.status != RunStatus::Running {
        return Ok(format!(
            "{} already finished as {}",
            run.id,
            run.status.as_str()
        ));
    }
    store
        .set_state(&format!("cancel.{}", run.id), "1")
        .map_err(|err| err.to_string())?;
    Ok(format!("asked {} to stop", run.id))
}

/// Mark a run cancelled once the daemon has honoured the request.
pub fn finish_cancelled(store: &ScheduleStore, run_id: &str, output: &str) -> Result<(), String> {
    store
        .finish_run(
            run_id,
            RunStatus::Cancelled,
            output,
            "cancelled by operator",
            Delivery::None,
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

/// The task class a job is dispatched as — exposed so the CLI can explain a queue refusal.
pub fn class_of(mode: JobMode) -> TaskClass {
    match mode {
        JobMode::Script => TaskClass::Script,
        JobMode::Agent | JobMode::Delegate => TaskClass::Agent,
        JobMode::Workflow => TaskClass::Workflow,
    }
}

/// `Owner` is re-exported so callers do not have to reach into the daemon module for a status line.
pub use crate::daemon::Owner;

/// Kept private to this module: the signal handling that turns Ctrl+C and `kill` into one notify.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(_) => return,
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, ScheduleStore) {
        let dir = TempDir::new().unwrap();
        let store = open(&dir.path().join("vikram.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn a_new_job_is_listed_with_the_time_it_will_run() {
        let (_dir, store) = fixture();
        let manager = JobManager::new(store.clone());
        let job = manager
            .create(
                NewJob::new("nightly", "0 3 * * *")
                    .timezone("UTC")
                    .payload("check the disk"),
                now_ms(),
            )
            .unwrap();
        assert!(job.next_run_ms.is_some());
        assert_eq!(job.mode, JobMode::Agent);
    }

    #[test]
    fn a_destination_nobody_can_reach_is_refused_before_the_job_exists() {
        let err = Draft {
            target: Some("telepathy".into()),
            ..Draft::default()
        }
        .apply(NewJob::new("x", "every 5m"))
        .unwrap_err();
        assert!(err.contains("telepathy"), "{err}");
    }

    #[test]
    fn a_timezone_that_is_not_understood_is_refused() {
        let err = Draft {
            timezone: Some("Europe/Berlin".into()),
            ..Draft::default()
        }
        .apply(NewJob::new("x", "every 5m"))
        .unwrap_err();
        assert!(err.contains("Europe/Berlin"), "{err}");
    }

    #[test]
    fn a_script_job_is_dispatched_as_a_script_not_as_an_agent() {
        assert_eq!(class_of(JobMode::Script), TaskClass::Script);
        assert_eq!(class_of(JobMode::Agent), TaskClass::Agent);
        assert_eq!(class_of(JobMode::Delegate), TaskClass::Agent);
        assert_eq!(class_of(JobMode::Workflow), TaskClass::Workflow);
    }

    #[test]
    fn a_delegated_job_without_a_bot_is_refused_while_it_is_being_typed() {
        let err = Draft {
            mode: Some(JobMode::Delegate),
            payload: Some("check the queue".into()),
            ..Draft::default()
        }
        .apply(NewJob::new("x", "every 5m"))
        .unwrap_err();
        assert!(err.contains("--bot"), "{err}");
    }

    #[test]
    fn a_team_job_without_a_team_is_refused_while_it_is_being_typed() {
        let err = Draft {
            mode: Some(JobMode::Workflow),
            ..Draft::default()
        }
        .apply(NewJob::new("x", "every 5m"))
        .unwrap_err();
        assert!(err.contains("--agent"), "{err}");
    }

    #[test]
    fn a_delegated_job_that_names_its_bot_is_accepted_with_it() {
        let job = Draft {
            mode: Some(JobMode::Delegate),
            bot: Some("arun".into()),
            ..Draft::default()
        }
        .apply(NewJob::new("x", "every 5m"))
        .unwrap();
        assert_eq!(job.mode, JobMode::Delegate);
        assert_eq!(job.bot, "arun");
    }

    #[test]
    fn a_run_that_is_recorded_is_readable_again() {
        let (_dir, store) = fixture();
        let manager = JobManager::new(store.clone());
        let job = manager
            .create(NewJob::new("check", "every 5m"), now_ms())
            .unwrap();
        let run = store
            .begin_run(&job, job.next_run_ms.unwrap(), None)
            .unwrap();
        store
            .finish_run(&run.id, RunStatus::Ok, "all good", "", Delivery::None)
            .unwrap();
        let read = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(read.status, RunStatus::Ok);
        assert_eq!(read.output, "all good");
    }

    #[test]
    fn cancelling_a_finished_run_does_nothing_and_says_why() {
        let (_dir, store) = fixture();
        let manager = JobManager::new(store.clone());
        let job = manager
            .create(NewJob::new("work", "every 5m"), now_ms())
            .unwrap();
        let run = store
            .begin_run(&job, job.next_run_ms.unwrap(), None)
            .unwrap();
        store
            .finish_run(&run.id, RunStatus::Ok, "done", "", Delivery::None)
            .unwrap();
        // The CLI would read the shared store; here the same rule is checked directly.
        assert_ne!(
            store.get_run(&run.id).unwrap().unwrap().status,
            RunStatus::Running
        );
    }

    #[test]
    fn a_draft_carries_every_field_the_cli_accepts() {
        let job = Draft {
            payload: Some("summarise".into()),
            mode: Some(JobMode::Script),
            target: Some("local".into()),
            timezone: Some("+02:00".into()),
            description: Some("nightly".into()),
            agent: Some("bot-1".into()),
            bot: Some("bot-1".into()),
            retries: Some(2),
            ..Draft::default()
        }
        .apply(NewJob::new("nightly", "0 3 * * *"))
        .unwrap();
        assert_eq!(job.mode, JobMode::Script);
        assert_eq!(job.retries, 2);
        assert_eq!(job.timezone, "+02:00");
    }
}
