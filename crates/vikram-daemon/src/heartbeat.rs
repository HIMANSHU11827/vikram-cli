//! The heartbeat: what the background runtime looks at when it is not doing anything else.
//!
//! There are three beats, and they answer three different questions:
//!
//! * [`BeatKind::Runtime`] — is the machine that runs the work healthy at all?
//! * [`BeatKind::Agent`] — is there something the main agent should be told about?
//! * [`BeatKind::Workers`] — is the work itself making progress, or is something stuck?
//!
//! They share one ticker and one set of rules ([`Policy`]) because the operator should not have to
//! configure three cooldowns. What they do not share is meaning: a runtime beat that reports a
//! healthy daemon is not the same claim as an agent beat that reports nothing to say.
//!
//! Two rules keep a heartbeat from becoming noise. A tick costs no model call — every check here is
//! a read against the store — and an issue is only raised once per cooldown, keyed by what is wrong
//! rather than by when it was noticed.

use serde::{Deserialize, Serialize};

use crate::store::{RunStatus, ScheduleStore, StoreError};
use crate::time::{self, Millis, Zone};

/// How bad things are, in the four words the operator sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// Everything checked out.
    Healthy,
    /// Something is late or retried and will probably fix itself.
    Warning,
    /// Something is wrong that the operator will have to look at.
    Degraded,
    /// The thing the beat is about is not running at all.
    Failed,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Healthy => "healthy",
            Health::Warning => "warning",
            Health::Degraded => "degraded",
            Health::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "healthy" | "ok" | "good" => Some(Health::Healthy),
            "warning" | "warn" | "late" => Some(Health::Warning),
            "degraded" | "degrad" => Some(Health::Degraded),
            "failed" | "fail" | "down" => Some(Health::Failed),
            _ => None,
        }
    }

    /// The worse of two states, because a beat reports its worst finding.
    pub fn worse(self, other: Health) -> Health {
        if other > self { other } else { self }
    }
}

impl std::fmt::Display for Health {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Which of the three beats produced a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeatKind {
    Runtime,
    Agent,
    Workers,
}

impl BeatKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BeatKind::Runtime => "runtime",
            BeatKind::Agent => "agent",
            BeatKind::Workers => "workers",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "runtime" | "daemon" | "system" => Some(BeatKind::Runtime),
            "agent" | "awareness" => Some(BeatKind::Agent),
            "workers" | "worker" | "jobs" => Some(BeatKind::Workers),
            _ => None,
        }
    }

    /// The store key that remembers when this beat last ran.
    fn last_key(self) -> String {
        format!("heartbeat.{}", self.as_str())
    }
}

/// One thing that is wrong, named so that noticing it twice in a row is not two things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Stable identity of the problem: the job id, never the timestamp.
    pub key: String,
    pub state: Health,
    pub message: String,
}

impl Issue {
    pub fn new(key: impl Into<String>, state: Health, message: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            state,
            message: message.into(),
        }
    }
}

/// The rules every beat obeys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// How often each beat runs.
    pub interval_ms: Millis,
    /// How long the same issue stays quiet after it has been reported.
    pub cooldown_ms: Millis,
    /// Hours of the local day during which nothing is delivered, as `(start, end)` in 24h form.
    /// A window that starts later than it ends wraps past midnight.
    pub quiet: Option<(u32, u32)>,
    /// A run that has held its slot for longer than this is treated as stuck.
    pub stuck_after_ms: Millis,
    /// A job whose next slot is this far in the past is late, not merely due.
    pub late_after_ms: Millis,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            interval_ms: 60_000,
            cooldown_ms: 30 * 60_000,
            quiet: None,
            stuck_after_ms: 60 * 60_000,
            late_after_ms: 15 * 60_000,
        }
    }
}

impl Policy {
    pub fn every(interval_ms: Millis) -> Self {
        Self {
            interval_ms: interval_ms.max(1_000),
            ..Self::default()
        }
    }

    pub fn with_cooldown(mut self, cooldown_ms: Millis) -> Self {
        self.cooldown_ms = cooldown_ms;
        self
    }

    pub fn with_quiet_hours(mut self, quiet: Option<(u32, u32)>) -> Self {
        self.quiet = quiet;
        self
    }

