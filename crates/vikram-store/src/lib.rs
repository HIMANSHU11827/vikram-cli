//! Durable state.
//!
//! Two tables carry the runtime: `journal` is the append-only execution substrate, and `sessions`
//! / `agents` are the metadata an interface lists. Journal appends and their idempotency marker are
//! written in one transaction, which is what makes crash recovery safe.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use vikram_contracts::error::{Result as ContractResult, VikramError};
use vikram_contracts::{
    AgentId, Appended, ContractDecl, EffectId, JournalRecord, JournalSink, NewRecord, RecordKind,
    SessionId, TeamId,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: SessionId,
    pub title: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub agent: Option<AgentId>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRow {
    pub id: AgentId,
    pub name: String,
    pub role: String,
    pub description: String,
    pub icon: Option<String>,
    pub created_ms: u64,
}

/// A HIVE team: a user-named group of saved agent profiles sharing one dedicated conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRow {
    pub id: TeamId,
    pub name: String,
    pub session_id: Option<SessionId>,
    pub created_ms: u64,
}

/// One connection guarded by a mutex. The runtime is single-process; contention is limited to
/// journal appends and the occasional metadata write, both sub-millisecond.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("store mutex not poisoned")
    }

    pub fn journal(&self) -> SqliteJournal {
        SqliteJournal {
            conn: Arc::clone(&self.conn),
        }
    }

    // ---- sessions -------------------------------------------------------

    pub fn create_session(
        &self,
        title: &str,
        cwd: &Path,
        model: Option<&str>,
        now_ms: u64,
    ) -> Result<SessionRow> {
        let row = SessionRow {
            id: SessionId::generate(),
            title: title.to_owned(),
            cwd: cwd.to_path_buf(),
            model: model.map(str::to_owned),
            agent: None,
            created_ms: now_ms,
            updated_ms: now_ms,
        };
        self.lock().execute(
            "INSERT INTO sessions (id, title, cwd, model, agent_id, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.id.as_str(),
                row.title,
                row.cwd.to_string_lossy(),
                row.model,
                row.agent.as_ref().map(|a| a.as_str().to_owned()),
                row.created_ms as i64,
                row.updated_ms as i64
            ],
        )?;
        Ok(row)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, title, cwd, model, agent_id, created_ms, updated_ms
             FROM sessions ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SessionRow {
                    id: SessionId::new(row.get::<_, String>(0)?),
                    title: row.get(1)?,
                    cwd: PathBuf::from(row.get::<_, String>(2)?),
                    model: row.get(3)?,
                    agent: row.get::<_, Option<String>>(4)?.map(AgentId::new),
                    created_ms: row.get::<_, i64>(5)? as u64,
                    updated_ms: row.get::<_, i64>(6)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn rename_session(&self, id: &SessionId, title: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET title = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), title, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn set_session_model(
        &self,
        id: &SessionId,
        model: Option<&str>,
        now_ms: u64,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET model = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), model, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn set_session_agent(
        &self,
        id: &SessionId,
        agent: Option<&AgentId>,
        now_ms: u64,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET agent_id = ?2, updated_ms = ?3 WHERE id = ?1",
            params![
                id.as_str(),
                agent.map(|a| a.as_str().to_owned()),
                now_ms as i64
            ],
        )?;
        Ok(())
    }

    pub fn touch_session(&self, id: &SessionId, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET updated_ms = ?2 WHERE id = ?1",
            params![id.as_str(), now_ms as i64],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, id: &SessionId) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM journal WHERE session = ?1",
            params![id.as_str()],
        )?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![id.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    // ---- agents ---------------------------------------------------------

    pub fn save_agent(&self, row: &AgentRow) -> Result<()> {
        self.lock().execute(
            "INSERT INTO agents (id, name, role, description, icon, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               name = excluded.name, role = excluded.role,
               description = excluded.description, icon = excluded.icon",
            params![
                row.id.as_str(),
                row.name,
                row.role,
                row.description,
                row.icon,
                row.created_ms as i64
            ],
        )?;
        Ok(())
    }

    pub fn list_agents(&self) -> Result<Vec<AgentRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, role, description, icon, created_ms FROM agents ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AgentRow {
                    id: AgentId::new(row.get::<_, String>(0)?),
                    name: row.get(1)?,
                    role: row.get(2)?,
                    description: row.get(3)?,
                    icon: row.get(4)?,
                    created_ms: row.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_agent(&self, id: &AgentId) -> Result<()> {
        self.lock()
            .execute("DELETE FROM agents WHERE id = ?1", params![id.as_str()])?;
        Ok(())
    }

    // ---- teams ----------------------------------------------------------

    pub fn create_team(&self, name: &str, now_ms: u64) -> Result<TeamRow> {
        let row = TeamRow {
            id: TeamId::generate(),
            name: name.to_owned(),
            session_id: None,
            created_ms: now_ms,
        };
        self.lock().execute(
            "INSERT INTO teams (id, name, session_id, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.id.as_str(),
                row.name,
                row.session_id.as_ref().map(|s| s.as_str().to_owned()),
                row.created_ms as i64,
                now_ms as i64
            ],
        )?;
        Ok(row)
    }

    pub fn list_teams(&self) -> Result<Vec<TeamRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, session_id, created_ms FROM teams ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(TeamRow {
                    id: TeamId::new(row.get::<_, String>(0)?),
                    name: row.get(1)?,
                    session_id: row.get::<_, Option<String>>(2)?.map(SessionId::new),
                    created_ms: row.get::<_, i64>(3)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn rename_team(&self, id: &TeamId, name: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE teams SET name = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), name, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn set_team_session(
        &self,
        id: &TeamId,
        session: Option<&SessionId>,
        now_ms: u64,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE teams SET session_id = ?2, updated_ms = ?3 WHERE id = ?1",
            params![
                id.as_str(),
                session.map(|s| s.as_str().to_owned()),
                now_ms as i64
            ],
        )?;
        Ok(())
    }

    pub fn delete_team(&self, id: &TeamId) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM team_members WHERE team = ?1",
            params![id.as_str()],
        )?;
        tx.execute("DELETE FROM teams WHERE id = ?1", params![id.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    pub fn add_team_member(&self, team: &TeamId, agent: &AgentId) -> Result<()> {
        self.lock().execute(
            "INSERT OR IGNORE INTO team_members (team, agent) VALUES (?1, ?2)",
            params![team.as_str(), agent.as_str()],
        )?;
        Ok(())
    }

    pub fn remove_team_member(&self, team: &TeamId, agent: &AgentId) -> Result<()> {
        self.lock().execute(
            "DELETE FROM team_members WHERE team = ?1 AND agent = ?2",
            params![team.as_str(), agent.as_str()],
        )?;
        Ok(())
    }

    pub fn list_team_members(&self, team: &TeamId) -> Result<Vec<AgentId>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT agent FROM team_members WHERE team = ?1 ORDER BY agent ASC")?;
        let rows = stmt
            .query_map(params![team.as_str()], |row| {
                Ok(AgentId::new(row.get::<_, String>(0)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Drop an agent from every team that references it, so membership never outlives a profile.
    pub fn remove_agent_from_teams(&self, agent: &AgentId) -> Result<Vec<TeamId>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT team FROM team_members WHERE agent = ?1")?;
        let teams: Vec<TeamId> = stmt
            .query_map(params![agent.as_str()], |row| {
                Ok(TeamId::new(row.get::<_, String>(0)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !teams.is_empty() {
            conn.execute(
                "DELETE FROM team_members WHERE agent = ?1",
                params![agent.as_str()],
            )?;
        }
        Ok(teams)
    }

    // ---- journal helpers ------------------------------------------------

    /// Load a session's conversation blocks in journal order. Used to rehydrate a session after a
    /// restart; replay never re-applies effects because the journal already records them.
    pub fn replay_messages(&self, session: &SessionId) -> Result<Vec<JournalRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT seq, session, ts_ms, kind, effect, payload FROM journal
             WHERE session = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![session.as_str()], |row| {
                let kind: String = row.get(3)?;
                let payload: String = row.get(5)?;
                Ok(JournalRecord {
                    seq: row.get::<_, i64>(0)? as u64,
                    session: SessionId::new(row.get::<_, String>(1)?),
                    ts_ms: row.get::<_, i64>(2)? as u64,
                    kind: RecordKind::Message,
                    effect: row.get::<_, Option<String>>(4)?.map(EffectId::new),
                    payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                })
                .map(|mut record| {
                    record.kind = kind_from_str(&kind);
                    record
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn session_count(&self) -> Result<u64> {
        let conn = self.lock();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        Ok(count as u64)
    }
}

fn kind_from_str(value: &str) -> RecordKind {
    match value {
        "message" => RecordKind::Message,
        "tool_call" => RecordKind::ToolCall,
        "tool_result" => RecordKind::ToolResult,
        "policy" => RecordKind::Policy,
        "approval" => RecordKind::Approval,
        "status" => RecordKind::Status,
        "compaction" => RecordKind::Compaction,
        "cancellation" => RecordKind::Cancellation,
        "activity" => RecordKind::Activity,
        _ => RecordKind::Status,
    }
}

fn kind_to_str(kind: RecordKind) -> &'static str {
    match kind {
        RecordKind::Message => "message",
        RecordKind::ToolCall => "tool_call",
        RecordKind::ToolResult => "tool_result",
        RecordKind::Policy => "policy",
        RecordKind::Approval => "approval",
        RecordKind::Status => "status",
        RecordKind::Compaction => "compaction",
        RecordKind::Cancellation => "cancellation",
        RecordKind::Activity => "activity",
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            id          TEXT PRIMARY KEY,
            title       TEXT NOT NULL,
            cwd         TEXT NOT NULL,
            model       TEXT,
            agent_id    TEXT,
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS agents (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            role        TEXT NOT NULL,
            description TEXT NOT NULL,
            icon        TEXT,
            created_ms  INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS teams (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            session_id  TEXT,
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS team_members (
            team  TEXT NOT NULL,
            agent TEXT NOT NULL,
            PRIMARY KEY (team, agent)
        );

        CREATE TABLE IF NOT EXISTS journal (
            session  TEXT NOT NULL,
            seq      INTEGER NOT NULL,
            ts_ms    INTEGER NOT NULL,
            kind     TEXT NOT NULL,
            effect   TEXT,
            payload  TEXT NOT NULL,
            PRIMARY KEY (session, seq)
        );

        CREATE UNIQUE INDEX IF NOT EXISTS journal_effect
            ON journal(session, effect) WHERE effect IS NOT NULL;
        "#,
    )?;
    Ok(())
}

/// [`JournalSink`] over the same connection as the metadata tables.
#[derive(Clone, Debug)]
pub struct SqliteJournal {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteJournal {
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("store mutex not poisoned")
    }
}

fn storage(err: impl std::fmt::Display) -> VikramError {
    VikramError::Storage(err.to_string())
}

impl JournalSink for SqliteJournal {
    fn contract(&self) -> ContractDecl {
        ContractDecl::new(vikram_contracts::version::JOURNAL_SINK, 1, 0)
    }

    fn append(&self, record: &NewRecord) -> ContractResult<Appended> {
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(storage)?;

        if let Some(effect) = &record.effect {
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT seq FROM journal WHERE session = ?1 AND effect = ?2",
                    params![record.session.as_str(), effect.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            if let Some(seq) = existing {
                return Ok(Appended::AlreadyApplied(seq as u64));
            }
        }

        let next: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM journal WHERE session = ?1",
                params![record.session.as_str()],
                |row| row.get(0),
            )
            .map_err(storage)?;

        tx.execute(
            "INSERT INTO journal (session, seq, ts_ms, kind, effect, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.session.as_str(),
                next,
                record.ts_ms as i64,
                kind_to_str(record.kind),
                record.effect.as_ref().map(|e| e.as_str().to_owned()),
                serde_json::to_string(&record.payload).map_err(storage)?,
            ],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(Appended::Written(next as u64))
    }

    fn read_after(
        &self,
        session: &SessionId,
        after: u64,
        limit: usize,
    ) -> ContractResult<Vec<JournalRecord>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT seq, session, ts_ms, kind, effect, payload FROM journal
                 WHERE session = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map(
                params![session.as_str(), after as i64, limit as i64],
                |row| {
                    let kind: String = row.get(3)?;
                    let payload: String = row.get(5)?;
                    Ok(JournalRecord {
                        seq: row.get::<_, i64>(0)? as u64,
                        session: SessionId::new(row.get::<_, String>(1)?),
                        ts_ms: row.get::<_, i64>(2)? as u64,
                        kind: kind_from_str(&kind),
                        effect: row.get::<_, Option<String>>(4)?.map(EffectId::new),
                        payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                    })
                },
            )
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        Ok(rows)
    }

    fn last_seq(&self, session: &SessionId) -> ContractResult<Option<u64>> {
        let conn = self.lock();
        let seq: Option<i64> = conn
            .query_row(
                "SELECT MAX(seq) FROM journal WHERE session = ?1",
                params![session.as_str()],
                |row| row.get(0),
            )
            .map_err(storage)?;
        Ok(seq.map(|s| s as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_round_trip_and_delete_cascades_the_journal() {
        let store = Store::open_in_memory().unwrap();
        let row = store
            .create_session("Session 1", Path::new("/work"), Some("m"), 10)
            .unwrap();
        let listed = store.list_sessions().unwrap();
        assert_eq!(listed, vec![row.clone()]);

        store
            .journal()
            .append(&NewRecord::new(
                row.id.clone(),
                RecordKind::Message,
                serde_json::json!({"text": "hi"}),
            ))
            .unwrap();

        store.delete_session(&row.id).unwrap();
        assert!(store.list_sessions().unwrap().is_empty());
        assert!(
            store
                .journal()
                .read_after(&row.id, 0, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_same_effect_is_never_applied_twice() {
        let store = Store::open_in_memory().unwrap();
        let session = SessionId::generate();
        let effect = EffectId::generate();
        let record = NewRecord::new(
            session.clone(),
            RecordKind::ToolResult,
            serde_json::json!({}),
        )
        .with_effect(effect.clone());

        let first = store.journal().append(&record).unwrap();
        let second = store.journal().append(&record).unwrap();
        assert!(matches!(first, Appended::Written(1)));
        assert_eq!(second, Appended::AlreadyApplied(1));
        assert_eq!(
            store.journal().read_after(&session, 0, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn journal_seqs_are_per_session_and_monotonic() {
        let store = Store::open_in_memory().unwrap();
        let a = SessionId::generate();
        let b = SessionId::generate();
        let journal = store.journal();
        assert_eq!(
            journal
                .append(&NewRecord::new(
                    a.clone(),
                    RecordKind::Message,
                    serde_json::json!(1)
                ))
                .unwrap(),
            Appended::Written(1)
        );
        assert_eq!(
            journal
                .append(&NewRecord::new(
                    b.clone(),
                    RecordKind::Message,
                    serde_json::json!(2)
                ))
                .unwrap(),
            Appended::Written(1)
        );
        assert_eq!(
            journal
                .append(&NewRecord::new(
                    a.clone(),
                    RecordKind::Status,
                    serde_json::json!(3)
                ))
                .unwrap(),
            Appended::Written(2)
        );
        assert_eq!(journal.last_seq(&a).unwrap(), Some(2));
        assert_eq!(journal.read_after(&a, 1, 10).unwrap()[0].seq, 2);
    }

    #[test]
    fn agent_profiles_persist_and_update_in_place() {
        let store = Store::open_in_memory().unwrap();
        let mut row = AgentRow {
            id: AgentId::generate(),
            name: "Rudra".into(),
            role: "Implementer".into(),
            description: "builds".into(),
            icon: None,
            created_ms: 1,
        };
        store.save_agent(&row).unwrap();
        row.description = "builds carefully".into();
        store.save_agent(&row).unwrap();
        let listed = store.list_agents().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].description, "builds carefully");
    }

    #[test]
    fn teams_round_trip_with_members_and_session_link() {
        let store = Store::open_in_memory().unwrap();
        let now = 100;
        let team = store.create_team("Forge", now).unwrap();
        assert_eq!(team.name, "Forge");
        assert_eq!(team.session_id, None);

        let agent = AgentId::generate();
        store.add_team_member(&team.id, &agent).unwrap();
        store.add_team_member(&team.id, &agent).unwrap(); // idempotent
        let session = store
            .create_session("Forge", Path::new("/work"), None, now + 1)
            .unwrap();
        store
            .set_team_session(&team.id, Some(&session.id), now + 2)
            .unwrap();

        let listed = store.list_teams().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Forge");
        assert_eq!(listed[0].session_id.as_ref(), Some(&session.id));
        assert_eq!(
            store.list_team_members(&team.id).unwrap(),
            vec![agent.clone()]
        );

        store
            .rename_team(&team.id, "Project Orion", now + 3)
            .unwrap();
        assert_eq!(store.list_teams().unwrap()[0].name, "Project Orion");

        store.remove_team_member(&team.id, &agent).unwrap();
        assert!(store.list_team_members(&team.id).unwrap().is_empty());
    }

    #[test]
    fn deleting_a_team_cascades_to_membership() {
        let store = Store::open_in_memory().unwrap();
        let team = store.create_team("Nexus", 1).unwrap();
        store
            .add_team_member(&team.id, &AgentId::generate())
            .unwrap();
        store.delete_team(&team.id).unwrap();
        assert!(store.list_teams().unwrap().is_empty());
        assert!(store.list_team_members(&team.id).unwrap().is_empty());
    }

    #[test]
    fn removing_an_agent_drops_it_from_every_team() {
        let store = Store::open_in_memory().unwrap();
        let agent = AgentId::generate();
        let a = store.create_team("A", 1).unwrap();
        let b = store.create_team("B", 2).unwrap();
        store.add_team_member(&a.id, &agent).unwrap();
        store.add_team_member(&b.id, &agent).unwrap();

        let affected = store.remove_agent_from_teams(&agent).unwrap();
        assert_eq!(affected.len(), 2);
        assert!(store.list_team_members(&a.id).unwrap().is_empty());
        assert!(store.list_team_members(&b.id).unwrap().is_empty());
        // Second call is a no-op with no teams affected.
        assert!(store.remove_agent_from_teams(&agent).unwrap().is_empty());
    }

    #[test]
    fn team_names_support_arbitrary_unicode() {
        let store = Store::open_in_memory().unwrap();
        let team = store.create_team("研究小组 🐝", 1).unwrap();
        assert_eq!(store.list_teams().unwrap()[0].name, "研究小组 🐝");
        assert_eq!(team.name, "研究小组 🐝");
    }
}
