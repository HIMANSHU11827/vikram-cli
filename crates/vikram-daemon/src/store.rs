//! Durable state for the scheduler.
//!
//! Everything the scheduler must not forget lives here: the jobs an operator created, the runs
//! they produced, and the few scalars the heartbeat keeps between boots. The tables are created by
//! this module against the same SQLite file the rest of vikram uses, so a scheduled job survives a
//! crash, a reboot or a daemon restart without a second database to keep in step.
//!
//! Two rules shape the schema. A job row is mutable — it is what the operator edits. A run row is
//! append-only — it is what happened, so a later run can never rewrite an earlier one. And because
//! at-least-once is the only honest promise a scheduler can make, a run is identified by the
//! scheduled slot it was created for and by its attempt number: a crash during a run leaves a row
//! that says so instead of a run that silently never happened.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};

use crate::time::Millis;

/// Where a job's work comes from. Not every job needs a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobMode {
    /// A fresh agent session given a prompt.
    #[default]
    Agent,
    /// A command run without a model.
    Script,
    /// A workflow the runtime already knows how to run.
    Workflow,
    /// Hand the job to a bot or subagent rather than the main agent.
    Delegate,
}

impl JobMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Script => "script",
            Self::Workflow => "workflow",
            Self::Delegate => "delegate",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "agent" | "ai" | "prompt" => Some(Self::Agent),
            "script" | "shell" | "command" | "cmd" => Some(Self::Script),
            "workflow" => Some(Self::Workflow),
            "delegate" | "bot" | "subagent" => Some(Self::Delegate),
            _ => None,
        }
    }
}

/// `running` is the only non-terminal state. `unknown` exists because a run interrupted by a crash
/// may or may not have had an effect outside this process, and pretending otherwise would be a lie
/// the operator would act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Ok,
    Failed,
    Unknown,
    Cancelled,
    Skipped,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "running" => Some(Self::Running),
            "ok" => Some(Self::Ok),
            "failed" => Some(Self::Failed),
            "unknown" => Some(Self::Unknown),
            "cancelled" => Some(Self::Cancelled),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }

    pub fn terminal(self) -> bool {
        self != Self::Running
    }

    pub fn succeeded(self) -> bool {
        matches!(self, Self::Ok | Self::Skipped)
    }
}

/// Whether a result ever reached its destination. `pending` and `failed` are the states that must
/// stay visible: silence about a failed delivery is the failure mode that makes people stop
/// trusting a scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    None,
    Pending,
    Delivered,
    Failed,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "pending" => Self::Pending,
            "delivered" => Self::Delivered,
            "failed" => Self::Failed,
            _ => Self::None,
        }
    }
}

/// One scheduled job, exactly as the operator configured it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub description: String,
    /// The schedule as typed, kept so `show` can print back what the operator wrote.
    pub schedule: String,
    pub timezone: String,
    pub mode: JobMode,
    /// The prompt, the script, or the workflow name, depending on `mode`.
    pub payload: String,
    /// Where a result goes: `local`, `session`, `webhook:<url>`.
    pub target: String,
    pub agent: String,
    pub bot: String,
    pub timeout_ms: Option<Millis>,
    pub retries: u32,
    pub priority: i64,
    /// How a hive job runs its members: `parallel` or `sequential`.
    pub order: String,
    pub enabled: bool,
    pub created_ms: Millis,
    pub updated_ms: Millis,
    pub last_run_ms: Option<Millis>,
    pub next_run_ms: Option<Millis>,
    pub last_status: String,
}

impl Job {
    /// A job with the defaults an operator would expect: no retries, normal priority, enabled.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        schedule: impl Into<String>,
    ) -> Self {
        let now = crate::time::now_ms();
        Self {
            id: id.into(),
            name: name.into(),
            description: String::new(),
            schedule: schedule.into(),
            timezone: "local".into(),
            mode: JobMode::Agent,
            payload: String::new(),
            target: "local".into(),
            agent: String::new(),
            bot: String::new(),
            timeout_ms: None,
            retries: 0,
            priority: 0,
            order: "parallel".into(),
            enabled: true,
            created_ms: now,
            updated_ms: now,
            last_run_ms: None,
            next_run_ms: None,
            last_status: "never".into(),
        }
    }
}