    pub fn with_stuck_after(mut self, stuck_after_ms: Millis) -> Self {
        self.stuck_after_ms = stuck_after_ms;
        self
    }

    /// Whether the local clock is inside the quiet window. Quiet hours stop delivery, never the
    /// checks: a stuck job at 03:00 is still recorded as stuck.
    pub fn is_quiet(&self, now: Millis) -> bool {
        let Some((start, end)) = self.quiet else {
            return false;
        };
        if start == end {
            return false;
        }
        let hour = time::civil_in(Zone::Local, now).hour;
        if start < end {
            hour >= start && hour < end
        } else {
            hour >= start || hour < end
        }
    }
}

/// What one beat found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beat {
    pub kind: BeatKind,
    pub at: Millis,
    /// The worst state any check reached.
    pub state: Health,
    /// Everything the beat found, whether or not it was delivered.
    pub issues: Vec<Issue>,
    /// The issues that passed the cooldown and are worth telling the operator about.
    pub notify: Vec<Issue>,
}

impl Beat {
    pub fn is_healthy(&self) -> bool {
        self.state == Health::Healthy
    }

    /// One line an operator reads in `vikram heartbeat status`.
    pub fn line(&self) -> String {
        let stamp = time::format_ms(Zone::Local, self.at);
        if self.issues.is_empty() {
            return format!("{stamp}  {}  {}", self.kind.as_str(), self.state.as_str());
        }
        let first = self
            .issues
            .first()
            .map(|issue| issue.message.clone())
            .unwrap_or_default();
        let more = self.issues.len().saturating_sub(1);
        let tail = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        format!(
            "{stamp}  {}  {}  {first}{tail}",
            self.kind.as_str(),
            self.state.as_str()
        )
    }
}

/// The beat itself: a store to read, and the rules to read it by.
pub struct Heartbeat<'a> {
    store: &'a ScheduleStore,
    policy: Policy,
}

