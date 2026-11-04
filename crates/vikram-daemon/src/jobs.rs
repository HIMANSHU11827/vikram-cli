//! Jobs: what is scheduled, when it next runs, and what happened when it did.
//!
//! This layer owns the decisions a store cannot make — what the next slot of a schedule is, whether
//! an overdue job is still worth running, when a retry comes back — and leaves every write to
//! [`ScheduleStore`]. Nothing here runs anything: the worker does that, and the daemon decides when
//! to hand it work.
//!
//! Two rules shape the whole file:
//!
//! * **A slot runs at most once.** Claiming moves `next_run_ms` forward in a single compare-and-set,
//!   so two ticks (or two daemons that think they are alone) cannot both take the same slot.
//! * **A retry is a new slot, not a replay.** The run rows keep the failed attempt and the retry both,
//!   which is the honest record; re-running the same slot under the same key would hide the failure.

use crate::cron::{Schedule, ScheduleError};
use crate::store::{Delivery, Job, JobMode, Run, RunStatus, ScheduleStore, StoreError};
use crate::time::{Millis, Zone};

/// A job as the operator describes it, before it has an id or a next wake-up.
#[derive(Debug, Clone, Default)]
pub struct NewJob {
    pub name: String,
    pub description: String,
    pub schedule: String,
    pub timezone: String,
    pub mode: JobMode,
    pub payload: String,
    pub target: String,
    pub agent: String,
    pub bot: String,
    pub timeout_ms: Option<Millis>,
    pub retries: u32,
    pub priority: i64,
    /// How a team job runs its members: `parallel` or `sequential`.
    pub order: String,
    pub enabled: bool,
}

impl NewJob {
    pub fn new(name: impl Into<String>, schedule: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            schedule: schedule.into(),
            timezone: "local".into(),
            mode: JobMode::Agent,
            target: "local".into(),
            order: "parallel".into(),
            enabled: true,
            ..Self::default()
        }
    }

    pub fn payload(mut self, payload: impl Into<String>) -> Self {
        self.payload = payload.into();
        self
    }

    pub fn mode(mut self, mode: JobMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = target.into();
        self
    }

    pub fn timezone(mut self, timezone: impl Into<String>) -> Self {
        self.timezone = timezone.into();
        self
    }

    pub fn retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = agent.into();
        self
    }

    pub fn bot(mut self, bot: impl Into<String>) -> Self {
        self.bot = bot.into();
        self
    }

    pub fn timeout_ms(mut self, timeout: Millis) -> Self {
        self.timeout_ms = Some(timeout);
        self
    }

    pub fn priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn order(mut self, order: impl Into<String>) -> Self {
        self.order = order.into();
        self
    }
}

/// What settling a run changed, so the daemon can say it out loud instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// When the retry was scheduled for, if one was.
    pub retry_at: Option<Millis>,
    /// True when the job has nothing left to do and was switched off.
    pub disabled: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("schedule: {0}")]
    Schedule(#[from] ScheduleError),
    #[error("no job matches `{0}`")]
    Missing(String),
    #[error("`{0}` matches more than one job: {1}")]
    Ambiguous(String, String),
    #[error("a one-time job cannot be scheduled in the past")]
    PastDeadline,
}

/// A claimed slot: the job as it now stands, the run row that records the attempt, and the slot the
/// run is for.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub job: Job,
    pub run: Run,
    pub slot: Millis,
}

pub struct JobManager {
    store: ScheduleStore,
}

