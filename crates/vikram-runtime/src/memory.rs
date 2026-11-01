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

    pub fn get<K, S>(&self, key: K, scope: S) -> Option<String>
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
        .ok()
        .flatten()
    }

    pub fn search<Q, S>(&self, query: Q, scope: S) -> Vec<(String, String)>
    where
        Q: AsRef<str>,
        S: AsRef<str>,
    {
        let query = query.as_ref();
        let scope = scope.as_ref();
        let conn = self.lock();
        let mut stmt =
            match conn.prepare("SELECT key, value FROM memory WHERE scope = ?1 ORDER BY key ASC") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
        let rows = stmt
            .query_map(params![scope], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .ok();
        let collected: Vec<(String, String)> = match rows {
            Some(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            None => Vec::new(),
        };
        collected
            .into_iter()
            .filter(|(k, v)| k.contains(query) || v.contains(query))
            .collect()
    }

    pub fn list<S>(&self, scope: S) -> Vec<(String, String)>
    where
        S: AsRef<str>,
    {
        let scope = scope.as_ref();
        let conn = self.lock();
        let mut stmt =
            match conn.prepare("SELECT key, value FROM memory WHERE scope = ?1 ORDER BY key ASC") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
        let rows = stmt
            .query_map(params![scope], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .ok();
        match rows {
            Some(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            None => Vec::new(),
        }
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

    pub fn count<S>(&self, scope: S) -> usize
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
        .unwrap_or(0)
    }

    /// Total number of entries across all scopes.
    pub fn total_count(&self) -> usize {
        let conn = self.lock();
        conn.query_row("SELECT COUNT(*) FROM memory", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|n| n as usize)
        .unwrap_or(0)
    }
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
    fn save_and_get_round_trip() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("greeting", "hello", "global").unwrap();
        assert_eq!(store.get("greeting", "global"), Some("hello".to_string()));
        assert_eq!(store.get("greeting", "other"), None);
    }

    #[test]
    fn save_upserts_value() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("k", "v1", "global").unwrap();
        store.save("k", "v2", "global").unwrap();
        assert_eq!(store.get("k", "global"), Some("v2".to_string()));
        assert_eq!(store.count("global"), 1);
    }

    #[test]
    fn search_matches_key_and_value() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("alpha", "first value", "global").unwrap();
        store.save("beta", "second", "global").unwrap();
        let hits = store.search("value", "global");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "alpha");
        let hits = store.search("beta", "global");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn list_returns_all_for_scope() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("a", "1", "global").unwrap();
        store.save("b", "2", "global").unwrap();
        let all = store.list("global");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn delete_returns_bool_and_removes() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("k", "v", "global").unwrap();
        assert!(store.delete("k", "global").unwrap());
        assert!(!store.delete("k", "global").unwrap());
        assert_eq!(store.get("k", "global"), None);
    }

    #[test]
    fn scope_isolation() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("k", "global-val", "global").unwrap();
        store.save("k", "session-val", "session-1").unwrap();
        assert_eq!(store.get("k", "global"), Some("global-val".to_string()));
        assert_eq!(store.get("k", "session-1"), Some("session-val".to_string()));
        assert_eq!(store.count("global"), 1);
        assert_eq!(store.count("session-1"), 1);
        assert_eq!(store.list("global").len(), 1);
    }
}