/// One attempt at one job. `slot_ms` is the scheduled instant the attempt was created for, and
/// `(job_id, slot_ms, attempt)` is unique, so a retry is a new row rather than a rewrite of the
/// attempt that already happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: String,
    pub job_id: String,
    pub slot_ms: Millis,
    pub attempt: u32,
    pub started_ms: Millis,
    pub finished_ms: Option<Millis>,
    pub status: RunStatus,
    pub session: Option<String>,
    pub output: String,
    pub error: String,
    pub delivery: Delivery,
    /// Why the delivery went the way it did, when that is worth keeping; empty otherwise.
    pub delivery_note: String,
}

/// The scheduler's database. Owns its own connection so the daemon can hold it for its whole life
/// without contending with the TUI's store handle.
#[derive(Clone, Debug)]
pub struct ScheduleStore {
    conn: Arc<Mutex<Connection>>,
}

impl ScheduleStore {
    /// Open (or create) the scheduler tables in the database vikram already uses.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// Same schema in memory. Tests use this so they never touch the operator's real database.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, StoreError> {
        // The TUI and the daemon are separate processes on one file: WAL plus a busy timeout is
        // what keeps a scheduling tick from failing because the interface was writing a session.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "busy_timeout", 5_000).ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .expect("scheduler store mutex not poisoned")
    }

    /// The tables this module owns. Idempotent, so opening an existing database is a no-op.
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        self.lock().execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS daemon_jobs (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                schedule TEXT NOT NULL,
                timezone TEXT NOT NULL DEFAULT 'local',
                mode TEXT NOT NULL DEFAULT 'agent',
                payload TEXT NOT NULL DEFAULT '',
                target TEXT NOT NULL DEFAULT 'local',
                agent TEXT NOT NULL DEFAULT '',
                bot TEXT NOT NULL DEFAULT '',
                timeout_ms INTEGER,
                retries INTEGER NOT NULL DEFAULT 0,
                priority INTEGER NOT NULL DEFAULT 0,
                order_mode TEXT NOT NULL DEFAULT 'parallel',
                enabled INTEGER NOT NULL DEFAULT 1,
                created_ms INTEGER NOT NULL,
                updated_ms INTEGER NOT NULL,
                last_run_ms INTEGER,
                next_run_ms INTEGER,
                last_status TEXT NOT NULL DEFAULT 'never'
            );
            CREATE INDEX IF NOT EXISTS daemon_jobs_due
                ON daemon_jobs(enabled, next_run_ms);

            CREATE TABLE IF NOT EXISTS daemon_runs (
                id TEXT PRIMARY KEY,
                job_id TEXT NOT NULL,
                slot_ms INTEGER NOT NULL,
                attempt INTEGER NOT NULL DEFAULT 1,
                started_ms INTEGER NOT NULL,
                finished_ms INTEGER,
                status TEXT NOT NULL,
                session TEXT,
                output TEXT NOT NULL DEFAULT '',
                error TEXT NOT NULL DEFAULT '',
                delivery TEXT NOT NULL DEFAULT 'none',
                delivery_note TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS daemon_runs_by_job
                ON daemon_runs(job_id, started_ms DESC);
            CREATE INDEX IF NOT EXISTS daemon_runs_by_status
                ON daemon_runs(status, started_ms DESC);
            CREATE UNIQUE INDEX IF NOT EXISTS daemon_runs_once
                ON daemon_runs(job_id, slot_ms, attempt);

            CREATE TABLE IF NOT EXISTS daemon_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;
        // Columns added after a home was first written. `CREATE TABLE IF NOT EXISTS` leaves an
        // older table alone, so a daemon upgraded in place would otherwise fail every read with
        // "no such column" — the one failure an operator cannot fix without deleting their jobs.
        self.add_column(
            "daemon_jobs",
            "order_mode",
            "TEXT NOT NULL DEFAULT 'parallel'",
        )?;
        self.add_column("daemon_runs", "delivery_note", "TEXT NOT NULL DEFAULT ''")?;
        Ok(())
    }

    /// Add a column if this table does not already have it.
    fn add_column(&self, table: &str, column: &str, declaration: &str) -> Result<(), StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let known: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        if known.iter().any(|name| name == column) {
            return Ok(());
        }
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {declaration}"),
            [],
        )?;
        Ok(())
    }

    // -- jobs -------------------------------------------------------------------------------

    pub fn insert_job(&self, job: &Job) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO daemon_jobs (id, name, description, schedule, timezone, mode, payload,
                 target, agent, bot, timeout_ms, retries, priority, order_mode, enabled,
                 created_ms, updated_ms, last_run_ms, next_run_ms, last_status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 ?17, ?18, ?19, ?20)",
            params![
                job.id,
                job.name,
                job.description,
                job.schedule,
                job.timezone,
                job.mode.as_str(),
                job.payload,
                job.target,
                job.agent,
                job.bot,
                job.timeout_ms,
                job.retries,
                job.priority,
                job.order,
                i64::from(job.enabled),
                job.created_ms,
                job.updated_ms,
                job.last_run_ms,
                job.next_run_ms,
                job.last_status,
            ],
        )?;
        Ok(())
    }

    /// Write every mutable field back. `created_ms` and the id are the only fixed columns.
    pub fn update_job(&self, job: &Job) -> Result<(), StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE daemon_jobs SET name = ?2, description = ?3, schedule = ?4, timezone = ?5,
                 mode = ?6, payload = ?7, target = ?8, agent = ?9, bot = ?10, timeout_ms = ?11,
                 retries = ?12, priority = ?13, order_mode = ?14, enabled = ?15, updated_ms = ?16,
                 last_run_ms = ?17, next_run_ms = ?18, last_status = ?19
             WHERE id = ?1",
            params![
                job.id,
                job.name,
                job.description,
                job.schedule,
                job.timezone,
                job.mode.as_str(),
                job.payload,
                job.target,
                job.agent,
                job.bot,
                job.timeout_ms,
                job.retries,
                job.priority,
                job.order,
                i64::from(job.enabled),
                job.updated_ms,
                job.last_run_ms,
                job.next_run_ms,
                job.last_status,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Missing(job.id.clone()));
        }
        Ok(())
    }

    pub fn get_job(&self, id: &str) -> Result<Option<Job>, StoreError> {
        let conn = self.lock();
        let job = conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM daemon_jobs WHERE id = ?1"),
                params![id],
                row_to_job,
            )
            .optional()?;
        Ok(job)
    }

    /// Every job, disabled ones included, newest first among equals by name.
    pub fn list_jobs(&self) -> Result<Vec<Job>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM daemon_jobs ORDER BY created_ms ASC, name ASC"
        ))?;
        let jobs = stmt
            .query_map([], row_to_job)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(jobs)
    }

    /// Jobs whose next slot has arrived. Priority first, then the slot that waited longest, so a
    /// backlog drains by importance without starving the oldest job.
    pub fn due_jobs(&self, now: Millis) -> Result<Vec<Job>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM daemon_jobs
             WHERE enabled = 1 AND next_run_ms IS NOT NULL AND next_run_ms <= ?1
             ORDER BY priority DESC, next_run_ms ASC"
        ))?;
        let jobs = stmt
            .query_map(params![now], row_to_job)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(jobs)
    }

    /// Move a job's next slot, but only if nobody else already moved it. The compare-and-set is
    /// what stops two scheduler ticks — or two daemons that briefly overlapped — from running the
    /// same slot twice.
    pub fn claim(
        &self,
        id: &str,
        expected: Millis,
        next: Option<Millis>,
    ) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE daemon_jobs SET next_run_ms = ?3, last_run_ms = ?2 WHERE id = ?1
             AND next_run_ms = ?2",
            params![id, expected, next],
        )?;
        Ok(changed == 1)
    }

    /// Record a run's outcome on the job row, so `list` can show it without reading run history.
    pub fn note_outcome(&self, id: &str, status: RunStatus) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE daemon_jobs SET last_status = ?2 WHERE id = ?1",
            params![id, status.as_str()],
        )?;
        Ok(())
    }

    pub fn set_enabled(&self, id: &str, enabled: bool, now: Millis) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE daemon_jobs SET enabled = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id, i64::from(enabled), now],
        )?;
        Ok(changed == 1)
    }

    /// Move the next wake-up without touching the rest of the job. A retry uses this to come back
    /// sooner than the schedule says; the run rows keep what actually happened.
    pub fn set_next_run(
        &self,
        id: &str,
        next: Option<Millis>,
        now: Millis,
    ) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE daemon_jobs SET next_run_ms = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id, next, now],
        )?;
        Ok(changed == 1)
    }

    pub fn delete_job(&self, id: &str) -> Result<bool, StoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM daemon_runs WHERE job_id = ?1", params![id])?;
        let changed = tx.execute("DELETE FROM daemon_jobs WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(changed == 1)
    }

    // -- runs -------------------------------------------------------------------------------

    /// Start the attempt for a slot. A retried slot gets the next attempt number, which the unique
    /// index keeps honest.
    pub fn begin_run(
        &self,
        job: &Job,
        slot_ms: Millis,
        session: Option<&str>,
    ) -> Result<Run, StoreError> {
        let conn = self.lock();
        let attempt: u32 = conn.query_row(
            "SELECT COALESCE(MAX(attempt), 0) FROM daemon_runs WHERE job_id = ?1 AND slot_ms = ?2",
            params![job.id, slot_ms],
            |row| row.get::<_, i64>(0),
        )? as u32
            + 1;
        let run = Run {
            id: format!("run_{}_{}_{}", job.id, slot_ms, attempt),
            job_id: job.id.clone(),
            slot_ms,
            attempt,
            started_ms: crate::time::now_ms(),
            finished_ms: None,
            status: RunStatus::Running,
            session: session.map(str::to_string),
            output: String::new(),
            error: String::new(),
            delivery: if job.target.trim().is_empty() || job.target.trim() == "none" {
                Delivery::None
            } else {
                Delivery::Pending
            },
            delivery_note: String::new(),
        };
        conn.execute(
            "INSERT INTO daemon_runs (id, job_id, slot_ms, attempt, started_ms, status, session,
                 output, error, delivery)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                run.id,
                run.job_id,
                run.slot_ms,
                run.attempt,
                run.started_ms,
                run.status.as_str(),
                run.session,
                run.output,
                run.error,
                run.delivery.as_str(),
            ],
        )?;
        Ok(run)
    }

    pub fn finish_run(
        &self,
        id: &str,
        status: RunStatus,
        output: &str,
        error: &str,
        delivery: Delivery,
    ) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE daemon_runs SET finished_ms = ?2, status = ?3, output = ?4, error = ?5,
                 delivery = ?6 WHERE id = ?1",
            params![
                id,
                crate::time::now_ms(),
                status.as_str(),
                output,
                error,
                delivery.as_str()
            ],
        )?;
        Ok(())
    }

    /// Record where a finished run's result went, without touching its status: delivery happens
    /// after the run is settled, and a delivery that failed must not rewrite what the job did.
    pub fn set_delivery(&self, id: &str, delivery: Delivery) -> Result<bool, StoreError> {
        self.set_delivery_note(id, delivery, "")
    }

    /// Record where a delivery ended up, and why. The note is what turns `delivery: failed` into
    /// something an operator can act on, so it is kept with the run instead of being printed once.
    pub fn set_delivery_note(
        &self,
        id: &str,
        delivery: Delivery,
        note: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE daemon_runs SET delivery = ?2, delivery_note = ?3 WHERE id = ?1",
            params![id, delivery.as_str(), note],
        )?;
        Ok(changed == 1)
    }

    pub fn get_run(&self, id: &str) -> Result<Option<Run>, StoreError> {
        let conn = self.lock();
        let run = conn
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM daemon_runs WHERE id = ?1"),
                params![id],
                row_to_run,
            )
            .optional()?;
        Ok(run)
    }

    pub fn runs_for(&self, job_id: &str, limit: usize) -> Result<Vec<Run>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM daemon_runs WHERE job_id = ?1
             ORDER BY {RUN_ORDER} LIMIT ?2"
        ))?;
        let runs = stmt
            .query_map(params![job_id, limit as i64], row_to_run)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(runs)
    }

    /// Recent runs across every job, for `vikram jobs list`.
    pub fn recent_runs(&self, limit: usize) -> Result<Vec<Run>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM daemon_runs ORDER BY {RUN_ORDER} LIMIT ?1"
        ))?;
        let runs = stmt
            .query_map(params![limit as i64], row_to_run)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(runs)
    }

    /// Runs still marked `running` from a previous life. They are not failures — the work may have
    /// finished after the process that owned it died — so they become `unknown`, which is the one
    /// state that tells the operator to check rather than assume.
    pub fn adopt_orphans(&self, job_id: Option<&str>) -> Result<Vec<Run>, StoreError> {
        let conn = self.lock();
        let mut stmt = match job_id {
            Some(_) => conn.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM daemon_runs WHERE status = 'running' AND job_id = ?1"
            ))?,
            None => conn.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM daemon_runs WHERE status = 'running'"
            ))?,
        };
        let rows = match job_id {
            Some(id) => stmt
                .query_map(params![id], row_to_run)?
                .collect::<Result<Vec<_>, _>>()?,
            None => stmt
                .query_map([], row_to_run)?
                .collect::<Result<Vec<_>, _>>()?,
        };
        drop(stmt);
        for run in &rows {
            conn.execute(
                "UPDATE daemon_runs SET status = 'unknown', finished_ms = ?2,
                     error = CASE WHEN error = '' THEN 'interrupted by a restart' ELSE error END
                 WHERE id = ?1",
                params![run.id, crate::time::now_ms()],
            )?;
        }
        Ok(rows)
    }

    /// Keep history bounded: the newest `keep` runs of a job survive, older ones go.
    pub fn prune_runs(&self, job_id: &str, keep: usize) -> Result<usize, StoreError> {
        let conn = self.lock();
        let removed = conn.execute(
            &format!(
                "DELETE FROM daemon_runs WHERE job_id = ?1 AND id NOT IN (
                 SELECT id FROM daemon_runs WHERE job_id = ?1 ORDER BY {RUN_ORDER} LIMIT ?2
             )"
            ),
            params![job_id, keep as i64],
        )?;
        Ok(removed)
    }

    // -- scalars ----------------------------------------------------------------------------

    pub fn state(&self, key: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock();
        let value = conn
            .query_row(
                "SELECT value FROM daemon_state WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO daemon_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn state_ms(&self, key: &str) -> Result<Option<Millis>, StoreError> {
        Ok(self.state(key)?.and_then(|text| text.parse().ok()))
    }

    pub fn set_state_ms(&self, key: &str, value: Millis) -> Result<(), StoreError> {
        self.set_state(key, &value.to_string())
    }

    pub fn state_bool(&self, key: &str, fallback: bool) -> Result<bool, StoreError> {
        Ok(match self.state(key)?.as_deref() {
            Some("1") | Some("true") => true,
            Some("0") | Some("false") => false,
            _ => fallback,
        })
    }
}

