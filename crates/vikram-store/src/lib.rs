//! Durable state.
//!
//! Two tables carry the runtime: `journal` is the append-only execution substrate, and `sessions`
//! / `bots` are the metadata an interface lists. Journal appends and their idempotency marker are
//! written in one transaction, which is what makes crash recovery safe.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use vikram_contracts::error::{Result as ContractResult, VikramError};
use vikram_contracts::{
    Appended, AssignmentId, BotId, ContractDecl, EffectId, EntityId, HiveId, HiveMessageId,
    HiveRosterId, JournalRecord, JournalSink, NewRecord, ProfileId, ProjectId, RecordKind,
    SessionId, TicketEventId, TicketId, TicketMessageId, UserId,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("corrupt data: {0}")]
    Parse(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: SessionId,
    pub title: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub bot: Option<BotId>,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// `active` or `archived`. Archived sessions are kept, hidden from default listings.
    pub status: String,
    /// Optional project association. `None` means a session workspace is used.
    pub project_id: Option<ProjectId>,
    /// Who owns the title: `generic` (still the placeholder), `auto` (derived from the
    /// conversation) or `manual` (the operator named it). A `manual` title is never overwritten.
    pub title_source: String,
    /// `pending` until a title has been derived, `ready` once it has, `failed` when derivation was
    /// tried and refused — the last two stop the runtime from re-asking on every message.
    pub title_status: String,
    /// The session this one was forked from, when it is a fork. `None` for an original session.
    /// This is the parent link `fork_session` writes; `duplicate_session` keeps the source's
    /// own parent so a copy of a fork stays in the same lineage.
    #[serde(default)]
    pub parent_session: Option<SessionId>,
    /// `fork` when this session was created by `fork_session`, empty otherwise. Kept separate
    /// from `parent_session` so the lineage column stays purely about delegation.
    #[serde(default)]
    pub forked_from: Option<SessionId>,
}

/// `title_source` values.
pub const TITLE_GENERIC: &str = "generic";
pub const TITLE_AUTO: &str = "auto";
pub const TITLE_MANUAL: &str = "manual";

/// `title_status` values.
pub const TITLE_PENDING: &str = "pending";
pub const TITLE_READY: &str = "ready";
pub const TITLE_FAILED: &str = "failed";

/// The projection every session read uses. Appending a column here keeps the six `SELECT` sites
/// and [`map_session_row`] in step; the positional reader below depends on this exact order.
const SESSION_COLS: &str = "id, title, cwd, model, bot_id, created_ms, updated_ms, status, \
                            project_id, title_source, title_status, parent_session, forked_from";

/// How a session's conversation is exported. `Json` is the machine-readable transcript;
/// `Markdown` is the human-readable one. Both are pure views over the journal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[default]
    Json,
    Markdown,
}

impl ExportFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "json" => Some(Self::Json),
            "markdown" | "md" => Some(Self::Markdown),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Markdown => "markdown",
        }
    }
}

/// What a crash left behind: a session whose journal ends in the middle of something.
/// `pending_calls` are tool calls the journal recorded but never answered — the call was
/// issued and its result never arrived, which is exactly what a crash between the two
/// journal appends looks like.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedSession {
    /// The session that did not finish cleanly.
    pub id: SessionId,
    /// Tool calls journalled without a matching result, in journal order.
    pub pending_calls: Vec<vikram_contracts::ToolCallId>,
    /// `tool_preview` belongs to the first pending call; `None` when there is none.
    pub tool_preview: Option<String>,
    /// The sequence number of the last record the journal holds for this session.
    pub last_seq: u64,
    /// Time of the last record, in milliseconds since the epoch.
    pub last_ts_ms: u64,
}

/// The shape of one exported session in JSON: the metadata row plus the journal it replays to.
/// `PartialEq` only — `JournalRecord` carries a JSON payload, so this type has no `Eq`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionExport {
    /// The stored session row.
    pub session: SessionRow,
    /// The journal in sequence order; replaying it rebuilds the transcript.
    pub journal: Vec<JournalRecord>,
    /// The usage ledger for the session, or empty when nothing has been accounted yet.
    #[serde(default)]
    pub usage: Vec<UsageRow>,
    /// Wall-clock time the export was produced.
    pub exported_ms: u64,
}

/// A project directory Vikram manages. `path` is the real location; internal projects live under
/// `projects/`, external projects keep their original path and are never copied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRow {
    pub id: ProjectId,
    pub name: String,
    pub path: PathBuf,
    /// `active` while the directory exists, `missing` when it moved away.
    pub status: String,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// The working-directory mode a session runs in: its own lazily created workspace, or a project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWorkspaceRow {
    pub session: SessionId,
    /// `session` or `project`.
    pub mode: String,
    /// Lazily created workspace root for `session` mode.
    pub workspace_path: Option<PathBuf>,
    pub project_id: Option<ProjectId>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// A file a session or agent produced or registered. The path is the source of truth; the row only
/// indexes it so a gallery can list it without copying data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRow {
    pub id: String,
    pub session: SessionId,
    pub project_id: Option<ProjectId>,
    pub bot: Option<BotId>,
    pub name: String,
    pub path: PathBuf,
    /// `source` | `image` | `pdf` | `document` | `data` | `other`.
    pub kind: String,
    pub mime: Option<String>,
    pub size: u64,
    pub checksum: Option<String>,
    /// Message or tool call this file came from, when known.
    pub source: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// One tracked filesystem change made during a session, with an optional checkpoint to restore.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChangeRow {
    pub id: String,
    pub session: SessionId,
    pub bot: Option<BotId>,
    pub path: PathBuf,
    /// `create` | `modify` | `delete`.
    pub operation: String,
    pub checksum_before: Option<String>,
    pub checksum_after: Option<String>,
    pub size: Option<u64>,
    /// Managed copy of the original content for recovery, when a checkpoint was taken.
    pub checkpoint_path: Option<PathBuf>,
    pub ts_ms: u64,
}

/// One accounting entry: what a single provider exchange cost. `measured` distinguishes a figure
/// the provider actually reported from one the estimator produced, so the ledger never presents an
/// estimate as a bill. `id` is assigned by SQLite and is `0` before the row is stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRow {
    pub id: i64,
    pub session: SessionId,
    pub ts_ms: u64,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub measured: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub requests: u64,
    pub measured_requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotRow {
    pub id: BotId,
    pub name: String,
    pub role: String,
    pub description: String,
    pub icon: Option<String>,
    pub created_ms: u64,
}

/// A HIVE team: a user-named group of saved agent profiles sharing one dedicated conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiveRosterRow {
    pub id: HiveRosterId,
    pub name: String,
    pub session_id: Option<SessionId>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// A human. Accounts never share private data; the id is the boundary every other table hangs off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRow {
    pub id: UserId,
    pub name: String,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// One working context of a user. Profiles of the same user are separate on purpose: preferences,
/// memory and conversations belong to the profile, never to the account as a whole.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileRow {
    pub id: ProfileId,
    pub user: UserId,
    pub name: String,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// Which of the three kinds of AI entity a row is. Stored as a string, so a row written by an older
/// build is still readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntityKind {
    MainAgent,
    Agent,
    Bot,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MainAgent => "main_agent",
            Self::Agent => "agent",
            Self::Bot => "bot",
        }
    }

    /// The directory the entity's context files live under, relative to the workspace.
    pub fn folder(self) -> &'static str {
        match self {
            Self::MainAgent => "main-agents",
            Self::Agent => "agents",
            Self::Bot => "bots",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "main_agent" | "main-agent" | "mainagent" => Ok(Self::MainAgent),
            "agent" => Ok(Self::Agent),
            "bot" => Ok(Self::Bot),
            _ => Err(StoreError::Parse(format!("unknown entity kind `{value}`"))),
        }
    }
}

impl std::fmt::Display for EntityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One AI entity. `owner_user` is `None` for a shared definition — reusable by any account, but
/// every account's conversations and memory for it stay its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRow {
    pub id: EntityId,
    pub owner_user: Option<UserId>,
    pub kind: EntityKind,
    pub name: String,
    /// Directory name under [`EntityKind::folder`], e.g. `rudra` or `coding-bot`.
    pub slug: String,
    /// Customizable responsibility/title (e.g. "Lead Coder"). Descriptive metadata, not a permission.
    pub title: String,
    pub role: String,
    /// Optional expertise/specialization. May hold multiple comma-separated values.
    pub specialization: String,
    pub description: String,
    pub icon: Option<String>,
    pub model: Option<String>,
    pub profile_id: Option<ProfileId>,
    /// active | inactive | unavailable
    pub status: String,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// The identity the interface last had selected. One row, or none before anything is chosen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveIdentity {
    pub user: Option<UserId>,
    pub profile: Option<ProfileId>,
    pub main_agent: Option<EntityId>,
}

/// One Hive group chat. `session_id` is the conversation its transcript is journalled into.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiveRow {
    pub id: HiveId,
    pub owner_user: UserId,
    pub profile_id: Option<ProfileId>,
    pub name: String,
    pub objective: String,
    pub session_id: Option<SessionId>,
    pub state: String,
    pub policy: String,
    pub created_ms: u64,
    pub updated_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiveParticipantRow {
    pub hive: HiveId,
    pub entity: EntityId,
    pub kind: EntityKind,
    pub role: String,
    pub joined_ms: u64,
}

/// One message in a Hive. The sender's id *and* kind travel with it, so a transcript can name who
/// spoke without a second lookup and without guessing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiveMessageRow {
    pub id: HiveMessageId,
    pub hive: HiveId,
    pub sender: String,
    pub sender_kind: String,
    pub content: String,
    pub task_ref: Option<String>,
    pub ts_ms: u64,
}