impl JobManager {
    pub fn new(store: ScheduleStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &ScheduleStore {
        &self.store
    }

    /// The schedule a job carries, resolved against its own timezone.
    pub fn schedule_of(&self, job: &Job, now: Millis) -> Result<Schedule, ManagerError> {
        let zone = Zone::parse(&job.timezone).unwrap_or(Zone::Local);
        Ok(Schedule::parse(&job.schedule, zone, now)?)
    }

    /// The first slot strictly after `after`, or `None` when the job will never run again.
    pub fn next_slot(&self, job: &Job, after: Millis) -> Option<Millis> {
        self.schedule_of(job, after).ok()?.next_after(after)
    }

    pub fn create(&self, draft: NewJob, now: Millis) -> Result<Job, ManagerError> {
        if draft.name.trim().is_empty() {
            return Err(ManagerError::Missing("a job needs a name".into()));
        }
        let mut job = Job::new(
            id_for(&draft.name, now),
            draft.name.trim().to_string(),
            draft.schedule.trim().to_string(),
        );
        job.description = draft.description;
        job.timezone = if draft.timezone.trim().is_empty() {
            "local".into()
        } else {
            draft.timezone
        };
        job.mode = draft.mode;
        job.payload = draft.payload;
        job.target = draft.target;
        job.agent = draft.agent;
        job.bot = draft.bot;
        job.timeout_ms = draft.timeout_ms;
        job.retries = draft.retries;
        job.priority = draft.priority;
        job.order = if draft.order.trim().is_empty() {
            "parallel".into()
        } else {
            draft.order
        };
        job.enabled = draft.enabled;
        job.created_ms = now;
        job.updated_ms = now;

        // Parsing before inserting means a typo is refused now, not discovered by a scheduler that
        // silently never fires.
        let mut schedule = self.schedule_of(&job, now)?;
        // `in 30m` is a delay from the moment it was typed, so it is frozen into an absolute instant
        // here. Left relative, every later read would push the job another 30 minutes away.
        if let Schedule::Once { at } = schedule
            && relative(&job.schedule)
        {
            job.schedule = format!("at {at}");
            schedule = Schedule::Once { at };
        }
        job.next_run_ms = match schedule.next_after(now) {
            Some(at) => Some(at),
            None => return Err(ManagerError::PastDeadline),
        };
        job.last_status = "never".into();
        self.store.insert_job(&job)?;
        Ok(job)
    }

    pub fn list(&self) -> Result<Vec<Job>, ManagerError> {
        Ok(self.store.list_jobs()?)
    }

    /// Find a job by id, by exact name, or by a prefix of either — the way an operator types it.
    pub fn find(&self, needle: &str) -> Result<Job, ManagerError> {
        let needle = needle.trim();
        if let Some(job) = self.store.get_job(needle)? {
            return Ok(job);
        }
        let jobs = self.store.list_jobs()?;
        if let Some(job) = jobs.iter().find(|job| job.name == needle) {
            return Ok(job.clone());
        }
        let matches: Vec<&Job> = jobs
            .iter()
            .filter(|job| job.name.starts_with(needle) || job.id.starts_with(needle))
            .collect();
        match matches.len() {
            0 => Err(ManagerError::Missing(needle.to_string())),
            1 => Ok(matches[0].clone()),
            _ => Err(ManagerError::Ambiguous(
                needle.to_string(),
                matches
                    .iter()
                    .map(|job| job.name.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            )),
        }
    }

    pub fn enable(&self, needle: &str, on: bool, now: Millis) -> Result<Job, ManagerError> {
        let mut job = self.find(needle)?;
        // Re-enabling recomputes the wake-up: a job paused for a week must not fire the moment it is
        // switched back on.
        if on {
            job.next_run_ms = self.next_slot(&job, now);
        }
        self.store.set_enabled(&job.id, on, now)?;
        job.enabled = on;
        job.updated_ms = now;
        Ok(job)
    }

    /// Bring the next run forward to now without disturbing the schedule.
    pub fn run_now(&self, needle: &str, now: Millis) -> Result<Job, ManagerError> {
        let mut job = self.find(needle)?;
        self.store.set_next_run(&job.id, Some(now), now)?;
        self.store.set_enabled(&job.id, true, now)?;
        job.next_run_ms = Some(now);
        job.enabled = true;
        job.updated_ms = now;
        Ok(job)
    }

    pub fn remove(&self, needle: &str) -> Result<Job, ManagerError> {
        let job = self.find(needle)?;
        self.store.delete_job(&job.id)?;
        Ok(job)
    }

    pub fn due(&self, now: Millis) -> Result<Vec<Job>, ManagerError> {
        Ok(self.store.due_jobs(now)?)
    }

    /// Take a job's slot. Returns `None` when another tick got there first, which is how a duplicate
    /// run is prevented without a lock around the whole scheduler.
    pub fn claim(
        &self,
        job: &Job,
        slot: Millis,
        now: Millis,
    ) -> Result<Option<Claimed>, ManagerError> {
        let next = self.next_slot(job, slot);
        if !self.store.claim(&job.id, slot, next)? {
            return Ok(None);
        }
        let run = self.store.begin_run(job, slot, None)?;
        let mut job = job.clone();
        job.next_run_ms = next;
        job.last_run_ms = Some(slot);
        job.updated_ms = now;
        Ok(Some(Claimed { job, run, slot }))
    }

    /// Record how a run ended and decide what the job does next.
    pub fn settle(
        &self,
        job: &Job,
        run: &Run,
        ok: bool,
        output: &str,
        error: &str,
        now: Millis,
    ) -> Result<Settled, ManagerError> {
        let status = if ok { RunStatus::Ok } else { RunStatus::Failed };
        self.store
            .finish_run(&run.id, status, output, error, delivery_for(job, ok))?;
        self.store.note_outcome(&job.id, status)?;

        if !ok && run.attempt <= job.retries {
            let at = now + retry_delay_ms(run.attempt);
            self.store.set_next_run(&job.id, Some(at), now)?;
            return Ok(Settled {
                retry_at: Some(at),
                disabled: false,
            });
        }

        // A one-time job has no next slot, so it is switched off rather than left enabled with
        // nothing to do.
        let disabled = job.next_run_ms.is_none();
        if disabled {
            self.store.set_enabled(&job.id, false, now)?;
        }
        Ok(Settled {
            retry_at: None,
            disabled,
        })
    }

    /// A run the operator cancelled is not a failure, so it earns no retry and no alert — but the
    /// job still has a status of its own and a one-time job still switches itself off.
    pub fn settle_cancelled(&self, job: &Job, now: Millis) -> Result<Settled, ManagerError> {
        self.store.note_outcome(&job.id, RunStatus::Cancelled)?;
        let disabled = job.next_run_ms.is_none();
        if disabled {
            self.store.set_enabled(&job.id, false, now)?;
        }
        Ok(Settled {
            retry_at: None,
            disabled,
        })
    }

    pub fn history(&self, job_id: &str, limit: usize) -> Result<Vec<Run>, ManagerError> {
        Ok(self.store.runs_for(job_id, limit)?)
    }

    /// An overdue slot is still run once when it is barely late, and skipped when the machine was
    /// asleep for a long time. Returning the skipped jobs lets the daemon explain the gap instead of
    /// running a week of morning reports at once.
    pub fn coalesce_overdue(&self, now: Millis) -> Result<Vec<Job>, ManagerError> {
        let mut skipped = Vec::new();
        for mut job in self.store.list_jobs()? {
            let Some(due) = job.next_run_ms else { continue };
            if !job.enabled || due > now {
                continue;
            }
            let grace = self.grace_ms(&job, due);
            if now - due <= grace {
                continue;
            }
            let next = self.next_slot(&job, now);
            self.store.set_next_run(&job.id, next, now)?;
            self.store.note_outcome(&job.id, RunStatus::Skipped)?;
            job.next_run_ms = next;
            job.last_status = RunStatus::Skipped.as_str().to_string();
            skipped.push(job);
        }
        Ok(skipped)
    }

    /// Half the period, clamped between two minutes and two hours: long enough that a restart does
    /// not throw away work that was nearly due, short enough that a laptop closed overnight does not
    /// replay the night.
    fn grace_ms(&self, job: &Job, due: Millis) -> Millis {
        let period = self
            .next_slot(job, due)
            .map(|next| next - due)
            .unwrap_or(3_600_000);
        (period / 2).clamp(120_000, 7_200_000)
    }
}

/// Delivery is only ever claimed as delivered by the code that delivered it; a job that has nowhere
/// to send its output records that instead of pretending.
fn delivery_for(job: &Job, ok: bool) -> Delivery {
    let target = job.target.trim();
    if target.is_empty() || target == "none" {
        return Delivery::None;
    }
    if ok {
        Delivery::Pending
    } else {
        Delivery::Failed
    }
}

/// Backoff a retry waits, indexed by the attempt that just failed.
pub fn retry_delay_ms(attempt: u32) -> Millis {
    match attempt {
        0 | 1 => 30_000,
        2 => 60_000,
        3 => 300_000,
        4 => 900_000,
        _ => 3_600_000,
    }
}

/// Whether a schedule text is measured from now rather than pinned to a wall clock reading.
fn relative(spec: &str) -> bool {
    let lower = spec.trim().to_ascii_lowercase();
    lower.starts_with("in ") || lower.starts_with("once ")
}

fn id_for(name: &str, now: Millis) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 24 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("job-{now}")
    } else {
        format!("{slug}-{now}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{Civil, Zone, instant_of};

    fn at(hour: u32, minute: u32) -> Millis {
        instant_of(
            Zone::Utc,
            Civil {
                year: 2026,
                month: 11,
                day: 4,
                hour,
                minute,
                second: 0,
            },
        )
    }

    fn manager() -> JobManager {
        let store = ScheduleStore::open_in_memory().unwrap();
        store.ensure_schema().unwrap();
        JobManager::new(store)
    }

    #[test]
    fn a_new_job_knows_when_it_runs_next() {
        let jobs = manager();
        let job = jobs
            .create(
                NewJob::new("nightly report", "0 9 * * *")
                    .timezone("UTC")
                    .payload("summarise the night"),
                at(8, 0),
            )
            .unwrap();
        assert_eq!(job.next_run_ms, Some(at(9, 0)));
        assert!(job.enabled);
        assert_eq!(jobs.find("night").unwrap().id, job.id);
    }

    #[test]
    fn a_typo_in_a_schedule_is_refused_at_creation() {
        let jobs = manager();
        let err = jobs
            .create(NewJob::new("bad", "0 25 * * *"), at(8, 0))
            .unwrap_err();
        assert!(err.to_string().contains("hour"), "{err}");
        assert!(jobs.list().unwrap().is_empty());
    }

    #[test]
    fn a_slot_is_taken_once_even_if_two_ticks_see_it() {
        let jobs = manager();
        let job = jobs
            .create(NewJob::new("check", "every 10m").payload("ping"), at(8, 0))
            .unwrap();
        let slot = job.next_run_ms.unwrap();
        let first = jobs.claim(&job, slot, at(8, 10)).unwrap();
        assert!(first.is_some());
        let second = jobs.claim(&job, slot, at(8, 10)).unwrap();
        assert!(second.is_none(), "the same slot must not run twice");
    }

    #[test]
    fn a_failure_comes_back_on_its_own_schedule() {
        let jobs = manager();
        let job = jobs
            .create(
                NewJob::new("flaky", "every 1h").payload("check").retries(2),
                at(8, 0),
            )
            .unwrap();
        let slot = job.next_run_ms.unwrap();
        let claimed = jobs.claim(&job, slot, at(8, 1)).unwrap().unwrap();
        let settled = jobs
            .settle(&claimed.job, &claimed.run, false, "", "boom", at(8, 2))
            .unwrap();
        assert_eq!(settled.retry_at, Some(at(8, 2) + 30_000));
        let stored = jobs.find("flaky").unwrap();
        assert_eq!(stored.next_run_ms, settled.retry_at);
        assert!(stored.enabled, "a job with retries left stays armed");
    }

    #[test]
    fn a_one_time_job_switches_itself_off_when_it_is_done() {
        let jobs = manager();
        let now = at(8, 0);
        let job = jobs
            .create(NewJob::new("remind", "in 30m").payload("stand up"), now)
            .unwrap();
        assert_eq!(job.next_run_ms, Some(now + 1_800_000));
        let slot = job.next_run_ms.unwrap();
        let claimed = jobs.claim(&job, slot, at(8, 30)).unwrap().unwrap();
        let settled = jobs
            .settle(&claimed.job, &claimed.run, true, "done", "", at(8, 31))
            .unwrap();
        assert!(settled.disabled);
        assert!(!jobs.find("remind").unwrap().enabled);
    }

    #[test]
    fn an_overdue_job_is_coalesced_instead_of_replayed() {
        let jobs = manager();
        let job = jobs
            .create(NewJob::new("poll", "every 10m").payload("ping"), at(8, 0))
            .unwrap();
        // Three hours of sleep: nine slots were missed.
        let skipped = jobs.coalesce_overdue(at(11, 0)).unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].id, job.id);
        let after = jobs.find("poll").unwrap();
        assert_eq!(after.next_run_ms, Some(at(11, 10)));
    }