impl<'a> Heartbeat<'a> {
    pub fn new(store: &'a ScheduleStore, policy: Policy) -> Self {
        Self { store, policy }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn set_policy(&mut self, policy: Policy) {
        self.policy = policy;
    }

    /// Whether the operator has paused the heartbeat. Paused means no beats run; it does not mean
    /// the ticks are quietly skipped and then reported as healthy.
    pub fn paused(&self) -> bool {
        self.store
            .state_bool("heartbeat.paused", false)
            .unwrap_or(false)
    }

    pub fn set_paused(&self, paused: bool) -> Result<(), StoreError> {
        self.store
            .set_state("heartbeat.paused", if paused { "1" } else { "0" })
    }

    pub fn enabled(&self) -> bool {
        self.store
            .state_bool("heartbeat.enabled", true)
            .unwrap_or(true)
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<(), StoreError> {
        self.store
            .set_state("heartbeat.enabled", if enabled { "1" } else { "0" })
    }

    pub fn interval_ms(&self) -> Millis {
        self.store
            .state_ms("heartbeat.interval_ms")
            .ok()
            .flatten()
            .unwrap_or(self.policy.interval_ms)
    }

    pub fn set_interval_ms(&self, interval_ms: Millis) -> Result<(), StoreError> {
        self.store
            .set_state_ms("heartbeat.interval_ms", interval_ms.max(1_000))
    }

    /// When the next beat is due, or `None` when the heartbeat is off.
    pub fn next_due(&self, now: Millis) -> Option<Millis> {
        if !self.enabled() || self.paused() {
            return None;
        }
        let last = self
            .store
            .state_ms(&BeatKind::Runtime.last_key())
            .ok()
            .flatten();
        Some(match last {
            Some(last) => last + self.interval_ms(),
            None => now,
        })
    }

    /// Record that a beat ran, so `next_due` advances.
    pub fn stamp(&self, kind: BeatKind, at: Millis) -> Result<(), StoreError> {
        self.store.set_state_ms(&kind.last_key(), at)?;
        if kind == BeatKind::Runtime {
            self.store.set_state_ms("heartbeat.last_ms", at)?;
            self.store
                .set_state_ms("heartbeat.next_ms", at + self.interval_ms())?;
        }
        Ok(())
    }

    /// The runtime beat: is the machine healthy. Cheap reads only.
    pub fn runtime(&self, now: Millis) -> Result<Beat, StoreError> {
        let mut issues = Vec::new();
        if self.store.list_jobs().is_err() {
            issues.push(Issue::new(
                "store.unreadable",
                Health::Failed,
                "the job store cannot be read",
            ));
        }

        for job in self.store.due_jobs(now)? {
            let Some(next) = job.next_run_ms else {
                continue;
            };
            let late = now - next;
            if late > self.policy.late_after_ms {
                issues.push(Issue::new(
                    format!("job.late.{}", job.id),
                    Health::Warning,
                    format!("{} is {} late", job.name, time::humanize_ms(late)),
                ));
            }
        }

        let stuck = self.stuck_runs(now)?;
        for run in stuck {
            issues.push(Issue::new(
                format!("run.stuck.{}", run.job_id),
                Health::Degraded,
                format!(
                    "{} has been running for {}",
                    run.job_id,
                    time::humanize_ms(now - run.started_ms)
                ),
            ));
        }

        Ok(self.finish(BeatKind::Runtime, now, issues))
    }

    /// The agent beat: is there anything the main agent should be told. This is the only beat whose
    /// findings are worth waking a conversation for, and even then only outside quiet hours.
    pub fn agent(&self, now: Millis) -> Result<Beat, StoreError> {
        let mut issues = Vec::new();
        let runs = self.store.recent_runs(200)?;
        let failures: Vec<_> = runs
            .iter()
            .filter(|run| run.status == RunStatus::Failed && run.finished_ms.is_some())
            .collect();

        for job in self.store.list_jobs()? {
            let mine: Vec<_> = failures.iter().filter(|run| run.job_id == job.id).collect();
            if mine.len() >= 3 {
                issues.push(Issue::new(
                    format!("job.failing.{}", job.id),
                    Health::Degraded,
                    format!("{} has failed {} times in a row", job.name, mine.len()),
                ));
            }
            if let Some(run) = mine.first()
                && run.delivery != crate::store::Delivery::Delivered
                && run.delivery != crate::store::Delivery::None
            {
                issues.push(Issue::new(
                    format!("delivery.{}", job.id),
                    Health::Warning,
                    format!("{} finished but its report was not delivered", job.name),
                ));
            }
            if job.last_status == "skipped" {
                issues.push(Issue::new(
                    format!("job.skipped.{}", job.id),
                    Health::Warning,
                    format!("{} was skipped because the scheduler was away", job.name),
                ));
            }
        }

        Ok(self.finish(BeatKind::Agent, now, issues))
    }

    /// The workers beat: is the work moving.
    pub fn workers(&self, now: Millis) -> Result<Beat, StoreError> {
        let running: Vec<_> = self
            .store
            .recent_runs(200)?
            .into_iter()
            .filter(|run| run.status == RunStatus::Running)
            .collect();
        let mut issues = Vec::new();
        if running.len() > 32 {
            issues.push(Issue::new(
                "workers.saturated",
                Health::Warning,
                format!("{} runs are in flight at once", running.len()),
            ));
        }
        for run in running {
            let held = now - run.started_ms;
            if held > self.policy.stuck_after_ms {
                issues.push(Issue::new(
                    format!("worker.stuck.{}", run.job_id),
                    Health::Degraded,
                    format!(
                        "{} has occupied a worker for {}",
                        run.job_id,
                        time::humanize_ms(held)
                    ),
                ));
            }
        }
        Ok(self.finish(BeatKind::Workers, now, issues))
    }

    /// Beat `kind`, stamp it, and remember what was reported so the same issue stays quiet.
    pub fn beat(&self, kind: BeatKind, now: Millis) -> Result<Beat, StoreError> {
        let mut beat = match kind {
            BeatKind::Runtime => self.runtime(now)?,
            BeatKind::Agent => self.agent(now)?,
            BeatKind::Workers => self.workers(now)?,
        };
        self.stamp(kind, now)?;
        Ok(match beat {
            ref mut beat => {
                beat.notify = self.release(beat.issues.clone(), now)?;
                beat.clone()
            }
        })
    }

    /// Which of these issues are past their cooldown. An issue that is no longer present is
    /// forgotten, so a problem that comes back is reported again rather than staying silent.
    pub fn release(&self, issues: Vec<Issue>, now: Millis) -> Result<Vec<Issue>, StoreError> {
        let mut send = Vec::new();
        for issue in issues {
            let key = format!("heartbeat.reported.{}", issue.key);
            let last = self.store.state_ms(&key).ok().flatten();
            let quiet = last.is_some_and(|last| now - last < self.policy.cooldown_ms);
            if quiet {
                continue;
            }
            self.store.set_state_ms(&key, now)?;
            send.push(issue);
        }
        Ok(send)
    }

    /// A run that is still marked running long after it should have ended.
    fn stuck_runs(&self, now: Millis) -> Result<Vec<crate::store::Run>, StoreError> {
        Ok(self
            .store
            .recent_runs(200)?
            .into_iter()
            .filter(|run| {
                run.status == RunStatus::Running
                    && now - run.started_ms > self.policy.stuck_after_ms
            })
            .collect())
    }

    fn finish(&self, kind: BeatKind, now: Millis, issues: Vec<Issue>) -> Beat {
        let state = issues
            .iter()
            .fold(Health::Healthy, |worst, issue| worst.worse(issue.state));
        Beat {
            kind,
            at: now,
            state,
            issues,
            notify: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{JobManager, NewJob};
    use crate::store::ScheduleStore;

    fn fixture() -> (ScheduleStore, JobManager) {
        let store = ScheduleStore::open_in_memory().unwrap();
        store.ensure_schema().unwrap();
        let manager = JobManager::new(store.clone());
        (store, manager)
    }

    fn minute(n: i64) -> Millis {
        1_700_000_000_000 + n * 60_000
    }

    #[test]
    fn a_quiet_system_reports_healthy_and_says_nothing() {
        let (store, _) = fixture();
        let beat = Heartbeat::new(&store, Policy::default())
            .runtime(minute(0))
            .unwrap();
        assert_eq!(beat.state, Health::Healthy);
        assert!(beat.issues.is_empty());
        assert!(beat.notify.is_empty());
        assert!(beat.line().contains("healthy"));
    }

    #[test]
    fn a_late_job_is_named_once_and_then_held_quiet() {
        let (store, manager) = fixture();
        let now = minute(0);
        let mut job = manager
            .create(NewJob::new("nightly report", "every 10m"), now)
            .unwrap();
        job.next_run_ms = Some(now - 40 * 60_000);
        store.update_job(&job).unwrap();

        let beat = Heartbeat::new(&store, Policy::default());
        let first = beat.beat(BeatKind::Runtime, now).unwrap();
        assert_eq!(first.state, Health::Warning);
        assert_eq!(first.notify.len(), 1);
        assert!(first.notify[0].message.contains("nightly report"));

        // The same late job, noticed again a minute later, must not be reported again.
        let second = beat.beat(BeatKind::Runtime, now + 60_000).unwrap();
        assert_eq!(second.issues.len(), 1);
        assert!(second.notify.is_empty(), "an old problem is not news");
    }

    #[test]
    fn the_same_issue_comes_back_after_the_cooldown() {
        let (store, manager) = fixture();
        let now = minute(0);
        let mut job = manager
            .create(NewJob::new("nightly report", "every 10m"), now)
            .unwrap();
        job.next_run_ms = Some(now - 40 * 60_000);
        store.update_job(&job).unwrap();
        let beat = Heartbeat::new(&store, Policy::default().with_cooldown(5 * 60_000));
        assert_eq!(beat.beat(BeatKind::Runtime, now).unwrap().notify.len(), 1);
        assert!(
            beat.beat(BeatKind::Runtime, now + 6 * 60_000)
                .unwrap()
                .notify
                .len()
                == 1
        );
    }

    #[test]
    fn quiet_hours_stop_delivery_but_never_the_check() {
        let policy = Policy::default().with_quiet_hours(Some((22, 7)));
        let evening = time::instant_of(Zone::Local, time::Civil::new(2026, 11, 4, 23, 0, 0));
        let noon = time::instant_of(Zone::Local, time::Civil::new(2026, 11, 4, 12, 0, 0));
        assert!(policy.is_quiet(evening));
        assert!(!policy.is_quiet(noon));
    }

    #[test]
    fn a_window_that_wraps_past_midnight_still_ends() {
        let policy = Policy::default().with_quiet_hours(Some((23, 1)));
        let midnight = time::instant_of(Zone::Local, time::Civil::new(2026, 11, 4, 0, 30, 0));
        let morning = time::instant_of(Zone::Local, time::Civil::new(2026, 11, 4, 2, 0, 0));
        assert!(policy.is_quiet(midnight));
        assert!(!policy.is_quiet(morning));
    }

    #[test]
    fn a_stuck_run_is_degraded_not_failed() {
        let (store, manager) = fixture();
        // A run stamps itself with the wall clock, so the beat is asked about a moment far enough
        // in the future that the run has plainly overrun.
        let now = crate::time::now_ms();
        let job = manager
            .create(NewJob::new("long job", "every 10m"), now)
            .unwrap();
        store.begin_run(&job, now, None).unwrap();
        let beat = Heartbeat::new(&store, Policy::default().with_stuck_after(60_000));
        let worker = beat.workers(now + 90 * 60_000).unwrap();
        assert_eq!(worker.state, Health::Degraded);
        assert!(worker.issues[0].message.contains("occupied a worker"));
    }

    #[test]
    fn a_job_that_keeps_failing_is_what_wakes_the_agent() {
        let (store, manager) = fixture();
        let now = minute(0);
        let job = manager
            .create(NewJob::new("flaky job", "every 10m"), now)
            .unwrap();
        for attempt in 0..3 {
            let run = store.begin_run(&job, now - attempt * 60_000, None).unwrap();
            store
                .finish_run(
                    &run.id,
                    RunStatus::Failed,
                    "",
                    "it broke",
                    crate::store::Delivery::Failed,
                )
                .unwrap();
        }
        let beat = Heartbeat::new(&store, Policy::default())
            .agent(now)
            .unwrap();
        assert_eq!(beat.state, Health::Degraded);
        assert!(
            beat.issues
                .iter()
                .any(|issue| issue.message.contains("failed 3 times")),
            "{:?}",
            beat.issues
        );
    }

    #[test]
    fn pausing_stops_the_next_beat_from_being_due() {
        let (store, _) = fixture();
        let beat = Heartbeat::new(&store, Policy::default());
        assert_eq!(beat.next_due(minute(0)), Some(minute(0)));
        beat.set_paused(true).unwrap();
        assert_eq!(beat.next_due(minute(5)), None);
        beat.set_paused(false).unwrap();
        assert_eq!(beat.next_due(minute(5)), Some(minute(5)));
    }

    #[test]
    fn the_interval_is_the_one_the_operator_set() {
        let (store, _) = fixture();
        let beat = Heartbeat::new(&store, Policy::default());
        beat.set_interval_ms(5 * 60_000).unwrap();
        beat.stamp(BeatKind::Runtime, minute(0)).unwrap();
        assert_eq!(beat.next_due(minute(0)), Some(minute(5)));
        assert_eq!(beat.interval_ms(), 5 * 60_000);
    }

    #[test]
    fn an_interval_can_never_be_zero() {
        let (store, _) = fixture();
        let beat = Heartbeat::new(&store, Policy::default());
        beat.set_interval_ms(0).unwrap();
        assert_eq!(beat.interval_ms(), 1_000);
    }

    #[test]
    fn three_beats_are_three_answers() {
        let (store, manager) = fixture();
        let now = minute(0);
        manager
            .create(NewJob::new("nightly report", "every 10m"), now)
            .unwrap();
        let beat = Heartbeat::new(&store, Policy::default());
        assert_eq!(beat.runtime(now).unwrap().kind, BeatKind::Runtime);
        assert_eq!(beat.agent(now).unwrap().kind, BeatKind::Agent);
        assert_eq!(beat.workers(now).unwrap().kind, BeatKind::Workers);
        assert!(BeatKind::parse("awareness") == Some(BeatKind::Agent));
        assert!(Health::parse("degraded") == Some(Health::Degraded));
        assert!(Health::Warning.worse(Health::Failed) == Health::Failed);
    }
}