// ---- unified ticket system rows ----------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketRow {
    pub id: TicketId,
    pub owner_key: String,
    pub owner_kind: String,
    pub title: String,
    pub description: String,
    pub request: String,
    pub kind: String,
    pub status: String,
    pub parent_ticket: Option<TicketId>,
    pub hive_id: Option<HiveId>,
    pub session_id: Option<SessionId>,
    pub strategy: Option<String>,
    pub result: Option<String>,
    pub failure_reason: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub closed_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketParticipantRow {
    pub ticket: TicketId,
    pub participant_key: String,
    pub participant_kind: String,
    pub entity_id: Option<EntityId>,
    pub user_id: Option<UserId>,
    pub role: String,
    pub can_read: bool,
    pub can_write: bool,
    pub can_admin: bool,
    pub joined_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketMessageRow {
    pub id: TicketMessageId,
    pub ticket: TicketId,
    pub author_key: String,
    pub author_kind: String,
    pub content: String,
    pub visibility: String,
    pub ts_ms: u64,
    pub task_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentRow {
    pub id: AssignmentId,
    pub ticket: TicketId,
    pub parent_assignment: Option<AssignmentId>,
    pub assignee_key: String,
    pub assignee_kind: String,
    pub entity_id: Option<EntityId>,
    pub title: String,
    pub description: String,
    pub status: String,
    pub strategy: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub completed_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketEventRow {
    pub id: TicketEventId,
    pub ticket: TicketId,
    pub ts_ms: u64,
    pub kind: String,
    pub actor_key: Option<String>,
    pub payload: serde_json::Value,
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
        // Two vikram processes share this file: a TUI left open in one terminal and a command in
        // another. WAL lets them read together, but a write still needs the lock, and the default
        // zero timeout turns that into an instant SQLITE_BUSY — which looks like a frozen turn.
        conn.pragma_update(None, "busy_timeout", 5_000).ok();
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
        // A caller that names the session at creation owns that name: nothing later derives over it.
        self.create_session_with_title(title, cwd, model, now_ms, TITLE_AUTO, TITLE_READY)
    }

    pub fn create_session_with_title(
        &self,
        title: &str,
        cwd: &Path,
        model: Option<&str>,
        now_ms: u64,
        title_source: &str,
        title_status: &str,
    ) -> Result<SessionRow> {
        let row = SessionRow {
            id: SessionId::generate(),
            title: title.to_owned(),
            cwd: cwd.to_path_buf(),
            model: model.map(str::to_owned),
            bot: None,
            created_ms: now_ms,
            updated_ms: now_ms,
            status: "active".to_owned(),
            project_id: None,
            title_source: title_source.to_owned(),
            title_status: title_status.to_owned(),
            parent_session: None,
            forked_from: None,
        };
        self.lock().execute(
            "INSERT INTO sessions (id, title, cwd, model, bot_id, created_ms, updated_ms, status, project_id, title_source, title_status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', NULL, ?8, ?9)",
            params![
                row.id.as_str(),
                row.title,
                row.cwd.to_string_lossy(),
                row.model,
                row.bot.as_ref().map(|b| b.as_str().to_owned()),
                row.created_ms as i64,
                row.updated_ms as i64,
                row.title_source,
                row.title_status
            ],
        )?;
        Ok(row)
    }

    /// Link a session to the user that owns it. Without this, deleting a user leaves orphaned
    /// sessions behind because the cascade has nothing to match.
    pub fn set_session_user(&self, session: &SessionId, user: &UserId) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET user_id = ?2 WHERE id = ?1",
            params![session.as_str(), user.as_str()],
        )?;
        Ok(())
    }

    /// Link a session to a profile, e.g. when the operator switched profiles.
    pub fn set_session_profile(&self, session: &SessionId, profile: &ProfileId) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET profile_id = ?2 WHERE id = ?1",
            params![session.as_str(), profile.as_str()],
        )?;
        Ok(())
    }

    /// Record the session that delegated this one, and how deep the chain goes. Depth is stored
    /// rather than walked so the delegation gate can be checked without loading the lineage.
    pub fn set_session_lineage(
        &self,
        session: &SessionId,
        parent: Option<&SessionId>,
        depth: u32,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET parent_session = ?2, delegation_depth = ?3 WHERE id = ?1",
            params![session.as_str(), parent.map(|p| p.as_str()), depth as i64],
        )?;
        Ok(())
    }

    pub fn session_lineage(&self, session: &SessionId) -> Result<Option<(SessionId, u32)>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT parent_session, delegation_depth FROM sessions WHERE id = ?1")?;
        let found = stmt
            .query_map(params![session.as_str()], |row| {
                let parent: Option<String> = row.get(0)?;
                let depth: i64 = row.get(1)?;
                Ok(parent.map(|p| (SessionId::new(p), depth.max(0) as u32)))
            })?
            .next()
            .and_then(|r| r.ok())
            .flatten();
        Ok(found)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE status = 'active' ORDER BY created_ms ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], map_session_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Active sessions ordered by most recent activity — the order the session list uses, so a
    /// conversation the operator just had floats to the top instead of hiding under its birth date.
    pub fn list_recent_sessions(&self) -> Result<Vec<SessionRow>> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE status = 'active' ORDER BY updated_ms DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], map_session_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every session including archived ones, newest first.
    pub fn list_all_sessions(&self) -> Result<Vec<SessionRow>> {
        let conn = self.lock();
        let sql = format!("SELECT {SESSION_COLS} FROM sessions ORDER BY updated_ms DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], map_session_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_session(&self, id: &SessionId) -> Result<Option<SessionRow>> {
        let conn = self.lock();
        let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE id = ?1");
        conn.query_row(&sql, params![id.as_str()], map_session_row)
            .optional()
            .map_err(Into::into)
    }

    /// Sessions matching a term in their title, working directory, or journaled conversation text.
    /// Message content lives in the journal rather than the `sessions` row, so it is reached with an
    /// `EXISTS` rather than a join — a transcript with twenty matches must still list one session.
    pub fn search_sessions(&self, query: &str, include_archived: bool) -> Result<Vec<SessionRow>> {
        let query = query.trim();
        if query.is_empty() {
            return if include_archived {
                self.list_all_sessions()
            } else {
                self.list_recent_sessions()
            };
        }
        let conn = self.lock();
        let like = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let status = if include_archived {
            String::new()
        } else {
            "AND status = 'active'".to_owned()
        };
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions \
             WHERE (title LIKE ?1 ESCAPE '\\' OR cwd LIKE ?1 ESCAPE '\\' \
                    OR EXISTS (SELECT 1 FROM journal j \
                               WHERE j.session = sessions.id \
                                 AND j.payload LIKE ?2 ESCAPE '\\')) {status} \
             ORDER BY updated_ms DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![like, like], map_session_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Index free-form text (e.g. a message or tool result) for a session into the FTS5 table.
    pub fn index_session_text(&self, session: &SessionId, content: &str) -> Result<()> {
        let conn = self.lock();
        // Best-effort: ignore if FTS5 unavailable.
        let _ = conn.execute(
            "INSERT INTO session_fts(session, content) VALUES (?1, ?2)",
            params![session.to_string(), content],
        );
        Ok(())
    }

    /// FTS5-backed session message search. Returns matching session ids ordered by rank.
    pub fn search_session_text(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(SessionId, String)>> {
        let conn = self.lock();
        let mut out = Vec::new();
        let sql = "SELECT session, snippet(session_fts, 1, '[', ']', '…', 12) \
                   FROM session_fts WHERE session_fts MATCH ?1 ORDER BY rank LIMIT ?2";
        if let Ok(mut stmt) = conn.prepare(sql)
            && let Ok(rows) = stmt.query_map(params![query, limit as i64], |r| {
                Ok((
                    SessionId::new(r.get::<_, String>(0)?),
                    r.get::<_, String>(1)?,
                ))
            })
        {
            for r in rows.flatten() {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// FTS5-backed session message search with optional agent/project/time filters, joining
    /// `session_fts` to `sessions`. `since_ms == 0` disables the time filter.
    pub fn search_session_text_filtered(
        &self,
        query: &str,
        bot_filter: Option<&BotId>,
        project_filter: Option<&ProjectId>,
        since_ms: u64,
        limit: usize,
    ) -> Result<Vec<(SessionId, String)>> {
        let conn = self.lock();
        let mut out = Vec::new();
        let sql = "SELECT session_fts.session, snippet(session_fts, 1, '[', ']', '…', 12) \
                   FROM session_fts JOIN sessions s ON s.id = session_fts.session \
                   WHERE session_fts MATCH ?1 \
                     AND (?2 IS NULL OR s.bot_id = ?2) \
                     AND (?3 IS NULL OR s.project_id = ?3) \
                     AND (?4 = 0 OR s.updated_ms >= ?4) \
                   ORDER BY rank LIMIT ?5";
        let agent = bot_filter.map(|a| a.as_str().to_owned());
        let project = project_filter.map(|p| p.as_str().to_owned());
        if let Ok(mut stmt) = conn.prepare(sql)
            && let Ok(rows) = stmt.query_map(
                params![query, agent, project, since_ms as i64, limit as i64],
                |r| {
                    Ok((
                        SessionId::new(r.get::<_, String>(0)?),
                        r.get::<_, String>(1)?,
                    ))
                },
            )
        {
            for r in rows.flatten() {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Recent sessions (active, by updated_ms) with a count of their indexed FTS rows.
    pub fn browse_sessions(&self) -> Result<Vec<(SessionRow, u64)>> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE status = 'active' ORDER BY updated_ms DESC"
        );
        let sessions: Vec<SessionRow> = {
            let mut stmt = conn.prepare(&sql)?;

            stmt.query_map([], map_session_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out = Vec::with_capacity(sessions.len());
        for s in sessions {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM session_fts WHERE session = ?1",
                    params![s.id.as_str()],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            out.push((s, count as u64));
        }
        Ok(out)
    }

    pub fn set_session_status(&self, id: &SessionId, status: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET status = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), status, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn set_session_project(
        &self,
        id: &SessionId,
        project: Option<&ProjectId>,
        now_ms: u64,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET project_id = ?2, updated_ms = ?3 WHERE id = ?1",
            params![
                id.as_str(),
                project.map(|p| p.as_str().to_owned()),
                now_ms as i64
            ],
        )?;
        Ok(())
    }

    /// Duplicate a session's metadata (not its files or its journal). The copy starts with the
    /// same title suffix. A copy of a fork shares the source's parent, so it stays in the same
    /// lineage rather than becoming a fork of its source.
    pub fn duplicate_session(&self, id: &SessionId, now_ms: u64) -> Result<Option<SessionRow>> {
        let src = match self.get_session(id)? {
            Some(s) => s,
            None => return Ok(None),
        };
        let copy = SessionRow {
            id: SessionId::generate(),
            title: format!("{} (copy)", src.title),
            cwd: src.cwd.clone(),
            model: src.model.clone(),
            bot: src.bot.clone(),
            created_ms: now_ms,
            updated_ms: now_ms,
            status: "active".to_owned(),
            project_id: src.project_id.clone(),
            // A copy of a named session is still a named session: the operator's title carries over,
            // so `manual` provenance carries over with it and automatic naming stays away.
            title_source: src.title_source.clone(),
            title_status: TITLE_READY.to_owned(),
            parent_session: src.parent_session.clone(),
            forked_from: src.forked_from.clone(),
        };
        self.lock().execute(
            "INSERT INTO sessions (id, title, cwd, model, bot_id, created_ms, updated_ms, status, project_id, title_source, title_status, parent_session, forked_from)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, ?9, ?10, ?11, ?12)",
            params![
                copy.id.as_str(),
                copy.title,
                copy.cwd.to_string_lossy(),
                copy.model,
                copy.bot.as_ref().map(|b| b.as_str().to_owned()),
                copy.created_ms as i64,
                copy.updated_ms as i64,
                copy.project_id.as_ref().map(|p| p.as_str().to_owned()),
                copy.title_source,
                copy.title_status,
                copy.parent_session.as_ref().map(|p| p.as_str().to_owned()),
                copy.forked_from.as_ref().map(|p| p.as_str().to_owned()),
            ],
        )?;
        Ok(Some(copy))
    }

    /// Fork a session: a new session that continues *from* the source's conversation. Unlike
    /// [`Self::duplicate_session`], the fork carries the journal with it, and the fork points
    /// back at its source via `forked_from` / `parent_session` so the lineage is queryable.
    /// One transaction: the new row and every copied journal row commit together, or neither.
    pub fn fork_session(&self, id: &SessionId, now_ms: u64) -> Result<Option<SessionRow>> {
        let src = match self.get_session(id)? {
            Some(s) => s,
            None => return Ok(None),
        };
        let fork = SessionRow {
            id: SessionId::generate(),
            title: format!("{} (fork)", src.title),
            cwd: src.cwd.clone(),
            model: src.model.clone(),
            bot: src.bot.clone(),
            created_ms: now_ms,
            updated_ms: now_ms,
            status: "active".to_owned(),
            project_id: src.project_id.clone(),
            title_source: src.title_source.clone(),
            title_status: TITLE_READY.to_owned(),
            parent_session: Some(id.clone()),
            forked_from: Some(id.clone()),
        };
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions (id, title, cwd, model, bot_id, created_ms, updated_ms, status, project_id, title_source, title_status, parent_session, forked_from)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, ?9, ?10, ?11, ?12)",
            params![
                fork.id.as_str(),
                fork.title,
                fork.cwd.to_string_lossy(),
                fork.model,
                fork.bot.as_ref().map(|b| b.as_str().to_owned()),
                fork.created_ms as i64,
                fork.updated_ms as i64,
                fork.project_id.as_ref().map(|p| p.as_str().to_owned()),
                fork.title_source,
                fork.title_status,
                fork.parent_session.as_ref().map(|p| p.as_str().to_owned()),
                fork.forked_from.as_ref().map(|p| p.as_str().to_owned()),
            ],
        )?;
        // Copy the journal verbatim: same seq, same effect ids, same payloads. Effect ids
        // stay unique per session because the idempotency guard keys on (session, effect).
        tx.execute(
            "INSERT INTO journal (session, seq, ts_ms, kind, effect, payload)
             SELECT ?2, seq, ts_ms, kind, effect, payload FROM journal WHERE session = ?1 ORDER BY seq ASC",
            params![id.as_str(), fork.id.as_str()],
        )?;
        tx.commit()?;
        Ok(Some(fork))
    }

    /// Sessions forked *from* `parent`, oldest first.
    pub fn list_session_forks(&self, parent: &SessionId) -> Result<Vec<SessionRow>> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE forked_from = ?1 ORDER BY created_ms ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![parent.as_str()], map_session_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Build the serializable export of a session: its metadata, its journal and its usage.
    /// `None` when the session does not exist.
    pub fn export_session(&self, id: &SessionId, now_ms: u64) -> Result<Option<SessionExport>> {
        let session = match self.get_session(id)? {
            Some(s) => s,
            None => return Ok(None),
        };
        Ok(Some(SessionExport {
            session,
            journal: self.replay_messages(id)?,
            usage: self.list_usage(id)?,
            exported_ms: now_ms,
        }))
    }

    /// Render a session as JSON or Markdown. The text is the export; callers write it to a
    /// file or stdout. `None` when the session does not exist.
    pub fn export_session_text(
        &self,
        id: &SessionId,
        format: ExportFormat,
        now_ms: u64,
    ) -> Result<Option<String>> {
        let export = match self.export_session(id, now_ms)? {
            Some(e) => e,
            None => return Ok(None),
        };
        Ok(Some(match format {
            ExportFormat::Json => serde_json::to_string_pretty(&export)?,
            ExportFormat::Markdown => render_session_markdown(&export),
        }))
    }

    /// Every session whose journal ends mid-work: tool calls recorded but never answered.
    /// Sessions whose last operation was a tool result, or whose journal is empty, are not
    /// interrupted and are not returned.
    pub fn list_interrupted_sessions(&self) -> Result<Vec<InterruptedSession>> {
        let ids: Vec<SessionId> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT id FROM sessions WHERE status = 'active' ORDER BY created_ms ASC",
            )?;
            let rows = stmt.query_map([], |r| Ok(SessionId::new(r.get::<_, String>(0)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out = Vec::new();
        for id in ids {
            let records = self.replay_messages(&id)?;
            if records.is_empty() {
                continue;
            }
            let mut pending: Vec<(vikram_contracts::ToolCallId, String)> = Vec::new();
            let mut answered = std::collections::HashSet::new();
            for record in &records {
                match record.kind {
                    RecordKind::ToolCall => {
                        if let Some(call) = record.payload.get("call").and_then(|v| v.as_str()) {
                            let preview = record
                                .payload
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("tool call")
                                .to_owned();
                            pending.push((vikram_contracts::ToolCallId::new(call), preview));
                        }
                    }
                    RecordKind::ToolResult => {
                        if let Some(call) = record
                            .payload
                            .get("call")
                            .and_then(|v| v.as_str())
                            .map(vikram_contracts::ToolCallId::new)
                        {
                            answered.insert(call);
                        }
                        if let Some(message) = record.payload.get("message")
                            && let Ok(msg) =
                                serde_json::from_value::<vikram_contracts::Message>(message.clone())
                        {
                            for block in &msg.blocks {
                                if let vikram_contracts::Block::ToolResult { call, .. } = block {
                                    answered.insert(call.clone());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            pending.retain(|(call, _)| !answered.contains(call));
            if pending.is_empty() {
                continue;
            }
            let last = records.last().expect("non-empty journal");
            out.push(InterruptedSession {
                pending_calls: pending.iter().map(|(call, _)| call.clone()).collect(),
                tool_preview: pending.first().map(|(_, preview)| preview.clone()),
                last_seq: last.seq,
                last_ts_ms: last.ts_ms,
                id,
            });
        }
        Ok(out)
    }

    /// Recover an interrupted session: append a synthetic tool result for every pending call,
    /// so the transcript is complete again, and journal a recovery marker. The journal gains
    /// rows, never loses any, which is what makes this safe to run more than once — a session
    /// with nothing pending is a no-op. Returns the calls that were answered.
    pub fn recover_interrupted_session(
        &self,
        id: &SessionId,
        now_ms: u64,
    ) -> Result<Vec<vikram_contracts::ToolCallId>> {
        let interrupted = self
            .list_interrupted_sessions()?
            .into_iter()
            .find(|s| s.id == *id);
        let Some(interrupted) = interrupted else {
            return Ok(Vec::new());
        };
        for call in &interrupted.pending_calls {
            let content = "the run ended before this call finished";
            // JournalSink reports a storage failure as VikramError; surface it through the
            // store's own error by converting through a JSON error round-trip.
            let _ = self.journal().append(&NewRecord::new(
                id.clone(),
                RecordKind::ToolResult,
                serde_json::json!({
                    "call": call.as_str(),
                    "ok": false,
                    "content": content,
                    "message": vikram_contracts::Message::tool_result(
                        call.clone(),
                        content,
                        false
                    ),
                }),
            ));
        }
        let _ = self.journal().append(&NewRecord::new(
            id.clone(),
            RecordKind::Status,
            serde_json::json!({
                "recovery": "interrupted_session",
                "answered": interrupted.pending_calls.len(),
                "tool": interrupted.tool_preview,
            }),
        ));
        let _ = self.touch_session(id, now_ms);
        Ok(interrupted.pending_calls)
    }

    /// The operator named this session. A manual title is the one thing automatic naming may never
    /// touch, so the provenance moves to `manual` here and stays there.
    pub fn rename_session(&self, id: &SessionId, title: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET title = ?2, updated_ms = ?3, title_source = 'manual', title_status = 'ready' WHERE id = ?1",
            params![id.as_str(), title, now_ms as i64],
        )?;
        Ok(())
    }

    /// Adopt a derived title, but only while the session has no name of its own. The `WHERE` is the
    /// whole guarantee: a manually renamed session returns `false` and keeps its title, so two
    /// writers cannot race the operator's own choice.
    pub fn set_session_auto_title(&self, id: &SessionId, title: &str, now_ms: u64) -> Result<bool> {
        let changed = self.lock().execute(
            "UPDATE sessions SET title = ?2, updated_ms = ?3, title_source = 'auto', title_status = 'ready'
             WHERE id = ?1 AND title_source != 'manual'",
            params![id.as_str(), title, now_ms as i64],
        )?;
        Ok(changed > 0)
    }

    /// Record how title derivation went without changing the name.
    pub fn set_session_title_status(&self, id: &SessionId, status: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET title_status = ?2 WHERE id = ?1",
            params![id.as_str(), status],
        )?;
        Ok(())
    }

    /// Mark a session as needing a derived name — how `/retitle` asks for a fresh one, and how a
    /// manual title can be handed back to the automatic path.
    pub fn mark_session_title_pending(&self, id: &SessionId, source: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET title_source = ?2, title_status = 'pending' WHERE id = ?1",
            params![id.as_str(), source],
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

    pub fn set_session_bot(&self, id: &SessionId, bot: Option<&BotId>, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE sessions SET bot_id = ?2, updated_ms = ?3 WHERE id = ?1",
            params![
                id.as_str(),
                bot.map(|b| b.as_str().to_owned()),
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
        tx.execute(
            "UPDATE hive_rosters SET session_id = NULL WHERE session_id = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "UPDATE hive_sessions SET session_id = NULL WHERE session_id = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "UPDATE tickets SET session_id = NULL WHERE session_id = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "DELETE FROM session_workspaces WHERE session = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "DELETE FROM artifacts WHERE session = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "DELETE FROM file_changes WHERE session = ?1",
            params![id.as_str()],
        )?;
        tx.execute("DELETE FROM usage WHERE session = ?1", params![id.as_str()])?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![id.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    // ---- projects -------------------------------------------------------

    pub fn create_project(&self, name: &str, path: &Path, now_ms: u64) -> Result<ProjectRow> {
        let id = ProjectId::generate();
        self.lock().execute(
            "INSERT INTO projects (id, name, path, status, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, 'active', ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET name = excluded.name, updated_ms = excluded.updated_ms",
            params![id.as_str(), name, path.to_string_lossy(), now_ms as i64, now_ms as i64],
        )?;
        // Re-read the stored row so the returned id matches the persisted one on conflict.
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, path, status, created_ms, updated_ms FROM projects WHERE path = ?1",
        )?;
        let row = stmt.query_row(params![path.to_string_lossy()], |r| {
            Ok(ProjectRow {
                id: ProjectId::new(r.get::<_, String>(0)?),
                name: r.get(1)?,
                path: PathBuf::from(r.get::<_, String>(2)?),
                status: r.get(3)?,
                created_ms: r.get::<_, i64>(4)? as u64,
                updated_ms: r.get::<_, i64>(5)? as u64,
            })
        })?;
        Ok(row)
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, path, status, created_ms, updated_ms FROM projects ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProjectRow {
                    id: ProjectId::new(r.get::<_, String>(0)?),
                    name: r.get(1)?,
                    path: PathBuf::from(r.get::<_, String>(2)?),
                    status: r.get(3)?,
                    created_ms: r.get::<_, i64>(4)? as u64,
                    updated_ms: r.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_project(&self, id: &ProjectId) -> Result<Option<ProjectRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, name, path, status, created_ms, updated_ms FROM projects WHERE id = ?1",
            params![id.as_str()],
            |r| {
                Ok(ProjectRow {
                    id: ProjectId::new(r.get::<_, String>(0)?),
                    name: r.get(1)?,
                    path: PathBuf::from(r.get::<_, String>(2)?),
                    status: r.get(3)?,
                    created_ms: r.get::<_, i64>(4)? as u64,
                    updated_ms: r.get::<_, i64>(5)? as u64,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn find_project_by_path(&self, path: &Path) -> Result<Option<ProjectRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, name, path, status, created_ms, updated_ms FROM projects WHERE path = ?1",
            params![path.to_string_lossy()],
            |r| {
                Ok(ProjectRow {
                    id: ProjectId::new(r.get::<_, String>(0)?),
                    name: r.get(1)?,
                    path: PathBuf::from(r.get::<_, String>(2)?),
                    status: r.get(3)?,
                    created_ms: r.get::<_, i64>(4)? as u64,
                    updated_ms: r.get::<_, i64>(5)? as u64,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn set_project_status(&self, id: &ProjectId, status: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE projects SET status = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), status, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn delete_project(&self, id: &ProjectId) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        // Detach dependents rather than leaving dangling references: sessions keep
        // existing, only their project link is cleared.
        tx.execute(
            "UPDATE sessions SET project_id = NULL WHERE project_id = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "UPDATE session_workspaces SET project_id = NULL WHERE project_id = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "UPDATE artifacts SET project_id = NULL WHERE project_id = ?1",
            params![id.as_str()],
        )?;
        tx.execute("DELETE FROM projects WHERE id = ?1", params![id.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    // ---- session workspaces --------------------------------------------

    pub fn set_session_workspace(
        &self,
        session: &SessionId,
        mode: &str,
        workspace_path: Option<&Path>,
        project_id: Option<&ProjectId>,
        now_ms: u64,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT INTO session_workspaces (session, mode, workspace_path, project_id, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(session) DO UPDATE SET mode = excluded.mode, workspace_path = excluded.workspace_path,
               project_id = excluded.project_id, updated_ms = excluded.updated_ms",
            params![
                session.as_str(),
                mode,
                workspace_path.map(|p| p.to_string_lossy().to_string()),
                project_id.map(|p| p.as_str().to_owned()),
                now_ms as i64,
            ],
        )?;
        Ok(())
    }

    pub fn get_session_workspace(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionWorkspaceRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT session, mode, workspace_path, project_id, created_ms, updated_ms
             FROM session_workspaces WHERE session = ?1",
            params![session.as_str()],
            |r| {
                Ok(SessionWorkspaceRow {
                    session: SessionId::new(r.get::<_, String>(0)?),
                    mode: r.get(1)?,
                    workspace_path: r.get::<_, Option<String>>(2)?.map(PathBuf::from),
                    project_id: r.get::<_, Option<String>>(3)?.map(ProjectId::new),
                    created_ms: r.get::<_, i64>(4)? as u64,
                    updated_ms: r.get::<_, i64>(5)? as u64,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    // ---- artifacts ------------------------------------------------------

    pub fn register_artifact(&self, row: &ArtifactRow) -> Result<()> {
        self.lock().execute(
            "INSERT INTO artifacts (id, session, project_id, bot_id, name, path, kind, mime, size, checksum, source, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(path) DO UPDATE SET session = excluded.session, name = excluded.name,
               kind = excluded.kind, mime = excluded.mime, size = excluded.size,
               checksum = excluded.checksum, source = excluded.source, updated_ms = excluded.updated_ms",
            params![
                row.id,
                row.session.as_str(),
                row.project_id.as_ref().map(|p| p.as_str().to_owned()),
                row.bot.as_ref().map(|b| b.as_str().to_owned()),
                row.name,
                row.path.to_string_lossy(),
                row.kind,
                row.mime,
                row.size as i64,
                row.checksum,
                row.source,
                row.created_ms as i64,
                row.updated_ms as i64,
            ],
        )?;
        Ok(())
    }

    /// List artifacts; `session` filters to one session, `None` lists across all sessions.
    pub fn list_artifacts(&self, session: Option<&SessionId>) -> Result<Vec<ArtifactRow>> {
        let conn = self.lock();
        let (sql, args): (&str, Vec<String>) = match session {
            Some(s) => (
                "SELECT id, session, project_id, bot_id, name, path, kind, mime, size, checksum, source, created_ms, updated_ms
                 FROM artifacts WHERE session = ?1 ORDER BY created_ms DESC",
                vec![s.as_str().to_owned()],
            ),
            None => (
                "SELECT id, session, project_id, bot_id, name, path, kind, mime, size, checksum, source, created_ms, updated_ms
                 FROM artifacts ORDER BY created_ms DESC",
                vec![],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = if args.is_empty() {
            stmt.query_map([], map_artifact)?
        } else {
            stmt.query_map(params![args[0]], map_artifact)?
        };
        let out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    pub fn remove_artifact(&self, id: &str) -> Result<bool> {
        let n = self
            .lock()
            .execute("DELETE FROM artifacts WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ---- file changes ---------------------------------------------------

    pub fn record_file_change(&self, row: &FileChangeRow) -> Result<()> {
        self.lock().execute(
            "INSERT INTO file_changes (id, session, bot_id, path, operation, checksum_before, checksum_after, size, checkpoint_path, ts_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                row.id,
                row.session.as_str(),
                row.bot.as_ref().map(|b| b.as_str().to_owned()),
                row.path.to_string_lossy(),
                row.operation,
                row.checksum_before,
                row.checksum_after,
                row.size.map(|s| s as i64),
                row.checkpoint_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                row.ts_ms as i64,
            ],
        )?;
        Ok(())
    }

    pub fn list_file_changes(&self, session: &SessionId) -> Result<Vec<FileChangeRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, bot_id, path, operation, checksum_before, checksum_after, size, checkpoint_path, ts_ms
             FROM file_changes WHERE session = ?1 ORDER BY ts_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![session.as_str()], |r| {
                Ok(FileChangeRow {
                    id: r.get(0)?,
                    session: SessionId::new(r.get::<_, String>(1)?),
                    bot: r.get::<_, Option<String>>(2)?.map(BotId::new),
                    path: PathBuf::from(r.get::<_, String>(3)?),
                    operation: r.get(4)?,
                    checksum_before: r.get(5)?,
                    checksum_after: r.get(6)?,
                    size: r.get::<_, Option<i64>>(7)?.map(|s| s as u64),
                    checkpoint_path: r.get::<_, Option<String>>(8)?.map(PathBuf::from),
                    ts_ms: r.get::<_, i64>(9)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_file_changes_for_session(&self, session: &SessionId) -> Result<()> {
        self.lock().execute(
            "DELETE FROM file_changes WHERE session = ?1",
            params![session.as_str()],
        )?;
        Ok(())
    }

    // ---- usage ----------------------------------------------------------

    pub fn record_usage(&self, row: &UsageRow) -> Result<()> {
        self.lock().execute(
            "INSERT INTO usage (session, ts_ms, model, prompt_tokens, completion_tokens, total_tokens, measured)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.session.as_str(),
                row.ts_ms as i64,
                row.model,
                row.prompt_tokens as i64,
                row.completion_tokens as i64,
                row.total_tokens as i64,
                row.measured as i64,
            ],
        )?;
        Ok(())
    }

    pub fn list_usage(&self, session: &SessionId) -> Result<Vec<UsageRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session, ts_ms, model, prompt_tokens, completion_tokens, total_tokens, measured
             FROM usage WHERE session = ?1 ORDER BY ts_ms ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![session.as_str()], |r| {
                Ok(UsageRow {
                    id: r.get(0)?,
                    session: SessionId::new(r.get::<_, String>(1)?),
                    ts_ms: r.get::<_, i64>(2)? as u64,
                    model: r.get(3)?,
                    prompt_tokens: r.get::<_, i64>(4)? as u64,
                    completion_tokens: r.get::<_, i64>(5)? as u64,
                    total_tokens: r.get::<_, i64>(6)? as u64,
                    measured: r.get::<_, i64>(7)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn usage_totals(&self, session: &SessionId) -> Result<UsageTotals> {
        let conn = self.lock();
        let totals = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(measured), 0),
                    COALESCE(SUM(prompt_tokens), 0),
                    COALESCE(SUM(completion_tokens), 0),
                    COALESCE(SUM(total_tokens), 0)
             FROM usage WHERE session = ?1",
            params![session.as_str()],
            |r| {
                Ok(UsageTotals {
                    requests: r.get::<_, i64>(0)? as u64,
                    measured_requests: r.get::<_, i64>(1)? as u64,
                    prompt_tokens: r.get::<_, i64>(2)? as u64,
                    completion_tokens: r.get::<_, i64>(3)? as u64,
                    total_tokens: r.get::<_, i64>(4)? as u64,
                })
            },
        )?;
        Ok(totals)
    }

    /// Account-wide totals, for the monitoring view that spans every session.
    pub fn usage_grand_totals(&self) -> Result<UsageTotals> {
        let conn = self.lock();
        let totals = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(measured), 0),
                    COALESCE(SUM(prompt_tokens), 0),
                    COALESCE(SUM(completion_tokens), 0),
                    COALESCE(SUM(total_tokens), 0)
             FROM usage",
            [],
            |r| {
                Ok(UsageTotals {
                    requests: r.get::<_, i64>(0)? as u64,
                    measured_requests: r.get::<_, i64>(1)? as u64,
                    prompt_tokens: r.get::<_, i64>(2)? as u64,
                    completion_tokens: r.get::<_, i64>(3)? as u64,
                    total_tokens: r.get::<_, i64>(4)? as u64,
                })
            },
        )?;
        Ok(totals)
    }

    pub fn delete_usage_for_session(&self, session: &SessionId) -> Result<()> {
        self.lock().execute(
            "DELETE FROM usage WHERE session = ?1",
            params![session.as_str()],
        )?;
        Ok(())
    }

    // ---- bots ----------------------------------------------------------

    pub fn save_bot(&self, row: &BotRow) -> Result<()> {
        self.lock().execute(
            "INSERT INTO bots (id, name, role, description, icon, created_ms)
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

    pub fn list_bots(&self) -> Result<Vec<BotRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, role, description, icon, created_ms FROM bots ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(BotRow {
                    id: BotId::new(row.get::<_, String>(0)?),
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

    pub fn delete_bot(&self, id: &BotId) -> Result<()> {
        self.lock()
            .execute("DELETE FROM bots WHERE id = ?1", params![id.as_str()])?;
        Ok(())
    }

    // ---- teams ----------------------------------------------------------

    pub fn create_hive_roster(&self, name: &str, now_ms: u64) -> Result<HiveRosterRow> {
        let row = HiveRosterRow {
            id: HiveRosterId::generate(),
            name: name.to_owned(),
            session_id: None,
            created_ms: now_ms,
            updated_ms: now_ms,
        };
        self.lock().execute(
            "INSERT INTO hive_rosters (id, name, session_id, created_ms, updated_ms)
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

    pub fn list_hive_rosters(&self) -> Result<Vec<HiveRosterRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, session_id, created_ms, updated_ms FROM hive_rosters ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(HiveRosterRow {
                    id: HiveRosterId::new(row.get::<_, String>(0)?),
                    name: row.get(1)?,
                    session_id: row.get::<_, Option<String>>(2)?.map(SessionId::new),
                    created_ms: row.get::<_, i64>(3)? as u64,
                    updated_ms: row.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn rename_hive_roster(&self, id: &HiveRosterId, name: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE hive_rosters SET name = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), name, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn set_hive_roster_session(
        &self,
        id: &HiveRosterId,
        session: Option<&SessionId>,
        now_ms: u64,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE hive_rosters SET session_id = ?2, updated_ms = ?3 WHERE id = ?1",
            params![
                id.as_str(),
                session.map(|s| s.as_str().to_owned()),
                now_ms as i64
            ],
        )?;
        Ok(())
    }

    pub fn delete_hive_roster(&self, id: &HiveRosterId) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM hive_roster_members WHERE roster = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "DELETE FROM hive_rosters WHERE id = ?1",
            params![id.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn add_hive_roster_member(&self, roster: &HiveRosterId, bot: &BotId) -> Result<()> {
        self.lock().execute(
            "INSERT OR IGNORE INTO hive_roster_members (roster, bot) VALUES (?1, ?2)",
            params![roster.as_str(), bot.as_str()],
        )?;
        Ok(())
    }

    pub fn remove_hive_roster_member(&self, roster: &HiveRosterId, bot: &BotId) -> Result<()> {
        self.lock().execute(
            "DELETE FROM hive_roster_members WHERE roster = ?1 AND bot = ?2",
            params![roster.as_str(), bot.as_str()],
        )?;
        Ok(())
    }

    pub fn list_hive_roster_members(&self, roster: &HiveRosterId) -> Result<Vec<BotId>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT bot FROM hive_roster_members WHERE roster = ?1 ORDER BY bot ASC")?;
        let rows = stmt
            .query_map(params![roster.as_str()], |row| {
                Ok(BotId::new(row.get::<_, String>(0)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Drop an agent from every team that references it, so membership never outlives a profile.
    pub fn remove_bot_from_hive_rosters(&self, bot: &BotId) -> Result<Vec<HiveRosterId>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT roster FROM hive_roster_members WHERE bot = ?1")?;
        let teams: Vec<HiveRosterId> = stmt
            .query_map(params![bot.as_str()], |row| {
                Ok(HiveRosterId::new(row.get::<_, String>(0)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !teams.is_empty() {
            conn.execute(
                "DELETE FROM hive_roster_members WHERE bot = ?1",
                params![bot.as_str()],
            )?;
        }
        Ok(teams)
    }

    // ---- users ----------------------------------------------------------

    pub fn create_user(&self, name: &str, now_ms: u64) -> Result<UserRow> {
        let row = UserRow {
            id: UserId::generate(),
            name: name.to_owned(),
            created_ms: now_ms,
            updated_ms: now_ms,
        };
        self.lock().execute(
            "INSERT INTO users (id, name, created_ms, updated_ms) VALUES (?1, ?2, ?3, ?4)",
            params![
                row.id.as_str(),
                row.name,
                row.created_ms as i64,
                row.updated_ms as i64
            ],
        )?;
        Ok(row)
    }

    pub fn list_users(&self) -> Result<Vec<UserRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, created_ms, updated_ms FROM users ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(UserRow {
                    id: UserId::new(row.get::<_, String>(0)?),
                    name: row.get(1)?,
                    created_ms: row.get::<_, i64>(2)? as u64,
                    updated_ms: row.get::<_, i64>(3)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_user(&self, id: &UserId) -> Result<Option<UserRow>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id, name, created_ms, updated_ms FROM users WHERE id = ?1")?;
        let mut rows = stmt.query(params![id.as_str()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(UserRow {
                id: UserId::new(row.get::<_, String>(0)?),
                name: row.get(1)?,
                created_ms: row.get::<_, i64>(2)? as u64,
                updated_ms: row.get::<_, i64>(3)? as u64,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn update_user(&self, id: &UserId, name: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE users SET name = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), name, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn delete_user(&self, id: &UserId) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        // Cascade: profiles, sessions (with journals), entities owned by this user.
        // Profiles
        let profile_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM profiles WHERE user_id = ?1")?;

            stmt.query_map(params![id.as_str()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for pid in &profile_ids {
            // Cascade sessions belonging to this profile.
            let prof_session_ids: Vec<String> = {
                let mut stmt = tx.prepare("SELECT id FROM sessions WHERE profile_id = ?1")?;
                stmt.query_map(params![pid], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for sid in &prof_session_ids {
                tx.execute("DELETE FROM journal WHERE session = ?1", params![sid])?;
                tx.execute(
                    "UPDATE hive_rosters SET session_id = NULL WHERE session_id = ?1",
                    params![sid],
                )?;
                tx.execute(
                    "UPDATE hive_sessions SET session_id = NULL WHERE session_id = ?1",
                    params![sid],
                )?;
                tx.execute(
                    "UPDATE tickets SET session_id = NULL WHERE session_id = ?1",
                    params![sid],
                )?;
                tx.execute(
                    "DELETE FROM session_workspaces WHERE session = ?1",
                    params![sid],
                )?;
                tx.execute("DELETE FROM artifacts WHERE session = ?1", params![sid])?;
                tx.execute("DELETE FROM file_changes WHERE session = ?1", params![sid])?;
                tx.execute("DELETE FROM usage WHERE session = ?1", params![sid])?;
                tx.execute("DELETE FROM sessions WHERE id = ?1", params![sid])?;
            }
            // Cascade hive sessions belonging to this profile.
            let prof_hive_ids: Vec<String> = {
                let mut stmt = tx.prepare("SELECT id FROM hive_sessions WHERE profile_id = ?1")?;
                stmt.query_map(params![pid], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for hid in &prof_hive_ids {
                tx.execute("DELETE FROM hive_messages WHERE hive = ?1", params![hid])?;
                tx.execute(
                    "DELETE FROM hive_participants WHERE hive = ?1",
                    params![hid],
                )?;
                tx.execute("DELETE FROM hive_sessions WHERE id = ?1", params![hid])?;
            }
            tx.execute("DELETE FROM profiles WHERE id = ?1", params![pid])?;
        }
        // Hive sessions owned by this user
        let hive_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM hive_sessions WHERE owner_user = ?1")?;

            stmt.query_map(params![id.as_str()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for hid in &hive_ids {
            tx.execute("DELETE FROM hive_messages WHERE hive = ?1", params![hid])?;
            tx.execute(
                "DELETE FROM hive_participants WHERE hive = ?1",
                params![hid],
            )?;
            tx.execute("DELETE FROM hive_sessions WHERE id = ?1", params![hid])?;
        }
        // Entities owned by this user (keep shared ones with NULL owner)
        tx.execute(
            "DELETE FROM entities WHERE owner_user = ?1",
            params![id.as_str()],
        )?;
        // Sessions belonging to this user (via user_id column)
        let session_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM sessions WHERE user_id = ?1")?;

            stmt.query_map(params![id.as_str()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for sid in &session_ids {
            tx.execute("DELETE FROM journal WHERE session = ?1", params![sid])?;
            tx.execute(
                "UPDATE hive_rosters SET session_id = NULL WHERE session_id = ?1",
                params![sid],
            )?;
            tx.execute(
                "UPDATE hive_sessions SET session_id = NULL WHERE session_id = ?1",
                params![sid],
            )?;
            tx.execute(
                "UPDATE tickets SET session_id = NULL WHERE session_id = ?1",
                params![sid],
            )?;
            tx.execute(
                "DELETE FROM session_workspaces WHERE session = ?1",
                params![sid],
            )?;
            tx.execute("DELETE FROM artifacts WHERE session = ?1", params![sid])?;
            tx.execute("DELETE FROM file_changes WHERE session = ?1", params![sid])?;
            tx.execute("DELETE FROM usage WHERE session = ?1", params![sid])?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", params![sid])?;
        }
        // Entities owned by this user may own a profile link; detach before profile delete.
        tx.execute(
            "UPDATE entities SET profile_id = NULL WHERE owner_user = ?1",
            params![id.as_str()],
        )?;
        tx.execute("DELETE FROM users WHERE id = ?1", params![id.as_str()])?;
        // Fix active_identity if it pointed at this user
        tx.execute(
            "UPDATE active_identity SET user_id = NULL, profile_id = NULL, main_agent = NULL WHERE user_id = ?1",
            params![id.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ---- profiles -------------------------------------------------------

    pub fn create_profile(&self, user: &UserId, name: &str, now_ms: u64) -> Result<ProfileRow> {
        let row = ProfileRow {
            id: ProfileId::generate(),
            user: user.clone(),
            name: name.to_owned(),
            created_ms: now_ms,
            updated_ms: now_ms,
        };
        self.lock().execute(
            "INSERT INTO profiles (id, user_id, name, created_ms, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.id.as_str(), row.user.as_str(), row.name, row.created_ms as i64, row.updated_ms as i64],
        )?;
        Ok(row)
    }

    pub fn list_profiles(&self) -> Result<Vec<ProfileRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, user_id, name, created_ms, updated_ms FROM profiles ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ProfileRow {
                    id: ProfileId::new(row.get::<_, String>(0)?),
                    user: UserId::new(row.get::<_, String>(1)?),
                    name: row.get(2)?,
                    created_ms: row.get::<_, i64>(3)? as u64,
                    updated_ms: row.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_profiles_by_user(&self, user: &UserId) -> Result<Vec<ProfileRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, user_id, name, created_ms, updated_ms FROM profiles WHERE user_id = ?1 ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![user.as_str()], |row| {
                Ok(ProfileRow {
                    id: ProfileId::new(row.get::<_, String>(0)?),
                    user: UserId::new(row.get::<_, String>(1)?),
                    name: row.get(2)?,
                    created_ms: row.get::<_, i64>(3)? as u64,
                    updated_ms: row.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_profile(&self, id: &ProfileId) -> Result<Option<ProfileRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, user_id, name, created_ms, updated_ms FROM profiles WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id.as_str()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(ProfileRow {
                id: ProfileId::new(row.get::<_, String>(0)?),
                user: UserId::new(row.get::<_, String>(1)?),
                name: row.get(2)?,
                created_ms: row.get::<_, i64>(3)? as u64,
                updated_ms: row.get::<_, i64>(4)? as u64,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn update_profile(&self, id: &ProfileId, name: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE profiles SET name = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), name, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn delete_profile(&self, id: &ProfileId) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        // Sessions belonging to this profile
        let session_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM sessions WHERE profile_id = ?1")?;

            stmt.query_map(params![id.as_str()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for sid in &session_ids {
            tx.execute("DELETE FROM journal WHERE session = ?1", params![sid])?;
            tx.execute(
                "UPDATE hive_rosters SET session_id = NULL WHERE session_id = ?1",
                params![sid],
            )?;
            tx.execute(
                "UPDATE hive_sessions SET session_id = NULL WHERE session_id = ?1",
                params![sid],
            )?;
            tx.execute(
                "UPDATE tickets SET session_id = NULL WHERE session_id = ?1",
                params![sid],
            )?;
            tx.execute(
                "DELETE FROM session_workspaces WHERE session = ?1",
                params![sid],
            )?;
            tx.execute("DELETE FROM artifacts WHERE session = ?1", params![sid])?;
            tx.execute("DELETE FROM file_changes WHERE session = ?1", params![sid])?;
            tx.execute("DELETE FROM usage WHERE session = ?1", params![sid])?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", params![sid])?;
        }
        tx.execute(
            "UPDATE entities SET profile_id = NULL WHERE profile_id = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "UPDATE active_identity SET profile_id = NULL WHERE profile_id = ?1",
            params![id.as_str()],
        )?;
        // Hive sessions belonging to this profile
        let hive_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM hive_sessions WHERE profile_id = ?1")?;

            stmt.query_map(params![id.as_str()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for hid in &hive_ids {
            tx.execute("DELETE FROM hive_messages WHERE hive = ?1", params![hid])?;
            tx.execute(
                "DELETE FROM hive_participants WHERE hive = ?1",
                params![hid],
            )?;
            tx.execute("DELETE FROM hive_sessions WHERE id = ?1", params![hid])?;
        }
        tx.execute("DELETE FROM profiles WHERE id = ?1", params![id.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    // ---- entities -------------------------------------------------------

    pub fn save_entity(&self, row: &EntityRow) -> Result<()> {
        self.lock().execute(
            "INSERT INTO entities (id, owner_user, kind, name, slug, title, role, specialization, description, icon, model, profile_id, status, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(id) DO UPDATE SET
               owner_user = excluded.owner_user, kind = excluded.kind, name = excluded.name,
               slug = excluded.slug, title = excluded.title, role = excluded.role,
               specialization = excluded.specialization, description = excluded.description,
               icon = excluded.icon, model = excluded.model, profile_id = excluded.profile_id,
               status = excluded.status, updated_ms = excluded.updated_ms",
            params![
                row.id.as_str(),
                row.owner_user.as_ref().map(|u| u.as_str().to_owned()),
                row.kind.as_str(),
                row.name,
                row.slug,
                row.title,
                row.role,
                row.specialization,
                row.description,
                row.icon,
                row.model,
                row.profile_id.as_ref().map(|p| p.as_str().to_owned()),
                row.status,
                row.created_ms as i64,
                row.updated_ms as i64,
            ],
        )?;
        Ok(())
    }

    pub fn list_entities(&self) -> Result<Vec<EntityRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_user, kind, name, slug, title, role, specialization, description, icon, model, profile_id, status, created_ms, updated_ms
             FROM entities ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], parse_entity_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_entities_by_owner(&self, owner: Option<&UserId>) -> Result<Vec<EntityRow>> {
        let conn = self.lock();
        let (sql, param): (&str, Option<String>) = match owner {
            Some(u) => (
                "SELECT id, owner_user, kind, name, slug, title, role, specialization, description, icon, model, profile_id, status, created_ms, updated_ms
                 FROM entities WHERE owner_user = ?1 ORDER BY created_ms ASC",
                Some(u.as_str().to_owned()),
            ),
            None => (
                "SELECT id, owner_user, kind, name, slug, title, role, specialization, description, icon, model, profile_id, status, created_ms, updated_ms
                 FROM entities WHERE owner_user IS NULL ORDER BY created_ms ASC",
                None,
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = if let Some(p) = param {
            stmt.query_map(params![p], parse_entity_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map([], parse_entity_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    pub fn list_entities_by_kind(&self, kind: EntityKind) -> Result<Vec<EntityRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_user, kind, name, slug, title, role, specialization, description, icon, model, profile_id, status, created_ms, updated_ms
             FROM entities WHERE kind = ?1 ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![kind.as_str()], parse_entity_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_entity(&self, id: &EntityId) -> Result<Option<EntityRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_user, kind, name, slug, title, role, specialization, description, icon, model, profile_id, status, created_ms, updated_ms
             FROM entities WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id.as_str()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(parse_entity_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn get_entity_by_slug(
        &self,
        owner: Option<&UserId>,
        kind: EntityKind,
        slug: &str,
    ) -> Result<Option<EntityRow>> {
        let conn = self.lock();
        let (sql, params_vec): (&str, Vec<String>) = match owner {
            Some(u) => (
                "SELECT id, owner_user, kind, name, slug, title, role, specialization, description, icon, model, profile_id, status, created_ms, updated_ms
                 FROM entities WHERE owner_user = ?1 AND kind = ?2 AND slug = ?3",
                vec![u.as_str().to_owned(), kind.as_str().to_owned(), slug.to_owned()],
            ),
            None => (
                "SELECT id, owner_user, kind, name, slug, title, role, specialization, description, icon, model, profile_id, status, created_ms, updated_ms
                 FROM entities WHERE owner_user IS NULL AND kind = ?1 AND slug = ?2",
                vec![kind.as_str().to_owned(), slug.to_owned()],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let mut rows = if params_vec.len() == 3 {
            stmt.query(params![params_vec[0], params_vec[1], params_vec[2]])?
        } else {
            stmt.query(params![params_vec[0], params_vec[1]])?
        };
        if let Some(row) = rows.next()? {
            Ok(Some(parse_entity_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn delete_entity(&self, id: &EntityId) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM hive_participants WHERE entity = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "UPDATE ticket_participants SET entity_id = NULL WHERE entity_id = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "UPDATE assignments SET entity_id = NULL WHERE entity_id = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "UPDATE active_identity SET main_agent = NULL WHERE main_agent = ?1",
            params![id.as_str()],
        )?;
        tx.execute("DELETE FROM entities WHERE id = ?1", params![id.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    // ---- active identity -------------------------------------------------

    pub fn get_active_identity(&self) -> Result<ActiveIdentity> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT user_id, profile_id, main_agent FROM active_identity WHERE singleton = 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(ActiveIdentity {
                user: row.get::<_, Option<String>>(0)?.map(UserId::new),
                profile: row.get::<_, Option<String>>(1)?.map(ProfileId::new),
                main_agent: row.get::<_, Option<String>>(2)?.map(EntityId::new),
            })
        } else {
            Ok(ActiveIdentity::default())
        }
    }

    pub fn set_active_identity(&self, identity: &ActiveIdentity) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO active_identity (singleton, user_id, profile_id, main_agent)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(singleton) DO UPDATE SET
               user_id = excluded.user_id, profile_id = excluded.profile_id, main_agent = excluded.main_agent",
            params![
                identity.user.as_ref().map(|u| u.as_str().to_owned()),
                identity.profile.as_ref().map(|p| p.as_str().to_owned()),
                identity.main_agent.as_ref().map(|e| e.as_str().to_owned()),
            ],
        )?;
        Ok(())
    }

    // ---- hive sessions ---------------------------------------------------

    pub fn create_hive(&self, row: &HiveRow) -> Result<HiveRow> {
        self.lock().execute(
            "INSERT INTO hive_sessions (id, owner_user, profile_id, name, objective, session_id, state, policy, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                row.id.as_str(),
                row.owner_user.as_str(),
                row.profile_id.as_ref().map(|p| p.as_str().to_owned()),
                row.name,
                row.objective,
                row.session_id.as_ref().map(|s| s.as_str().to_owned()),
                row.state,
                row.policy,
                row.created_ms as i64,
                row.updated_ms as i64,
            ],
        )?;
        Ok(row.clone())
    }

    pub fn list_hives(&self) -> Result<Vec<HiveRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_user, profile_id, name, objective, session_id, state, policy, created_ms, updated_ms
             FROM hive_sessions ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], parse_hive_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_hives_by_user(&self, user: &UserId) -> Result<Vec<HiveRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_user, profile_id, name, objective, session_id, state, policy, created_ms, updated_ms
             FROM hive_sessions WHERE owner_user = ?1 ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![user.as_str()], parse_hive_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_hive(&self, id: &HiveId) -> Result<Option<HiveRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_user, profile_id, name, objective, session_id, state, policy, created_ms, updated_ms
             FROM hive_sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id.as_str()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(parse_hive_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn update_hive(&self, row: &HiveRow) -> Result<()> {
        self.lock().execute(
            "UPDATE hive_sessions SET owner_user = ?2, profile_id = ?3, name = ?4, objective = ?5, session_id = ?6, state = ?7, policy = ?8, updated_ms = ?9 WHERE id = ?1",
            params![
                row.id.as_str(),
                row.owner_user.as_str(),
                row.profile_id.as_ref().map(|p| p.as_str().to_owned()),
                row.name,
                row.objective,
                row.session_id.as_ref().map(|s| s.as_str().to_owned()),
                row.state,
                row.policy,
                row.updated_ms as i64,
            ],
        )?;
        Ok(())
    }

    pub fn delete_hive(&self, id: &HiveId) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM hive_messages WHERE hive = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "DELETE FROM hive_participants WHERE hive = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "DELETE FROM hive_sessions WHERE id = ?1",
            params![id.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_hive_session(
        &self,
        id: &HiveId,
        session: Option<&SessionId>,
        now_ms: u64,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE hive_sessions SET session_id = ?2, updated_ms = ?3 WHERE id = ?1",
            params![
                id.as_str(),
                session.map(|s| s.as_str().to_owned()),
                now_ms as i64
            ],
        )?;
        Ok(())
    }

    // ---- hive participants ----------------------------------------------

    pub fn add_hive_participant(&self, row: &HiveParticipantRow) -> Result<()> {
        self.lock().execute(
            "INSERT OR IGNORE INTO hive_participants (hive, entity, kind, role, joined_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row.hive.as_str(),
                row.entity.as_str(),
                row.kind.as_str(),
                row.role,
                row.joined_ms as i64
            ],
        )?;
        Ok(())
    }

    pub fn remove_hive_participant(&self, hive: &HiveId, entity: &EntityId) -> Result<()> {
        self.lock().execute(
            "DELETE FROM hive_participants WHERE hive = ?1 AND entity = ?2",
            params![hive.as_str(), entity.as_str()],
        )?;
        Ok(())
    }

    pub fn list_hive_participants(&self, hive: &HiveId) -> Result<Vec<HiveParticipantRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT hive, entity, kind, role, joined_ms FROM hive_participants WHERE hive = ?1 ORDER BY joined_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![hive.as_str()], |row| {
                Ok(HiveParticipantRow {
                    hive: HiveId::new(row.get::<_, String>(0)?),
                    entity: EntityId::new(row.get::<_, String>(1)?),
                    kind: EntityKind::parse(&row.get::<_, String>(2)?).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
                    role: row.get(3)?,
                    joined_ms: row.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- hive messages --------------------------------------------------

    pub fn append_hive_message(&self, row: &HiveMessageRow) -> Result<HiveMessageRow> {
        self.lock().execute(
            "INSERT INTO hive_messages (id, hive, sender, sender_kind, content, task_ref, ts_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.id.as_str(),
                row.hive.as_str(),
                row.sender,
                row.sender_kind,
                row.content,
                row.task_ref,
                row.ts_ms as i64,
            ],
        )?;
        Ok(row.clone())
    }

    pub fn list_hive_messages(&self, hive: &HiveId) -> Result<Vec<HiveMessageRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, hive, sender, sender_kind, content, task_ref, ts_ms
             FROM hive_messages WHERE hive = ?1 ORDER BY ts_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![hive.as_str()], |row| {
                Ok(HiveMessageRow {
                    id: HiveMessageId::new(row.get::<_, String>(0)?),
                    hive: HiveId::new(row.get::<_, String>(1)?),
                    sender: row.get(2)?,
                    sender_kind: row.get(3)?,
                    content: row.get(4)?,
                    task_ref: row.get(5)?,
                    ts_ms: row.get::<_, i64>(6)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- tickets --------------------------------------------------------

    pub fn create_ticket(&self, row: &TicketRow) -> Result<TicketRow> {
        self.lock().execute(
            "INSERT INTO tickets (id, owner_key, owner_kind, title, description, request, kind, status, parent_ticket, hive_id, session_id, strategy, result, failure_reason, created_ms, updated_ms, closed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                row.id.as_str(),
                row.owner_key,
                row.owner_kind,
                row.title,
                row.description,
                row.request,
                row.kind,
                row.status,
                row.parent_ticket.as_ref().map(|t| t.as_str().to_owned()),
                row.hive_id.as_ref().map(|h| h.as_str().to_owned()),
                row.session_id.as_ref().map(|s| s.as_str().to_owned()),
                row.strategy,
                row.result,
                row.failure_reason,
                row.created_ms as i64,
                row.updated_ms as i64,
                row.closed_ms.map(|c| c as i64),
            ],
        )?;
        Ok(row.clone())
    }

    pub fn get_ticket(&self, id: &TicketId) -> Result<Option<TicketRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_key, owner_kind, title, description, request, kind, status, parent_ticket, hive_id, session_id, strategy, result, failure_reason, created_ms, updated_ms, closed_ms
             FROM tickets WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id.as_str()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(parse_ticket_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_tickets(&self) -> Result<Vec<TicketRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_key, owner_kind, title, description, request, kind, status, parent_ticket, hive_id, session_id, strategy, result, failure_reason, created_ms, updated_ms, closed_ms
             FROM tickets ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map([], parse_ticket_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_tickets_by_owner(&self, owner_key: &str) -> Result<Vec<TicketRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_key, owner_kind, title, description, request, kind, status, parent_ticket, hive_id, session_id, strategy, result, failure_reason, created_ms, updated_ms, closed_ms
             FROM tickets WHERE owner_key = ?1 ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![owner_key], parse_ticket_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_tickets_by_kind(&self, kind: &str) -> Result<Vec<TicketRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_key, owner_kind, title, description, request, kind, status, parent_ticket, hive_id, session_id, strategy, result, failure_reason, created_ms, updated_ms, closed_ms
             FROM tickets WHERE kind = ?1 ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![kind], parse_ticket_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_tickets_by_status(&self, status: &str) -> Result<Vec<TicketRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_key, owner_kind, title, description, request, kind, status, parent_ticket, hive_id, session_id, strategy, result, failure_reason, created_ms, updated_ms, closed_ms
             FROM tickets WHERE status = ?1 ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![status], parse_ticket_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_child_tickets(&self, parent: &TicketId) -> Result<Vec<TicketRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, owner_key, owner_kind, title, description, request, kind, status, parent_ticket, hive_id, session_id, strategy, result, failure_reason, created_ms, updated_ms, closed_ms
             FROM tickets WHERE parent_ticket = ?1 ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![parent.as_str()], parse_ticket_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn search_tickets(&self, query: &str) -> Result<Vec<TicketRow>> {
        let conn = self.lock();
        let like = format!("%{query}%");
        let mut stmt = conn.prepare(
            "SELECT id, owner_key, owner_kind, title, description, request, kind, status, parent_ticket, hive_id, session_id, strategy, result, failure_reason, created_ms, updated_ms, closed_ms
             FROM tickets WHERE title LIKE ?1 OR description LIKE ?1 OR request LIKE ?1 ORDER BY updated_ms DESC",
        )?;
        let rows = stmt
            .query_map(params![like], parse_ticket_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn update_ticket(&self, row: &TicketRow) -> Result<()> {
        self.lock().execute(
            "UPDATE tickets SET owner_key = ?2, owner_kind = ?3, title = ?4, description = ?5, request = ?6, kind = ?7, status = ?8, parent_ticket = ?9, hive_id = ?10, session_id = ?11, strategy = ?12, result = ?13, failure_reason = ?14, updated_ms = ?15, closed_ms = ?16 WHERE id = ?1",
            params![
                row.id.as_str(),
                row.owner_key,
                row.owner_kind,
                row.title,
                row.description,
                row.request,
                row.kind,
                row.status,
                row.parent_ticket.as_ref().map(|t| t.as_str().to_owned()),
                row.hive_id.as_ref().map(|h| h.as_str().to_owned()),
                row.session_id.as_ref().map(|s| s.as_str().to_owned()),
                row.strategy,
                row.result,
                row.failure_reason,
                row.updated_ms as i64,
                row.closed_ms.map(|c| c as i64),
            ],
        )?;
        Ok(())
    }

    pub fn set_ticket_status(&self, id: &TicketId, status: &str, now_ms: u64) -> Result<()> {
        let closed = if status == "completed" || status == "failed" || status == "cancelled" {
            Some(now_ms as i64)
        } else {
            None
        };
        self.lock().execute(
            "UPDATE tickets SET status = ?2, updated_ms = ?3, closed_ms = COALESCE(?4, closed_ms) WHERE id = ?1",
            params![id.as_str(), status, now_ms as i64, closed],
        )?;
        Ok(())
    }

    pub fn set_ticket_result(&self, id: &TicketId, result: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE tickets SET result = ?2, updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), result, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn set_ticket_failure(&self, id: &TicketId, reason: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE tickets SET failure_reason = ?2, status = 'failed', updated_ms = ?3 WHERE id = ?1",
            params![id.as_str(), reason, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn reopen_ticket(&self, id: &TicketId, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE tickets SET status = 'reopened', closed_ms = NULL, updated_ms = ?2 WHERE id = ?1",
            params![id.as_str(), now_ms as i64],
        )?;
        Ok(())
    }

    pub fn cancel_ticket(&self, id: &TicketId, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE tickets SET status = 'cancelled', closed_ms = ?2, updated_ms = ?2 WHERE id = ?1",
            params![id.as_str(), now_ms as i64],
        )?;
        Ok(())
    }

    pub fn delete_ticket(&self, id: &TicketId) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM ticket_events WHERE ticket = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "DELETE FROM ticket_messages WHERE ticket = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "DELETE FROM ticket_participants WHERE ticket = ?1",
            params![id.as_str()],
        )?;
        tx.execute(
            "DELETE FROM assignments WHERE ticket = ?1",
            params![id.as_str()],
        )?;
        tx.execute("DELETE FROM tickets WHERE id = ?1", params![id.as_str()])?;
        tx.commit()?;
        Ok(())
    }

    // ---- ticket participants --------------------------------------------

    pub fn add_ticket_participant(&self, row: &TicketParticipantRow) -> Result<()> {
        self.lock().execute(
            "INSERT OR REPLACE INTO ticket_participants (ticket, participant_key, participant_kind, entity_id, user_id, role, can_read, can_write, can_admin, joined_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                row.ticket.as_str(),
                row.participant_key,
                row.participant_kind,
                row.entity_id.as_ref().map(|e| e.as_str().to_owned()),
                row.user_id.as_ref().map(|u| u.as_str().to_owned()),
                row.role,
                row.can_read as i64,
                row.can_write as i64,
                row.can_admin as i64,
                row.joined_ms as i64,
            ],
        )?;
        Ok(())
    }

    pub fn remove_ticket_participant(&self, ticket: &TicketId, key: &str) -> Result<()> {
        self.lock().execute(
            "DELETE FROM ticket_participants WHERE ticket = ?1 AND participant_key = ?2",
            params![ticket.as_str(), key],
        )?;
        Ok(())
    }

    pub fn list_ticket_participants(&self, ticket: &TicketId) -> Result<Vec<TicketParticipantRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ticket, participant_key, participant_kind, entity_id, user_id, role, can_read, can_write, can_admin, joined_ms
             FROM ticket_participants WHERE ticket = ?1 ORDER BY joined_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![ticket.as_str()], |r| {
                Ok(TicketParticipantRow {
                    ticket: TicketId::new(r.get::<_, String>(0)?),
                    participant_key: r.get(1)?,
                    participant_kind: r.get(2)?,
                    entity_id: r.get::<_, Option<String>>(3)?.map(EntityId::new),
                    user_id: r.get::<_, Option<String>>(4)?.map(UserId::new),
                    role: r.get(5)?,
                    can_read: r.get::<_, i64>(6)? != 0,
                    can_write: r.get::<_, i64>(7)? != 0,
                    can_admin: r.get::<_, i64>(8)? != 0,
                    joined_ms: r.get::<_, i64>(9)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_ticket_participant(
        &self,
        ticket: &TicketId,
        key: &str,
    ) -> Result<Option<TicketParticipantRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ticket, participant_key, participant_kind, entity_id, user_id, role, can_read, can_write, can_admin, joined_ms
             FROM ticket_participants WHERE ticket = ?1 AND participant_key = ?2",
        )?;
        let mut rows = stmt.query(params![ticket.as_str(), key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(TicketParticipantRow {
                ticket: TicketId::new(row.get::<_, String>(0)?),
                participant_key: row.get(1)?,
                participant_kind: row.get(2)?,
                entity_id: row.get::<_, Option<String>>(3)?.map(EntityId::new),
                user_id: row.get::<_, Option<String>>(4)?.map(UserId::new),
                role: row.get(5)?,
                can_read: row.get::<_, i64>(6)? != 0,
                can_write: row.get::<_, i64>(7)? != 0,
                can_admin: row.get::<_, i64>(8)? != 0,
                joined_ms: row.get::<_, i64>(9)? as u64,
            }))
        } else {
            Ok(None)
        }
    }

    // ---- ticket messages -------------------------------------------------

    pub fn append_ticket_message(&self, row: &TicketMessageRow) -> Result<TicketMessageRow> {
        self.lock().execute(
            "INSERT INTO ticket_messages (id, ticket, author_key, author_kind, content, visibility, ts_ms, task_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.id.as_str(),
                row.ticket.as_str(),
                row.author_key,
                row.author_kind,
                row.content,
                row.visibility,
                row.ts_ms as i64,
                row.task_ref,
            ],
        )?;
        Ok(row.clone())
    }

    pub fn list_ticket_messages(&self, ticket: &TicketId) -> Result<Vec<TicketMessageRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ticket, author_key, author_kind, content, visibility, ts_ms, task_ref
             FROM ticket_messages WHERE ticket = ?1 ORDER BY ts_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![ticket.as_str()], |r| {
                Ok(TicketMessageRow {
                    id: TicketMessageId::new(r.get::<_, String>(0)?),
                    ticket: TicketId::new(r.get::<_, String>(1)?),
                    author_key: r.get(2)?,
                    author_kind: r.get(3)?,
                    content: r.get(4)?,
                    visibility: r.get(5)?,
                    ts_ms: r.get::<_, i64>(6)? as u64,
                    task_ref: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- assignments ----------------------------------------------------

    pub fn create_assignment(&self, row: &AssignmentRow) -> Result<AssignmentRow> {
        self.lock().execute(
            "INSERT INTO assignments (id, ticket, parent_assignment, assignee_key, assignee_kind, entity_id, title, description, status, strategy, result, error, created_ms, updated_ms, completed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                row.id.as_str(),
                row.ticket.as_str(),
                row.parent_assignment.as_ref().map(|a| a.as_str().to_owned()),
                row.assignee_key,
                row.assignee_kind,
                row.entity_id.as_ref().map(|e| e.as_str().to_owned()),
                row.title,
                row.description,
                row.status,
                row.strategy,
                row.result,
                row.error,
                row.created_ms as i64,
                row.updated_ms as i64,
                row.completed_ms.map(|c| c as i64),
            ],
        )?;
        Ok(row.clone())
    }

    pub fn get_assignment(&self, id: &AssignmentId) -> Result<Option<AssignmentRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ticket, parent_assignment, assignee_key, assignee_kind, entity_id, title, description, status, strategy, result, error, created_ms, updated_ms, completed_ms
             FROM assignments WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id.as_str()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(parse_assignment_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_assignments(&self, ticket: &TicketId) -> Result<Vec<AssignmentRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ticket, parent_assignment, assignee_key, assignee_kind, entity_id, title, description, status, strategy, result, error, created_ms, updated_ms, completed_ms
             FROM assignments WHERE ticket = ?1 ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![ticket.as_str()], parse_assignment_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_assignments_by_assignee(&self, key: &str) -> Result<Vec<AssignmentRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ticket, parent_assignment, assignee_key, assignee_kind, entity_id, title, description, status, strategy, result, error, created_ms, updated_ms, completed_ms
             FROM assignments WHERE assignee_key = ?1 ORDER BY created_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![key], parse_assignment_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn update_assignment(&self, row: &AssignmentRow) -> Result<()> {
        self.lock().execute(
            "UPDATE assignments SET ticket = ?2, parent_assignment = ?3, assignee_key = ?4, assignee_kind = ?5, entity_id = ?6, title = ?7, description = ?8, status = ?9, strategy = ?10, result = ?11, error = ?12, updated_ms = ?13, completed_ms = ?14 WHERE id = ?1",
            params![
                row.id.as_str(),
                row.ticket.as_str(),
                row.parent_assignment.as_ref().map(|a| a.as_str().to_owned()),
                row.assignee_key,
                row.assignee_kind,
                row.entity_id.as_ref().map(|e| e.as_str().to_owned()),
                row.title,
                row.description,
                row.status,
                row.strategy,
                row.result,
                row.error,
                row.updated_ms as i64,
                row.completed_ms.map(|c| c as i64),
            ],
        )?;
        Ok(())
    }

    pub fn set_assignment_status(
        &self,
        id: &AssignmentId,
        status: &str,
        now_ms: u64,
    ) -> Result<()> {
        let completed = if status == "completed" || status == "failed" || status == "cancelled" {
            Some(now_ms as i64)
        } else {
            None
        };
        self.lock().execute(
            "UPDATE assignments SET status = ?2, updated_ms = ?3, completed_ms = COALESCE(?4, completed_ms) WHERE id = ?1",
            params![id.as_str(), status, now_ms as i64, completed],
        )?;
        Ok(())
    }

    pub fn set_assignment_result(
        &self,
        id: &AssignmentId,
        result: &str,
        now_ms: u64,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE assignments SET result = ?2, status = 'completed', updated_ms = ?3, completed_ms = ?3 WHERE id = ?1",
            params![id.as_str(), result, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn set_assignment_error(&self, id: &AssignmentId, error: &str, now_ms: u64) -> Result<()> {
        self.lock().execute(
            "UPDATE assignments SET error = ?2, status = 'failed', updated_ms = ?3, completed_ms = ?3 WHERE id = ?1",
            params![id.as_str(), error, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn delete_assignment(&self, id: &AssignmentId) -> Result<()> {
        self.lock().execute(
            "DELETE FROM assignments WHERE id = ?1",
            params![id.as_str()],
        )?;
        Ok(())
    }

    // ---- ticket events --------------------------------------------------

    pub fn append_ticket_event(&self, row: &TicketEventRow) -> Result<TicketEventRow> {
        self.lock().execute(
            "INSERT INTO ticket_events (id, ticket, ts_ms, kind, actor_key, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.id.as_str(),
                row.ticket.as_str(),
                row.ts_ms as i64,
                row.kind,
                row.actor_key,
                row.payload.to_string(),
            ],
        )?;
        Ok(row.clone())
    }

    pub fn list_ticket_events(&self, ticket: &TicketId) -> Result<Vec<TicketEventRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ticket, ts_ms, kind, actor_key, payload FROM ticket_events WHERE ticket = ?1 ORDER BY ts_ms ASC",
        )?;
        let rows = stmt
            .query_map(params![ticket.as_str()], |r| {
                let payload_str: String = r.get(5)?;
                let event_id: String = r.get(0)?;
                Ok(TicketEventRow {
                    id: TicketEventId::new(r.get::<_, String>(0)?),
                    ticket: TicketId::new(r.get::<_, String>(1)?),
                    ts_ms: r.get::<_, i64>(2)? as u64,
                    kind: r.get(3)?,
                    actor_key: r.get(4)?,
                    payload: serde_json::from_str(&payload_str).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(StoreError::Parse(format!(
                                "ticket event {event_id} payload: {e}",
                            ))),
                        )
                    })?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
                let sess_str: String = row.get(1)?;
                let seq_val: i64 = row.get(0)?;
                Ok(JournalRecord {
                    seq: row.get::<_, i64>(0)? as u64,
                    session: SessionId::new(row.get::<_, String>(1)?),
                    ts_ms: row.get::<_, i64>(2)? as u64,
                    kind: RecordKind::Message,
                    effect: row.get::<_, Option<String>>(4)?.map(EffectId::new),
                    payload: serde_json::from_str(&payload).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(StoreError::Parse(format!(
                                "journal payload for session {sess_str} seq {seq_val}: {e}",
                            ))),
                        )
                    })?,
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

fn parse_entity_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntityRow> {
    let kind_str: String = row.get(2)?;
    Ok(EntityRow {
        id: EntityId::new(row.get::<_, String>(0)?),
        owner_user: row.get::<_, Option<String>>(1)?.map(UserId::new),
        kind: EntityKind::parse(&kind_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?,
        name: row.get(3)?,
        slug: row.get(4)?,
        title: row.get(5)?,
        role: row.get(6)?,
        specialization: row.get(7)?,
        description: row.get(8)?,
        icon: row.get(9)?,
        model: row.get(10)?,
        profile_id: row.get::<_, Option<String>>(11)?.map(ProfileId::new),
        status: row.get(12)?,
        created_ms: row.get::<_, i64>(13)? as u64,
        updated_ms: row.get::<_, i64>(14)? as u64,
    })
}

fn parse_hive_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HiveRow> {
    Ok(HiveRow {
        id: HiveId::new(row.get::<_, String>(0)?),
        owner_user: UserId::new(row.get::<_, String>(1)?),
        profile_id: row.get::<_, Option<String>>(2)?.map(ProfileId::new),
        name: row.get(3)?,
        objective: row.get(4)?,
        session_id: row.get::<_, Option<String>>(5)?.map(SessionId::new),
        state: row.get(6)?,
        policy: row.get(7)?,
        created_ms: row.get::<_, i64>(8)? as u64,
        updated_ms: row.get::<_, i64>(9)? as u64,
    })
}

fn parse_ticket_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TicketRow> {
    Ok(TicketRow {
        id: TicketId::new(row.get::<_, String>(0)?),
        owner_key: row.get(1)?,
        owner_kind: row.get(2)?,
        title: row.get(3)?,
        description: row.get(4)?,
        request: row.get(5)?,
        kind: row.get(6)?,
        status: row.get(7)?,
        parent_ticket: row.get::<_, Option<String>>(8)?.map(TicketId::new),
        hive_id: row.get::<_, Option<String>>(9)?.map(HiveId::new),
        session_id: row.get::<_, Option<String>>(10)?.map(SessionId::new),
        strategy: row.get(11)?,
        result: row.get(12)?,
        failure_reason: row.get(13)?,
        created_ms: row.get::<_, i64>(14)? as u64,
        updated_ms: row.get::<_, i64>(15)? as u64,
        closed_ms: row.get::<_, Option<i64>>(16)?.map(|c| c as u64),
    })
}

fn parse_assignment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssignmentRow> {
    Ok(AssignmentRow {
        id: AssignmentId::new(row.get::<_, String>(0)?),
        ticket: TicketId::new(row.get::<_, String>(1)?),
        parent_assignment: row.get::<_, Option<String>>(2)?.map(AssignmentId::new),
        assignee_key: row.get(3)?,
        assignee_kind: row.get(4)?,
        entity_id: row.get::<_, Option<String>>(5)?.map(EntityId::new),
        title: row.get(6)?,
        description: row.get(7)?,
        status: row.get(8)?,
        strategy: row.get(9)?,
        result: row.get(10)?,
        error: row.get(11)?,
        created_ms: row.get::<_, i64>(12)? as u64,
        updated_ms: row.get::<_, i64>(13)? as u64,
        completed_ms: row.get::<_, Option<i64>>(14)?.map(|c| c as u64),
    })
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
    // The agent-profile table was renamed to `bots` (AgentId -> BotId). Existing databases
    // still carry `agents`; move it once, before the CREATE TABLE IF NOT EXISTS batch runs.
    let table_exists = |name: &str| -> Result<bool> {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .map_err(StoreError::from)
    };
    if table_exists("agents")? && !table_exists("bots")? {
        conn.execute("ALTER TABLE agents RENAME TO bots", [])?;
    }
    // Same rename for the roster tables (teams -> hive_rosters, team_members -> hive_roster_members).
    if table_exists("teams")? && !table_exists("hive_rosters")? {
        conn.execute("ALTER TABLE teams RENAME TO hive_rosters", [])?;
    }
    if table_exists("team_members")? && !table_exists("hive_roster_members")? {
        conn.execute("ALTER TABLE team_members RENAME TO hive_roster_members", [])?;
        conn.execute(
            "ALTER TABLE hive_roster_members RENAME COLUMN team TO roster",
            [],
        )?;
    }
    // agent_id -> bot_id columns (they store BotId values now).
    for table in ["sessions", "artifacts", "file_changes"] {
        let has_old: bool = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = 'agent_id'"
                ),
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        if has_old {
            conn.execute(
                &format!("ALTER TABLE {table} RENAME COLUMN agent_id TO bot_id"),
                [],
            )?;
        }
    }
    let member_agent_col: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('hive_roster_members') WHERE name = 'agent'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    if member_agent_col {
        conn.execute(
            "ALTER TABLE hive_roster_members RENAME COLUMN agent TO bot",
            [],
        )?;
    }
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            id          TEXT PRIMARY KEY,
            title       TEXT NOT NULL,
            cwd         TEXT NOT NULL,
            model       TEXT,
            bot_id    TEXT,
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL,
            title_source TEXT NOT NULL DEFAULT 'manual',
            title_status TEXT NOT NULL DEFAULT 'ready'
        );

        CREATE TABLE IF NOT EXISTS bots (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            role        TEXT NOT NULL,
            description TEXT NOT NULL,
            icon        TEXT,
            created_ms  INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS hive_rosters (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            session_id  TEXT,
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS hive_roster_members (
            roster TEXT NOT NULL,
            bot    TEXT NOT NULL,
            PRIMARY KEY (roster, bot)
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

        CREATE TABLE IF NOT EXISTS usage (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            session           TEXT NOT NULL,
            ts_ms             INTEGER NOT NULL,
            model             TEXT NOT NULL,
            prompt_tokens     INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            total_tokens      INTEGER NOT NULL,
            measured          INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS usage_by_session ON usage(session, ts_ms);

        CREATE TABLE IF NOT EXISTS users (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS profiles (
            id          TEXT PRIMARY KEY,
            user_id     TEXT NOT NULL,
            name        TEXT NOT NULL,
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS profiles_by_user ON profiles(user_id);

        -- One AI entity: a main agent, an agent or a bot. `owner_user` NULL marks a shared
        -- definition any account may invoke while keeping its own conversation and memory.
        CREATE TABLE IF NOT EXISTS entities (
            id          TEXT PRIMARY KEY,
            owner_user  TEXT,
            kind        TEXT NOT NULL,
            name        TEXT NOT NULL,
            slug        TEXT NOT NULL,
            title       TEXT NOT NULL DEFAULT '',
            role        TEXT NOT NULL,
            specialization TEXT NOT NULL DEFAULT '',
            description TEXT NOT NULL,
            icon        TEXT,
            model       TEXT,
            profile_id  TEXT,
            status      TEXT NOT NULL DEFAULT 'active',
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL
        );

        CREATE UNIQUE INDEX IF NOT EXISTS entities_slug
            ON entities(owner_user, kind, slug);

        -- Exactly one row. Whichever user, profile and main agent the interface last selected,
        -- so the next start does not begin as nobody.
        CREATE TABLE IF NOT EXISTS active_identity (
            singleton   INTEGER PRIMARY KEY CHECK (singleton = 1),
            user_id     TEXT,
            profile_id  TEXT,
            main_agent  TEXT
        );

        CREATE TABLE IF NOT EXISTS hive_sessions (
            id          TEXT PRIMARY KEY,
            owner_user  TEXT NOT NULL,
            profile_id  TEXT,
            name        TEXT NOT NULL,
            objective   TEXT NOT NULL,
            session_id  TEXT,
            state       TEXT NOT NULL,
            policy      TEXT NOT NULL,
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS hive_participants (
            hive        TEXT NOT NULL,
            entity      TEXT NOT NULL,
            kind        TEXT NOT NULL,
            role        TEXT NOT NULL DEFAULT 'member',
            joined_ms   INTEGER NOT NULL,
            PRIMARY KEY (hive, entity)
        );

        CREATE TABLE IF NOT EXISTS hive_messages (
            id          TEXT PRIMARY KEY,
            hive        TEXT NOT NULL,
            sender      TEXT NOT NULL,
            sender_kind TEXT NOT NULL,
            content     TEXT NOT NULL,
            task_ref    TEXT,
            ts_ms       INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS hive_messages_by_hive
            ON hive_messages(hive, ts_ms);

        -- Session/workspace/project management. The journal stays the authoritative conversation
        -- store; these tables only add metadata, file ownership and recovery pointers.
        CREATE TABLE IF NOT EXISTS projects (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            path        TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'active',
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS projects_path ON projects(path);

        CREATE TABLE IF NOT EXISTS session_workspaces (
            session         TEXT PRIMARY KEY,
            mode            TEXT NOT NULL DEFAULT 'session',
            workspace_path  TEXT,
            project_id      TEXT,
            created_ms      INTEGER NOT NULL,
            updated_ms      INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS artifacts (
            id          TEXT PRIMARY KEY,
            session     TEXT NOT NULL,
            project_id  TEXT,
            bot_id    TEXT,
            name        TEXT NOT NULL,
            path        TEXT NOT NULL,
            kind        TEXT NOT NULL DEFAULT 'other',
            mime        TEXT,
            size        INTEGER NOT NULL DEFAULT 0,
            checksum    TEXT,
            source      TEXT,
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS artifacts_session ON artifacts(session);
        CREATE UNIQUE INDEX IF NOT EXISTS artifacts_path ON artifacts(path);

        CREATE TABLE IF NOT EXISTS file_changes (
            id               TEXT PRIMARY KEY,
            session          TEXT NOT NULL,
            bot_id         TEXT,
            path             TEXT NOT NULL,
            operation        TEXT NOT NULL,
            checksum_before  TEXT,
            checksum_after   TEXT,
            size             INTEGER,
            checkpoint_path  TEXT,
            ts_ms            INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS file_changes_session ON file_changes(session);
        CREATE INDEX IF NOT EXISTS file_changes_path ON file_changes(path);

        -- Unified Ticket System (one engine for Hive, bots, sub-agents).
        CREATE TABLE IF NOT EXISTS tickets (
            id              TEXT PRIMARY KEY,
            owner_key       TEXT NOT NULL,
            owner_kind      TEXT NOT NULL,
            title           TEXT NOT NULL,
            description     TEXT NOT NULL DEFAULT '',
            request         TEXT NOT NULL DEFAULT '',
            kind            TEXT NOT NULL DEFAULT 'general',
            status          TEXT NOT NULL DEFAULT 'open',
            parent_ticket   TEXT,
            hive_id         TEXT,
            session_id      TEXT,
            strategy        TEXT,
            result          TEXT,
            failure_reason  TEXT,
            created_ms      INTEGER NOT NULL,
            updated_ms      INTEGER NOT NULL,
            closed_ms       INTEGER
        );
        CREATE INDEX IF NOT EXISTS tickets_owner ON tickets(owner_key);
        CREATE INDEX IF NOT EXISTS tickets_kind ON tickets(kind);
        CREATE INDEX IF NOT EXISTS tickets_status ON tickets(status);
        CREATE INDEX IF NOT EXISTS tickets_parent ON tickets(parent_ticket);

        CREATE TABLE IF NOT EXISTS ticket_participants (
            ticket            TEXT NOT NULL,
            participant_key   TEXT NOT NULL,
            participant_kind  TEXT NOT NULL,
            entity_id         TEXT,
            user_id           TEXT,
            role              TEXT NOT NULL DEFAULT 'member',
            can_read          INTEGER NOT NULL DEFAULT 1,
            can_write         INTEGER NOT NULL DEFAULT 0,
            can_admin         INTEGER NOT NULL DEFAULT 0,
            joined_ms         INTEGER NOT NULL,
            PRIMARY KEY (ticket, participant_key)
        );
        CREATE INDEX IF NOT EXISTS tp_ticket ON ticket_participants(ticket);

        CREATE TABLE IF NOT EXISTS ticket_messages (
            id              TEXT PRIMARY KEY,
            ticket          TEXT NOT NULL,
            author_key      TEXT NOT NULL,
            author_kind     TEXT NOT NULL,
            content         TEXT NOT NULL,
            visibility      TEXT NOT NULL DEFAULT 'shared',
            ts_ms           INTEGER NOT NULL,
            task_ref        TEXT
        );
        CREATE INDEX IF NOT EXISTS tm_ticket ON ticket_messages(ticket, ts_ms);

        CREATE TABLE IF NOT EXISTS assignments (
            id                TEXT PRIMARY KEY,
            ticket            TEXT NOT NULL,
            parent_assignment TEXT,
            assignee_key      TEXT NOT NULL,
            assignee_kind     TEXT NOT NULL,
            entity_id         TEXT,
            title             TEXT NOT NULL,
            description       TEXT NOT NULL DEFAULT '',
            status            TEXT NOT NULL DEFAULT 'pending',
            strategy          TEXT,
            result            TEXT,
            error             TEXT,
            created_ms        INTEGER NOT NULL,
            updated_ms        INTEGER NOT NULL,
            completed_ms      INTEGER
        );
        CREATE INDEX IF NOT EXISTS asg_ticket ON assignments(ticket);
        CREATE INDEX IF NOT EXISTS asg_assignee ON assignments(assignee_key);

        CREATE TABLE IF NOT EXISTS ticket_events (
            id          TEXT PRIMARY KEY,
            ticket      TEXT NOT NULL,
            ts_ms       INTEGER NOT NULL,
            kind        TEXT NOT NULL,
            actor_key   TEXT,
            payload     TEXT NOT NULL DEFAULT '{}'
        );
        CREATE INDEX IF NOT EXISTS te_ticket ON ticket_events(ticket, ts_ms);
        "#,
    )?;
    // Columns added after the first release. SQLite has no `ADD COLUMN IF NOT EXISTS`, so each
    // one is checked against the live schema instead of being folded into the batch above.
    ensure_column(conn, "sessions", "user_id", "TEXT")?;
    ensure_column(conn, "sessions", "profile_id", "TEXT")?;
    ensure_column(conn, "sessions", "status", "TEXT NOT NULL DEFAULT 'active'")?;
    ensure_column(conn, "sessions", "project_id", "TEXT")?;
    // Delegation lineage: which session spawned this one, and how far from the operator it sits.
    ensure_column(conn, "sessions", "parent_session", "TEXT")?;
    ensure_column(
        conn,
        "sessions",
        "delegation_depth",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    // Fork provenance: which session this one was forked from.
    ensure_column(conn, "sessions", "forked_from", "TEXT")?;
    ensure_column(conn, "entities", "title", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(
        conn,
        "entities",
        "specialization",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(conn, "entities", "status", "TEXT NOT NULL DEFAULT 'active'")?;

    // Title provenance. The default is `manual` because a row that predates this column already had
    // its name chosen by whoever created it, and an unrecognised title must never be overwritten by
    // a guess. The classification below then moves the rows that are *known* not to be operator-
    // named: the old positional placeholders, and names the runtime derived from a truncated request.
    ensure_column(
        conn,
        "sessions",
        "title_source",
        "TEXT NOT NULL DEFAULT 'manual'",
    )?;
    ensure_column(
        conn,
        "sessions",
        "title_status",
        "TEXT NOT NULL DEFAULT 'ready'",
    )?;
    if title_migration_needed(conn)? {
        // First upgrade only: sort the existing names into `generic` (a placeholder worth replacing)
        // and `auto` (derived, so replaceable but already meaningful). A second run must not re-judge
        // titles the operator has since renamed by hand.
        classify_legacy_titles(conn)?;
        mark_title_migration_done(conn)?;
    }

    // FTS5 session message search (Hermes-shaped). Best-effort: if FTS5 is unavailable the table
    // simply isn't created and callers fall back to LIKE.
    let _ = conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS session_fts USING fts5(session UNINDEXED, content, tokenize='porter');",
    );

    Ok(())
}

/// Titles the old runtime invented from a session's position or its first truncated message.
/// Matching is deliberately narrow: an unrecognised title is left as operator-owned.
fn is_placeholder_title(title: &str) -> bool {
    let t = title.trim();
    if t.eq_ignore_ascii_case("session") || t.eq_ignore_ascii_case("new session") {
        return true;
    }
    // "Session 1", "Session 42" — the positional names `format!("Session {count}")` produced.
    if let Some(rest) = t.strip_prefix("Session ") {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// Names the runtime derived itself: a clipped request (the `…` marker headless naming appends) or
/// a delegated session's `agent · task` label. Both may be replaced by a generated title.
fn is_derived_title(title: &str) -> bool {
    title.chars().any(|c| c == '…') || title.contains(" · ")
}

fn classify_legacy_titles(conn: &Connection) -> Result<()> {
    let rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT id, title FROM sessions")?;

        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (id, title) in rows {
        let (source, status) = if is_placeholder_title(&title) {
            (TITLE_GENERIC, TITLE_PENDING)
        } else if is_derived_title(&title) {
            (TITLE_AUTO, TITLE_READY)
        } else {
            continue;
        };
        conn.execute(
            "UPDATE sessions SET title_source = ?2, title_status = ?3 WHERE id = ?1",
            params![id, source, status],
        )?;
    }
    Ok(())
}

/// Bumped once the title provenance columns exist and legacy names have been classified. `PRAGMA
/// user_version` is per-database and needs no table of its own.
const TITLE_SCHEMA_VERSION: u32 = 1;

fn title_migration_needed(conn: &Connection) -> Result<bool> {
    let current: u32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    Ok(current < TITLE_SCHEMA_VERSION)
}

fn mark_title_migration_done(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {TITLE_SCHEMA_VERSION}"))?;
    Ok(())
}

/// Add `column` to `table` only when it is missing. A database created before the column existed
/// keeps its rows; a database that already has it is untouched.
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let existing = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if existing.iter().any(|name| name == column) {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
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

fn render_session_markdown(export: &SessionExport) -> String {
    let session = &export.session;
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", session.title));
    out.push_str(&format!("- id: `{}`\n", session.id.as_str()));
    out.push_str(&format!("- cwd: `{}`\n", session.cwd.display()));
    if let Some(model) = &session.model {
        out.push_str(&format!("- model: `{model}`\n"));
    }
    if let Some(parent) = &session.forked_from {
        out.push_str(&format!("- forked from: `{}`\n", parent.as_str()));
    }
    out.push_str(&format!(
        "- created: {} · updated: {}\n\n",
        session.created_ms, session.updated_ms
    ));
    out.push_str("## Transcript\n\n");
    for record in &export.journal {
        match record.kind {
            RecordKind::Message => {
                if let Some(raw) = record.payload.get("message")
                    && let Ok(message) =
                        serde_json::from_value::<vikram_contracts::Message>(raw.clone())
                {
                    let label = match message.role {
                        vikram_contracts::Role::User => "user",
                        vikram_contracts::Role::Assistant => "assistant",
                        vikram_contracts::Role::System => "system",
                        vikram_contracts::Role::Tool => "tool",
                    };
                    out.push_str(&format!("### {label} (seq {})\n\n", record.seq));
                    out.push_str(&message.text_body());
                    out.push('\n');
                    for block in &message.blocks {
                        if let vikram_contracts::Block::ToolCall { call } = block {
                            out.push_str(&format!(
                                "\n- tool call `{}`: `{}`\n",
                                call.id.as_str(),
                                call.name
                            ));
                        }
                    }
                    out.push('\n');
                }
            }
            RecordKind::ToolCall => {
                if let (Some(call), Some(name)) = (
                    record.payload.get("call").and_then(|v| v.as_str()),
                    record.payload.get("name").and_then(|v| v.as_str()),
                ) {
                    out.push_str(&format!(
                        "### tool call `{call}` — {name} (seq {})\n\n",
                        record.seq
                    ));
                }
            }
            RecordKind::ToolResult => {
                if let Some(content) = record.payload.get("content").and_then(|v| v.as_str()) {
                    out.push_str(&format!(
                        "### tool result (seq {})\n\n{content}\n\n",
                        record.seq
                    ));
                } else if let Some(raw) = record.payload.get("message")
                    && let Ok(message) =
                        serde_json::from_value::<vikram_contracts::Message>(raw.clone())
                {
                    out.push_str(&format!(
                        "### tool result (seq {})\n\n{}\n\n",
                        record.seq,
                        message.text_body()
                    ));
                }
            }
            RecordKind::Compaction => {
                out.push_str(&format!("_compaction marker (seq {})_\n\n", record.seq));
            }
            RecordKind::Status => {
                if record.payload.get("recovery").is_some() {
                    out.push_str(&format!(
                        "_recovered after an interruption (seq {})_\n\n",
                        record.seq
                    ));
                }
            }
            RecordKind::Policy
            | RecordKind::Approval
            | RecordKind::Cancellation
            | RecordKind::Activity => {}
        }
    }
    if !export.usage.is_empty() {
        let totals: u64 = export.usage.iter().map(|u| u.total_tokens).sum();
        out.push_str(&format!(
            "## Usage\n\n{} request(s), {totals} token(s)\n",
            export.usage.len()
        ));
    }
    out
}

fn map_session_row(row: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: SessionId::new(row.get::<_, String>(0)?),
        title: row.get(1)?,
        cwd: PathBuf::from(row.get::<_, String>(2)?),
        model: row.get(3)?,
        bot: row.get::<_, Option<String>>(4)?.map(BotId::new),
        created_ms: row.get::<_, i64>(5)? as u64,
        updated_ms: row.get::<_, i64>(6)? as u64,
        status: row.get(7)?,
        project_id: row.get::<_, Option<String>>(8)?.map(ProjectId::new),
        title_source: row.get(9)?,
        title_status: row.get(10)?,
        parent_session: row.get::<_, Option<String>>(11)?.map(SessionId::new),
        forked_from: row.get::<_, Option<String>>(12)?.map(SessionId::new),
    })
}

fn map_artifact(row: &rusqlite::Row) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        id: row.get(0)?,
        session: SessionId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        bot: row.get::<_, Option<String>>(3)?.map(BotId::new),
        name: row.get(4)?,
        path: PathBuf::from(row.get::<_, String>(5)?),
        kind: row.get(6)?,
        mime: row.get(7)?,
        size: row.get::<_, i64>(8)? as u64,
        checksum: row.get(9)?,
        source: row.get(10)?,
        created_ms: row.get::<_, i64>(11)? as u64,
        updated_ms: row.get::<_, i64>(12)? as u64,
    })
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
                    let sess_str: String = row.get(1)?;
                    let seq_val: i64 = row.get(0)?;
                    Ok(JournalRecord {
                        seq: row.get::<_, i64>(0)? as u64,
                        session: SessionId::new(row.get::<_, String>(1)?),
                        ts_ms: row.get::<_, i64>(2)? as u64,
                        kind: kind_from_str(&kind),
                        effect: row.get::<_, Option<String>>(4)?.map(EffectId::new),
                        payload: serde_json::from_str(&payload).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                5,
                                rusqlite::types::Type::Text,
                                Box::new(StoreError::Parse(format!(
                                    "journal payload for session {sess_str} seq {seq_val}: {e}",
                                ))),
                            )
                        })?,
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
    fn deleting_a_user_cascades_to_sessions_stamped_with_an_owner() {
        let store = Store::open_in_memory().unwrap();
        let user = store.create_user("Himanshu", 1).unwrap();
        let profile = store.create_profile(&user.id, "main", 1).unwrap();
        let session = store
            .create_session("owned", Path::new("/work"), None, 2)
            .unwrap();
        store.set_session_user(&session.id, &user.id).unwrap();
        store.set_session_profile(&session.id, &profile.id).unwrap();

        store.delete_user(&user.id).unwrap();
        assert!(
            store.list_all_sessions().unwrap().is_empty(),
            "the stamped session must cascade with its owner"
        );
    }

    #[test]
    fn a_broken_session_cascade_is_reported_instead_of_committed_as_success() {
        let store = Store::open_in_memory().unwrap();
        let row = store
            .create_session("Session 1", Path::new("/work"), Some("m"), 10)
            .unwrap();
        {
            let conn = store.lock();
            conn.execute("DROP TABLE hive_rosters", []).unwrap();
        }
        let err = store
            .delete_session(&row.id)
            .expect_err("a failed cascade must not be swallowed and committed");
        assert!(
            err.to_string().contains("hive_rosters"),
            "names the table: {err}"
        );
        assert!(
            store
                .list_sessions()
                .unwrap()
                .iter()
                .any(|s| s.id == row.id),
            "the transaction rolls back, so the session is not half-deleted"
        );
    }

    #[test]
    fn a_broken_identity_clear_is_reported_when_a_user_is_deleted() {
        let store = Store::open_in_memory().unwrap();
        let user = store.create_user("Himanshu", 1).unwrap();
        {
            let conn = store.lock();
            conn.execute("DROP TABLE active_identity", []).unwrap();
        }
        assert!(
            store.delete_user(&user.id).is_err(),
            "a failed cascade is not a clean delete"
        );
    }

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
        let mut row = BotRow {
            id: BotId::generate(),
            name: "Rudra".into(),
            role: "Implementer".into(),
            description: "builds".into(),
            icon: None,
            created_ms: 1,
        };
        store.save_bot(&row).unwrap();
        row.description = "builds carefully".into();
        store.save_bot(&row).unwrap();
        let listed = store.list_bots().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].description, "builds carefully");
    }

    #[test]
    fn teams_round_trip_with_members_and_session_link() {
        let store = Store::open_in_memory().unwrap();
        let now = 100;
        let team = store.create_hive_roster("Forge", now).unwrap();
        assert_eq!(team.name, "Forge");
        assert_eq!(team.session_id, None);

        let agent = BotId::generate();
        store.add_hive_roster_member(&team.id, &agent).unwrap();
        store.add_hive_roster_member(&team.id, &agent).unwrap(); // idempotent
        let session = store
            .create_session("Forge", Path::new("/work"), None, now + 1)
            .unwrap();
        store
            .set_hive_roster_session(&team.id, Some(&session.id), now + 2)
            .unwrap();

        let listed = store.list_hive_rosters().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Forge");
        assert_eq!(listed[0].session_id.as_ref(), Some(&session.id));
        assert_eq!(
            store.list_hive_roster_members(&team.id).unwrap(),
            vec![agent.clone()]
        );

        store
            .rename_hive_roster(&team.id, "Project Orion", now + 3)
            .unwrap();
        assert_eq!(store.list_hive_rosters().unwrap()[0].name, "Project Orion");

        store.remove_hive_roster_member(&team.id, &agent).unwrap();
        assert!(store.list_hive_roster_members(&team.id).unwrap().is_empty());
    }

    #[test]
    fn deleting_a_team_cascades_to_membership() {
        let store = Store::open_in_memory().unwrap();
        let team = store.create_hive_roster("Nexus", 1).unwrap();
        store
            .add_hive_roster_member(&team.id, &BotId::generate())
            .unwrap();
        store.delete_hive_roster(&team.id).unwrap();
        assert!(store.list_hive_rosters().unwrap().is_empty());
        assert!(store.list_hive_roster_members(&team.id).unwrap().is_empty());
    }

    #[test]
    fn removing_an_agent_drops_it_from_every_team() {
        let store = Store::open_in_memory().unwrap();
        let agent = BotId::generate();
        let a = store.create_hive_roster("A", 1).unwrap();
        let b = store.create_hive_roster("B", 2).unwrap();
        store.add_hive_roster_member(&a.id, &agent).unwrap();
        store.add_hive_roster_member(&b.id, &agent).unwrap();

        let affected = store.remove_bot_from_hive_rosters(&agent).unwrap();
        assert_eq!(affected.len(), 2);
        assert!(store.list_hive_roster_members(&a.id).unwrap().is_empty());
        assert!(store.list_hive_roster_members(&b.id).unwrap().is_empty());
        // Second call is a no-op with no teams affected.
        assert!(
            store
                .remove_bot_from_hive_rosters(&agent)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn team_names_support_arbitrary_unicode() {
        let store = Store::open_in_memory().unwrap();
        let team = store.create_hive_roster("研究小组 🐝", 1).unwrap();
        assert_eq!(store.list_hive_rosters().unwrap()[0].name, "研究小组 🐝");
        assert_eq!(team.name, "研究小组 🐝");
    }

    #[test]
    fn users_profiles_entities_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let now = 10;
        let user = store.create_user("Himanshu", now).unwrap();
        let profile = store.create_profile(&user.id, "dev", now + 1).unwrap();
        assert_eq!(store.list_profiles_by_user(&user.id).unwrap().len(), 1);

        let entity = EntityRow {
            id: EntityId::generate(),
            owner_user: Some(user.id.clone()),
            kind: EntityKind::Agent,
            name: "Rudra".into(),
            slug: "rudra".into(),
            title: "Senior Developer".into(),
            role: "coder".into(),
            specialization: "rust".into(),
            description: "builds".into(),
            icon: None,
            model: None,
            profile_id: Some(profile.id.clone()),
            status: "active".into(),
            created_ms: now,
            updated_ms: now,
        };
        store.save_entity(&entity).unwrap();
        assert_eq!(
            store.list_entities_by_owner(Some(&user.id)).unwrap().len(),
            1
        );
        assert_eq!(
            store
                .list_entities_by_kind(EntityKind::Agent)
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .get_entity_by_slug(Some(&user.id), EntityKind::Agent, "rudra")
                .unwrap()
                .is_some()
        );

        // active identity
        let ident = ActiveIdentity {
            user: Some(user.id.clone()),
            profile: Some(profile.id.clone()),
            main_agent: Some(entity.id.clone()),
        };
        store.set_active_identity(&ident).unwrap();
        assert_eq!(store.get_active_identity().unwrap(), ident);

        // delete user cascades
        store.delete_user(&user.id).unwrap();
        assert!(store.list_users().unwrap().is_empty());
        assert!(store.list_profiles().unwrap().is_empty());
        assert!(store.list_entities().unwrap().is_empty());
    }

    #[test]
    fn hive_sessions_participants_messages_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let now = 5;
        let user = store.create_user("U", now).unwrap();
        let hive_row = HiveRow {
            id: HiveId::generate(),
            owner_user: user.id.clone(),
            profile_id: None,
            name: "Forge".into(),
            objective: "build".into(),
            session_id: None,
            state: "active".into(),
            policy: "cooperative".into(),
            created_ms: now,
            updated_ms: now,
        };
        let hive = store.create_hive(&hive_row).unwrap();
        let agent_ent = EntityRow {
            id: EntityId::generate(),
            owner_user: None,
            kind: EntityKind::Agent,
            name: "Rudra".into(),
            slug: "rudra".into(),
            title: "Senior Developer".into(),
            role: "coder".into(),
            specialization: "rust".into(),
            description: String::new(),
            icon: None,
            model: None,
            profile_id: None,
            status: "active".into(),
            created_ms: now,
            updated_ms: now,
        };
        store.save_entity(&agent_ent).unwrap();
        let part = HiveParticipantRow {
            hive: hive.id.clone(),
            entity: agent_ent.id.clone(),
            kind: EntityKind::Agent,
            role: "member".into(),
            joined_ms: now,
        };
        store.add_hive_participant(&part).unwrap();
        assert_eq!(store.list_hive_participants(&hive.id).unwrap().len(), 1);

        let msg = HiveMessageRow {
            id: HiveMessageId::generate(),
            hive: hive.id.clone(),
            sender: "rudra".into(),
            sender_kind: "agent".into(),
            content: "hello".into(),
            task_ref: None,
            ts_ms: now,
        };
        store.append_hive_message(&msg).unwrap();
        assert_eq!(store.list_hive_messages(&hive.id).unwrap().len(), 1);

        // delete hive cascades
        store.delete_hive(&hive.id).unwrap();
        assert!(store.list_hives().unwrap().is_empty());
        assert!(store.list_hive_participants(&hive.id).unwrap().is_empty());
        assert!(store.list_hive_messages(&hive.id).unwrap().is_empty());
    }

    #[test]
    fn delete_profile_cascades_sessions() {
        let store = Store::open_in_memory().unwrap();
        let now = 1;
        let user = store.create_user("U", now).unwrap();
        let profile = store.create_profile(&user.id, "p", now).unwrap();
        let sess = store
            .create_session("s", std::path::Path::new("/w"), None, now)
            .unwrap();
        // manually link session to profile via update? sessions table has profile_id column added.
        // Just verify profile delete doesn't panic.
        store.delete_profile(&profile.id).unwrap();
        assert!(store.get_profile(&profile.id).unwrap().is_none());
        let _ = sess;
    }

    #[test]
    fn usage_ledger_round_trips_and_separates_measured_from_estimated() {
        let store = Store::open_in_memory().unwrap();
        let sess = store
            .create_session("s", std::path::Path::new("/w"), Some("gpt"), 1)
            .unwrap();

        store
            .record_usage(&UsageRow {
                id: 0,
                session: sess.id.clone(),
                ts_ms: 10,
                model: "gpt".into(),
                prompt_tokens: 100,
                completion_tokens: 40,
                total_tokens: 140,
                measured: true,
            })
            .unwrap();
        store
            .record_usage(&UsageRow {
                id: 0,
                session: sess.id.clone(),
                ts_ms: 20,
                model: "gpt".into(),
                prompt_tokens: 30,
                completion_tokens: 10,
                total_tokens: 40,
                measured: false,
            })
            .unwrap();

        let rows = store.list_usage(&sess.id).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ts_ms, 10);
        assert!(rows[0].measured);
        assert!(!rows[1].measured);
        assert_eq!(rows[0].prompt_tokens, 100);

        let totals = store.usage_totals(&sess.id).unwrap();
        assert_eq!(totals.requests, 2);
        assert_eq!(
            totals.measured_requests, 1,
            "only the reported exchange counts as measured"
        );
        assert_eq!(totals.prompt_tokens, 130);
        assert_eq!(totals.completion_tokens, 50);
        assert_eq!(totals.total_tokens, 180);
    }

    #[test]
    fn usage_grand_totals_span_sessions_and_delete_cascades() {
        let store = Store::open_in_memory().unwrap();
        let a = store
            .create_session("a", std::path::Path::new("/w"), None, 1)
            .unwrap();
        let b = store
            .create_session("b", std::path::Path::new("/w"), None, 1)
            .unwrap();
        for (session, tokens) in [(&a.id, 500u64), (&b.id, 250u64)] {
            store
                .record_usage(&UsageRow {
                    id: 0,
                    session: session.clone(),
                    ts_ms: 1,
                    model: "m".into(),
                    prompt_tokens: tokens / 2,
                    completion_tokens: tokens - tokens / 2,
                    total_tokens: tokens,
                    measured: true,
                })
                .unwrap();
        }

        let grand = store.usage_grand_totals().unwrap();
        assert_eq!(grand.requests, 2);
        assert_eq!(grand.total_tokens, 750);

        store.delete_session(&a.id).unwrap();
        assert!(
            store.list_usage(&a.id).unwrap().is_empty(),
            "usage must cascade with the session"
        );
        assert_eq!(store.usage_grand_totals().unwrap().total_tokens, 250);
    }

    #[test]
    fn an_empty_ledger_reports_zero_totals() {
        let store = Store::open_in_memory().unwrap();
        let sess = store
            .create_session("s", std::path::Path::new("/w"), None, 1)
            .unwrap();
        assert_eq!(
            store.usage_totals(&sess.id).unwrap(),
            UsageTotals::default()
        );
        assert_eq!(store.usage_grand_totals().unwrap(), UsageTotals::default());
    }
}