    #[test]
    fn a_job_that_is_barely_late_still_runs() {
        let jobs = manager();
        jobs.create(NewJob::new("poll", "every 10m").payload("ping"), at(8, 0))
            .unwrap();
        let skipped = jobs.coalesce_overdue(at(8, 2)).unwrap();
        assert!(skipped.is_empty(), "a two-minute lag is not a missed run");
    }

    #[test]
    fn pausing_and_resuming_does_not_fire_the_old_backlog() {
        let jobs = manager();
        let job = jobs
            .create(NewJob::new("poll", "every 10m").payload("ping"), at(8, 0))
            .unwrap();
        jobs.enable(&job.id, false, at(8, 1)).unwrap();
        let resumed = jobs.enable("poll", true, at(12, 0)).unwrap();
        assert_eq!(resumed.next_run_ms, Some(at(12, 10)));
        assert!(resumed.enabled);
    }

    #[test]
    fn an_ambiguous_prefix_names_the_jobs_it_could_mean() {
        let jobs = manager();
        jobs.create(
            NewJob::new("report morning", "0 9 * * *").payload("a"),
            at(8, 0),
        )
        .unwrap();
        jobs.create(
            NewJob::new("report evening", "0 21 * * *").payload("b"),
            at(8, 0),
        )
        .unwrap();
        let err = jobs.find("report").unwrap_err();
        assert!(matches!(err, ManagerError::Ambiguous(..)), "{err}");
        assert!(err.to_string().contains("report morning"), "{err}");
    }

    #[test]
    fn running_now_brings_the_next_slot_forward() {
        let jobs = manager();
        let job = jobs
            .create(NewJob::new("poll", "every 1h").payload("ping"), at(8, 0))
            .unwrap();
        let moved = jobs.run_now("poll", at(8, 5)).unwrap();
        assert_eq!(moved.next_run_ms, Some(at(8, 5)));
        assert_eq!(jobs.find(&job.id).unwrap().next_run_ms, Some(at(8, 5)));
    }
}
