//! Persistent memory backed by SQLite.
//!
//! A small key/value store scoped by `scope` ("global" or a session/agent id). All access is
//! serialised through a mutex around the underlying connection, mirroring `vikram_store::Store`.

use crate::error::{Result, RuntimeError};
use crate::types::now_ms;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone)]
pub struct MemoryStore {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore").finish_non_exhaustive()
    }
}

impl MemoryStore {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| RuntimeError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path).map_err(|e| RuntimeError::Store(e.to_string()))?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| RuntimeError::Store(e.to_string()))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("memory store mutex not poisoned")
    }

    pub fn save<K, V, S>(&self, key: K, value: V, scope: S) -> Result<()>
    where
        K: AsRef<str>,
        V: AsRef<str>,
        S: AsRef<str>,
    {
        let key = key.as_ref();
        let value = value.as_ref();
        let scope = scope.as_ref();
        let now = now_ms() as i64;
        let conn = self.lock();
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM memory WHERE key = ?1 AND scope = ?2",
                params![key, scope],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;

        if let Some(id) = existing {
            conn.execute(
                "UPDATE memory SET value = ?1, updated_ms = ?2 WHERE id = ?3",
                params![value, now, id],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        } else {
            let id = format!("{scope}\u{1f}{key}");
            conn.execute(
                "INSERT INTO memory (id, key, value, scope, created_ms, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, key, value, scope, now, now],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        }
        Ok(())
    }

    pub fn get<K, S>(&self, key: K, scope: S) -> Result<Option<String>>
    where
        K: AsRef<str>,
        S: AsRef<str>,
    {
        let key = key.as_ref();
        let scope = scope.as_ref();
        let conn = self.lock();
        conn.query_row(
            "SELECT value FROM memory WHERE key = ?1 AND scope = ?2",
            params![key, scope],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| RuntimeError::Store(e.to_string()))
    }

    pub fn search<Q, S>(&self, query: Q, scope: S) -> Result<Vec<(String, String)>>
    where
        Q: AsRef<str>,
        S: AsRef<str>,
    {
        let query = query.as_ref();
        let scope = scope.as_ref();
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT key, value FROM memory WHERE scope = ?1 ORDER BY key ASC")
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let mut rows = stmt
            .query_map(params![scope], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let collected = read_pairs(&mut rows)?;
        Ok(collected
            .into_iter()
            .filter(|(k, v)| k.contains(query) || v.contains(query))
            .collect())
    }

    /// Ranked recall for the context builder, restricted to the scopes the caller is allowed to see.
    ///
    /// The stored `search` answers a whole-phrase question; this answers "what does this turn
    /// overlap with". Terms are matched with parameterized `LIKE` clauses, then each candidate is
    /// scored on how many terms it hits and how recently it was touched, so a stale note that
    /// matches one word cannot outrank a current note that matches five.
    ///
    /// `scopes` is a deny-first guard. Every conversation writes into one shared table keyed only by
    /// a free-form `scope` string, so an unscoped sweep would surface another session's or entity's
    /// notes into this one's context. Recall is confined to the scopes named here, and to nothing
    /// when none are given.
    pub fn recall_terms(
        &self,
        terms: &[String],
        scopes: &[String],
        limit: usize,
    ) -> Result<Vec<(String, String, String)>> {
        if terms.is_empty() || limit == 0 || scopes.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let scope_placeholders = scopes.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let clauses = terms
            .iter()
            .map(|_| "(key LIKE ? OR value LIKE ?)")
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT scope, key, value, updated_ms FROM memory WHERE scope IN ({scope_placeholders}) \
             AND ({clauses}) ORDER BY updated_ms DESC LIMIT 200"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let mut bindings: Vec<String> = scopes.to_vec();
        for term in terms {
            bindings.push(format!("%{term}%"));
            bindings.push(format!("%{term}%"));
        }
        let params: Vec<&dyn rusqlite::ToSql> =
            bindings.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        let mut rows = stmt
            .query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? as u64,
                ))
            })
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let mut scored: Vec<(u64, (String, String, String))> = Vec::new();
        for row in read_quadruples(&mut rows)? {
            let (scope, key, value, updated_ms) = row;
            let haystack = format!("{key} {}", value.to_lowercase());
            let hits = terms
                .iter()
                .filter(|term| haystack.contains(&term.to_lowercase()))
                .count() as u64;
            // Recency breaks ties inside a rough day band, never dominates a real match.
            let score = hits * 1_000 + updated_ms / (24 * 3_600_000);
            scored.push((score, (scope, key, value)));
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry)
            .collect())
    }

    /// Every key in every scope, ordered by scope then key. The interface shows what the model has
    /// remembered; a scope-filtered list would make the panel depend on a word the operator has to
    /// guess first.
    pub fn list_all(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT scope, key, value FROM memory ORDER BY scope ASC, key ASC")
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let mut rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let mut collected = Vec::new();
        while let Some(row) = rows.next() {
            let row = row.map_err(|e| RuntimeError::Store(e.to_string()))?;
            collected.push(row);
        }
        Ok(collected)
    }

    pub fn list<S>(&self, scope: S) -> Result<Vec<(String, String)>>
    where
        S: AsRef<str>,
    {
        let scope = scope.as_ref();
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT key, value FROM memory WHERE scope = ?1 ORDER BY key ASC")
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let mut rows = stmt
            .query_map(params![scope], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let collected = read_pairs(&mut rows)?;
        Ok(collected)
    }

    pub fn delete<K, S>(&self, key: K, scope: S) -> Result<bool>
    where
        K: AsRef<str>,
        S: AsRef<str>,
    {
        let key = key.as_ref();
        let scope = scope.as_ref();
        let conn = self.lock();
        let n = conn
            .execute(
                "DELETE FROM memory WHERE key = ?1 AND scope = ?2",
                params![key, scope],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        Ok(n > 0)
    }

    pub fn count<S>(&self, scope: S) -> Result<usize>
    where
        S: AsRef<str>,
    {
        let scope = scope.as_ref();
        let conn = self.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM memory WHERE scope = ?1",
            params![scope],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n as usize)
        .map_err(|e| RuntimeError::Store(e.to_string()))
    }

    /// Total number of entries across all scopes.
    pub fn total_count(&self) -> Result<usize> {
        let conn = self.lock();
        conn.query_row("SELECT COUNT(*) FROM memory", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|n| n as usize)
        .map_err(|e| RuntimeError::Store(e.to_string()))
    }
}