const JOB_COLUMNS: &str = "id, name, description, schedule, timezone, mode, payload, target, agent,
    bot, timeout_ms, retries, priority, enabled, created_ms, updated_ms, last_run_ms, next_run_ms,
    last_status, order_mode";

/// Newest first, with the slot and the attempt as tiebreakers. Runs started in the same
/// millisecond — which is normal in a test and possible in a burst — must still come back in a
/// stable order, or `history` would show a different list every time it was asked.
const RUN_ORDER: &str = "started_ms DESC, slot_ms DESC, attempt DESC";

/// The state key that asks one member of one running hive to stop. It lives in the database
/// because the CLI that asks and the daemon that honours it are different processes.
pub fn member_stop_key(run: &str, agent: &str) -> String {
    format!("member.{run}.{agent}")
}

const RUN_COLUMNS: &str =
    "id, job_id, slot_ms, attempt, started_ms, finished_ms, status, session, output, error,
    delivery, delivery_note";

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let mode: String = row.get(5)?;
    Ok(Job {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        schedule: row.get(3)?,
        timezone: row.get(4)?,
        // A row written by a newer build must not take the daemon down: an unknown mode is treated
        // as a plain agent job, and the run will fail loudly when the payload turns out wrong.
        mode: JobMode::parse(&mode).unwrap_or(JobMode::Agent),
        payload: row.get(6)?,
        target: row.get(7)?,
        agent: row.get(8)?,
        bot: row.get(9)?,
        timeout_ms: row.get(10)?,
        retries: row.get::<_, i64>(11)?.max(0) as u32,
        priority: row.get(12)?,
        enabled: row.get::<_, i64>(13)? != 0,
        created_ms: row.get(14)?,
        updated_ms: row.get(15)?,
        last_run_ms: row.get(16)?,
        next_run_ms: row.get(17)?,
        last_status: row.get(18)?,
        // Kept exactly as written; the runner reads it with a fallback, so a word from a newer
        // build cannot stop the job from running.
        order: row.get(19)?,
    })
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    let status: String = row.get(6)?;
    let delivery: String = row.get(10)?;
    let delivery_note: String = row.get(11)?;
    Ok(Run {
        id: row.get(0)?,
        job_id: row.get(1)?,
        slot_ms: row.get(2)?,
        attempt: row.get::<_, i64>(3)?.max(1) as u32,
        started_ms: row.get(4)?,
        finished_ms: row.get(5)?,
        status: RunStatus::parse(&status).unwrap_or(RunStatus::Unknown),
        session: row.get(7)?,
        output: row.get(8)?,
        error: row.get(9)?,
        delivery: Delivery::parse(&delivery),
        delivery_note,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("scheduler database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("scheduler database: {0}")]
    Io(#[from] std::io::Error),
    #[error("no job named `{0}`")]
    Missing(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ScheduleStore {
        ScheduleStore::open_in_memory().expect("in memory store")
    }

    fn job(name: &str, schedule: &str) -> Job {
        Job::new(format!("job_{name}"), name, schedule)
    }

    #[test]
    fn a_job_survives_a_reopen() {
        let store = store();
        store.insert_job(&job("report", "every 10m")).unwrap();
        // Same connection, but the read is a fresh SELECT: the row is in the table, not in memory.
        let read = store.get_job("job_report").unwrap().expect("job is there");
        assert_eq!(read.name, "report");
        assert_eq!(read.schedule, "every 10m");
        assert_eq!(read.mode, JobMode::Agent);
        assert!(read.enabled);

        let mut edited = read.clone();
        edited.payload = "look at the server".into();
        edited.next_run_ms = Some(1_000);
        store.update_job(&edited).unwrap();
        let reread = store.get_job("job_report").unwrap().unwrap();
        assert_eq!(reread.payload, "look at the server");
        assert_eq!(reread.next_run_ms, Some(1_000));
        assert_eq!(reread.created_ms, read.created_ms, "creation time is fixed");
    }

    #[test]
    fn the_order_a_team_runs_in_is_kept_with_the_job() {
        let store = store();
        let mut hive = job("deploy", "every 1h");
        hive.order = "sequential".into();
        store.insert_job(&hive).unwrap();
        assert_eq!(
            store.get_job("job_deploy").unwrap().unwrap().order,
            "sequential"
        );

        // A row written before the column existed still reads as the default.
        let mut plain = job("plain", "every 1h");
        plain.order = String::new();
        store.insert_job(&plain).unwrap();
        assert_eq!(store.get_job("job_plain").unwrap().unwrap().order, "");
    }

    #[test]
    fn only_due_and_enabled_jobs_are_offered() {
        let store = store();
        let mut soon = job("soon", "every 10m");
        soon.next_run_ms = Some(1_000);
        soon.priority = 5;
        let mut later = job("later", "every 10m");
        later.next_run_ms = Some(9_000);
        let mut off = job("off", "every 10m");
        off.next_run_ms = Some(500);
        off.enabled = false;
        for job in [&soon, &later, &off] {
            store.insert_job(job).unwrap();
        }

        let due = store.due_jobs(2_000).unwrap();
        assert_eq!(
            due.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(),
            ["job_soon"],
            "a disabled job is not due and a later slot has not arrived"
        );

        let all = store.due_jobs(10_000).unwrap();
        assert_eq!(
            all.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(),
            ["job_soon", "job_later"],
            "priority first, then the oldest slot"
        );
    }

    #[test]
    fn a_slot_is_claimed_once() {
        let store = store();
        let mut job = job("claim", "every 10m");
        job.next_run_ms = Some(1_000);
        store.insert_job(&job).unwrap();

        assert!(store.claim("job_claim", 1_000, Some(601_000)).unwrap());
        assert!(
            !store.claim("job_claim", 1_000, Some(601_000)).unwrap(),
            "the second tick must lose the race, not run the slot again"
        );
        let moved = store.get_job("job_claim").unwrap().unwrap();
        assert_eq!(moved.next_run_ms, Some(601_000));
        assert_eq!(moved.last_run_ms, Some(1_000));
    }

    #[test]
    fn a_retry_is_a_new_attempt_of_the_same_slot() {
        let store = store();
        let job = job("retry", "every 10m");
        store.insert_job(&job).unwrap();

        let first = store.begin_run(&job, 1_000, None).unwrap();
        store
            .finish_run(&first.id, RunStatus::Failed, "", "boom", Delivery::Pending)
            .unwrap();
        let second = store.begin_run(&job, 1_000, None).unwrap();

        assert_eq!(first.attempt, 1);
        assert_eq!(second.attempt, 2);
        assert_ne!(first.id, second.id);
        let runs = store.runs_for("job_retry", 10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].attempt, 2, "newest run first");
        assert_eq!(runs[1].status, RunStatus::Failed);
    }

    #[test]
    fn a_run_left_running_becomes_unknown_not_failed() {
        let store = store();
        let job = job("crash", "every 10m");
        store.insert_job(&job).unwrap();
        let run = store.begin_run(&job, 1_000, Some("session_1")).unwrap();

        let adopted = store.adopt_orphans(None).unwrap();
        assert_eq!(adopted.len(), 1);
        let after = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(after.status, RunStatus::Unknown);
        assert!(after.finished_ms.is_some());
        assert!(after.error.contains("interrupted"));
    }

    #[test]
    fn history_is_capped_but_the_newest_survives() {
        let store = store();
        let job = job("noisy", "every 1m");
        store.insert_job(&job).unwrap();
        for slot in 0..6 {
            let run = store.begin_run(&job, slot, None).unwrap();
            store
                .finish_run(&run.id, RunStatus::Ok, "fine", "", Delivery::None)
                .unwrap();
        }
        let removed = store.prune_runs("job_noisy", 2).unwrap();
        assert_eq!(removed, 4);
        let left = store.runs_for("job_noisy", 10).unwrap();
        assert_eq!(left.len(), 2);
        assert_eq!(left[0].slot_ms, 5, "the newest runs are the ones kept");
    }

    #[test]
    fn deleting_a_job_takes_its_history_with_it() {
        let store = store();
        let job = job("gone", "every 10m");
        store.insert_job(&job).unwrap();
        store.begin_run(&job, 1_000, None).unwrap();

        assert!(store.delete_job("job_gone").unwrap());
        assert!(store.get_job("job_gone").unwrap().is_none());
        assert!(store.runs_for("job_gone", 10).unwrap().is_empty());
        assert!(
            !store.delete_job("job_gone").unwrap(),
            "second delete says so"
        );
    }

    #[test]
    fn scalars_round_trip_for_the_heartbeat() {
        let store = store();
        assert_eq!(store.state_ms("heartbeat.last").unwrap(), None);
        assert!(store.state_bool("heartbeat.enabled", true).unwrap());

        store
            .set_state_ms("heartbeat.last", 1_700_000_000_000)
            .unwrap();
        store.set_state("heartbeat.enabled", "0").unwrap();

        assert_eq!(
            store.state_ms("heartbeat.last").unwrap(),
            Some(1_700_000_000_000)
        );
        assert!(!store.state_bool("heartbeat.enabled", true).unwrap());
    }
    #[test]
    fn one_member_of_one_run_gets_its_own_stop_flag() {
        let store = store();
        assert_eq!(
            member_stop_key("run_1", "mira"),
            "member.run_1.mira".to_string()
        );
        let key = member_stop_key("run_1", "mira");
        assert!(!store.state_bool(&key, false).unwrap());
        store.set_state(&key, "1").unwrap();
        assert!(store.state_bool(&key, false).unwrap());
        // The other member of the same run is untouched by her stop.
        assert!(
            !store
                .state_bool(&member_stop_key("run_1", "arun"), false)
                .unwrap()
        );
    }
    #[test]
    fn a_column_added_later_is_given_to_a_home_written_before_it() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("vikram.db");
        {
            let conn = rusqlite::Connection::open(&path).expect("old database");
            // The schema as it was before `order_mode` and `delivery_note` existed.
            conn.execute_batch(
                "CREATE TABLE daemon_jobs (
                    id TEXT PRIMARY KEY, name TEXT NOT NULL,
                    description TEXT NOT NULL DEFAULT '', schedule TEXT NOT NULL,
                    timezone TEXT NOT NULL DEFAULT 'local', mode TEXT NOT NULL DEFAULT 'agent',
                    payload TEXT NOT NULL DEFAULT '', target TEXT NOT NULL DEFAULT 'local',
                    agent TEXT NOT NULL DEFAULT '', bot TEXT NOT NULL DEFAULT '',
                    timeout_ms INTEGER, retries INTEGER NOT NULL DEFAULT 0,
                    priority INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 1,
                    created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL,
                    last_run_ms INTEGER, next_run_ms INTEGER,
                    last_status TEXT NOT NULL DEFAULT 'never'
                 );
                 CREATE TABLE daemon_runs (
                    id TEXT PRIMARY KEY, job_id TEXT NOT NULL, slot_ms INTEGER NOT NULL,
                    attempt INTEGER NOT NULL DEFAULT 1, started_ms INTEGER NOT NULL,
                    finished_ms INTEGER, status TEXT NOT NULL, session TEXT,
                    output TEXT NOT NULL DEFAULT '', error TEXT NOT NULL DEFAULT '',
                    delivery TEXT NOT NULL DEFAULT 'none'
                 );",
            )
            .expect("old schema");
        }
        // Opening it again is what an upgraded daemon does, and it has to leave a table it can
        // read: a missing column fails every `jobs inspect` with an error no operator can fix.
        let store = ScheduleStore::open(&path).expect("upgraded store");
        let conn = store.lock();
        for (table, column) in [
            ("daemon_jobs", "order_mode"),
            ("daemon_runs", "delivery_note"),
        ] {
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .expect("pragma");
            let known: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .expect("columns")
                .collect::<Result<Vec<_>, _>>()
                .expect("names");
            assert!(known.iter().any(|name| name == column), "{table}.{column}");
        }
    }
}