fn read_pairs<I>(rows: I) -> Result<Vec<(String, String)>>
where
    I: Iterator<Item = std::result::Result<(String, String), rusqlite::Error>>,
{
    rows.map(|row| row.map_err(|e| RuntimeError::Store(e.to_string())))
        .collect()
}

fn read_quadruples<I>(rows: I) -> Result<Vec<(String, String, String, u64)>>
where
    I: Iterator<Item = std::result::Result<(String, String, String, u64), rusqlite::Error>>,
{
    rows.map(|row| row.map_err(|e| RuntimeError::Store(e.to_string())))
        .collect()
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS memory (
            id          TEXT PRIMARY KEY,
            key         TEXT NOT NULL,
            value       TEXT NOT NULL,
            scope       TEXT NOT NULL,
            created_ms  INTEGER NOT NULL,
            updated_ms  INTEGER NOT NULL
        );
        "#,
    )
    .map_err(|e| RuntimeError::Store(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scope_is_listed_so_a_panel_never_hides_what_was_remembered() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("greeting", "hello", "global").unwrap();
        store.save("convention", "tabs", "project").unwrap();
        let all = store.list_all().unwrap();
        assert_eq!(
            all,
            vec![
                (
                    "global".to_owned(),
                    "greeting".to_owned(),
                    "hello".to_owned()
                ),
                (
                    "project".to_owned(),
                    "convention".to_owned(),
                    "tabs".to_owned()
                ),
            ],
            "ordered by scope then key, and both scopes present"
        );
    }

    #[test]
    fn save_and_get_round_trip() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("greeting", "hello", "global").unwrap();
        assert_eq!(
            store.get("greeting", "global").unwrap(),
            Some("hello".to_string())
        );
        assert_eq!(store.get("greeting", "other").unwrap(), None);
    }

    #[test]
    fn save_upserts_value() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("k", "v1", "global").unwrap();
        store.save("k", "v2", "global").unwrap();
        assert_eq!(store.get("k", "global").unwrap(), Some("v2".to_string()));
        assert_eq!(store.count("global").unwrap(), 1);
    }

    #[test]
    fn search_matches_key_and_value() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("alpha", "first value", "global").unwrap();
        store.save("beta", "second", "global").unwrap();
        let hits = store.search("value", "global").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "alpha");
        let hits = store.search("beta", "global").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn list_returns_all_for_scope() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("a", "1", "global").unwrap();
        store.save("b", "2", "global").unwrap();
        let all = store.list("global").unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn delete_returns_bool_and_removes() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("k", "v", "global").unwrap();
        assert!(store.delete("k", "global").unwrap());
        assert!(!store.delete("k", "global").unwrap());
        assert_eq!(store.get("k", "global").unwrap(), None);
    }

    #[test]
    fn scope_isolation() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("k", "global-val", "global").unwrap();
        store.save("k", "session-val", "session-1").unwrap();
        assert_eq!(
            store.get("k", "global").unwrap(),
            Some("global-val".to_string())
        );
        assert_eq!(
            store.get("k", "session-1").unwrap(),
            Some("session-val".to_string())
        );
        assert_eq!(store.count("global").unwrap(), 1);
        assert_eq!(store.count("session-1").unwrap(), 1);
        assert_eq!(store.list("global").unwrap().len(), 1);
    }

    #[test]
    fn recall_terms_confines_results_to_the_granted_scopes() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .save("note", "deploy the payment gateway", "global")
            .unwrap();
        store
            .save("note", "the gateway secret lives in vault", "session-1")
            .unwrap();
        store
            .save("note", "a different user's gateway plan", "session-2")
            .unwrap();
        let terms = vec!["gateway".to_string()];

        let mine = store
            .recall_terms(&terms, &["global".into(), "session-1".into()], 8)
            .unwrap();
        let scopes: Vec<&str> = mine.iter().map(|(scope, _, _)| scope.as_str()).collect();
        assert!(
            scopes.contains(&"global"),
            "global notes are shared: {mine:?}"
        );
        assert!(
            scopes.contains(&"session-1"),
            "the session's own notes recall: {mine:?}"
        );
        assert!(
            !scopes.contains(&"session-2"),
            "a foreign session's note must never surface here: {mine:?}"
        );

        assert!(
            store.recall_terms(&terms, &[], 8).unwrap().is_empty(),
            "no granted scope means no recall at all, deny-first"
        );
    }

    #[test]
    fn a_read_failure_is_reported_instead_of_looking_like_an_empty_memory() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("k", "v", "global").unwrap();
        {
            let conn = store.lock();
            conn.execute("DROP TABLE memory", []).unwrap();
        }
        assert!(
            store.get("k", "global").is_err(),
            "missing table is not 'no value'"
        );
        assert!(
            store.list("global").is_err(),
            "missing table is not 'no entries'"
        );
        assert!(
            store.search("k", "global").is_err(),
            "missing table is not 'no matches'"
        );
        assert!(
            store.list_all().is_err(),
            "missing table is not 'nothing remembered'"
        );
        assert!(
            store.count("global").is_err(),
            "missing table is not a count of 0"
        );
        assert!(
            store.total_count().is_err(),
            "missing table is not a total of 0"
        );
    }

    #[test]
    fn a_write_failure_is_reported() {
        let store = MemoryStore::open_in_memory().unwrap();
        {
            let conn = store.lock();
            conn.execute("DROP TABLE memory", []).unwrap();
        }
        assert!(
            store.save("k", "v", "global").is_err(),
            "a failed save must not look like a saved memory"
        );
    }
}
