//! Persistent memory backed by SQLite.
//!
//! A small key/value store scoped by `scope` ("global" or a session/agent id). All access is
//! serialised through a mutex around the underlying connection, mirroring `vikram_store::Store`.
//!
//! # Hybrid retrieval
//!
//! `recall_terms` (and its richer sibling `hybrid_recall`) fuse four signals:
//!
//! 1. **BM25 over FTS5** (`memory_fts`, porter tokenizer) over `key + value`. The raw `rank`
//!    column is negative-BM25 (lower = better); it is mapped onto a positive, roughly
//!    `[0, ~1]` scale. When FTS5 can't run — a MATCH syntax error, the table is missing, or the
//!    bundled SQLite was built without it — `fts_or_like_scores` panics-free-falls back to a
//!    per-term `LIKE` token-overlap score, so retrieval degrades but never breaks.
//! 2. **Token overlap** — fraction of distinct query terms literally present (case-insensitive
//!    substring) in the entry; term-coverage is emphasised over raw frequency.
//! 3. **Recency + importance** — an exponential half-life decay (7 days, floored so ancient
//!    entries still count for something) multiplied into a factor from the entry's `importance`
//!    (0..1, default 0.5).
//! 4. **Cosine similarity** against an optional local embedding (f32 blob, no cloud API). Only
//!    scored when the caller supplies a query vector and the entry has a same-length blob.
//!
//! Scores are fused by a weighted sum, then reranked with **MMR** (Maximal Marginal Relevance,
//! lambda=0.7) over token-set Jaccard similarity so near-duplicate entries don't crowd the
//! result list out while distinct entries get a chance to surface.

use std::collections::{HashMap, HashSet};

/// Scope name for memory shared by every session of one operator: `user:<id>`.
///
/// The scope column is free-form; the `user:`/`project:` prefixes are the convention the
/// runtime's scope binding ([`MemoryStore::bind_session_scopes`]) and recall expansion rely
/// on. Returns `None` for an empty segment so a caller can fall back to a narrower scope
/// rather than write into a malformed shared shelf.
pub fn user_scope(user: &str) -> Option<String> {
    scope_segment(user).map(|seg| format!("user:{seg}"))
}

/// Scope name for memory shared by every session attached to one project or workspace:
/// `project:<slug>`. Same free-form convention and same `None`-on-empty contract as
/// [`user_scope`].
pub fn project_scope(slug: &str) -> Option<String> {
    scope_segment(slug).map(|seg| format!("project:{seg}"))
}

/// Scope name for memory shared by every participant of one hive: `hive:<id>`.
///
/// Hive participants run ad-hoc — they hold no session id, so the session-keyed
/// [`MemoryStore::bind_session_scopes`] binding cannot reach them. Instead the hive run
/// names this shelf explicitly in its recall scopes. Same free-form convention and same
/// `None`-on-empty contract as [`user_scope`]. Recall-only by convention: host code may
/// write here, but nothing in a run writes a participant's private notes onto the shelf.
pub fn hive_scope(hive: &str) -> Option<String> {
    scope_segment(hive).map(|seg| format!("hive:{seg}"))
}

/// Scope ids are path/DB-safe by construction: anything outside `[A-Za-z0-9._~-]` folds to
/// `_`, and an all-empty result is no scope at all.
fn scope_segment(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c
            } else {
                '_'
            }
        })
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

use crate::error::{Result, RuntimeError};
use crate::types::now_ms;
use rusqlite::{Connection, OptionalExtension, params};

/// Weights for the hybrid fusion. They sum to 1.0 so a caller can reason about the fused score
/// on the same scale as the component signals.
#[derive(Clone, Copy, Debug)]
pub struct HybridWeights {
    /// FTS5 BM25 (or token-overlap fallback) component.
    pub bm25: f64,
    /// Token-overlap component.
    pub overlap: f64,
    /// Recency + importance component.
    pub recency_importance: f64,
    /// Cosine-similarity component (only when a query vector + stored blobs exist).
    pub cosine: f64,
}

impl Default for HybridWeights {
    fn default() -> Self {
        Self {
            bm25: 0.40,
            overlap: 0.25,
            recency_importance: 0.20,
            cosine: 0.15,
        }
    }
}

/// Options for a single hybrid recall. `query_embedding: None` skips the cosine signal entirely —
/// callers that have no local embedder (the common case) pay nothing for it.
#[derive(Clone, Debug)]
pub struct HybridOptions {
    pub weights: HybridWeights,
    pub query_embedding: Option<Vec<f32>>,
    /// Half-life, in days, for the recency decay. Newer = more weight on recency.
    pub recency_half_life_days: f64,
    /// Maximal Marginal Relevance trade-off: 1.0 = pure relevance, 0.0 = pure diversity.
    pub mmr_lambda: f64,
    /// Upper bound on how many candidates MMR considers (before the result `limit` cut).
    pub candidate_pool: usize,
}

impl Default for HybridOptions {
    fn default() -> Self {
        Self {
            weights: HybridWeights::default(),
            query_embedding: None,
            recency_half_life_days: 7.0,
            mmr_lambda: 0.7,
            candidate_pool: 32,
        }
    }
}

/// One fused, reranked memory entry plus the component signals that produced its score.
/// The component breakdown is exposed so callers can explain a ranking to the operator instead
/// of presenting an opaque number.
#[derive(Clone, Debug)]
pub struct ScoredMemory {
    pub scope: String,
    pub key: String,
    pub value: String,
    pub updated_ms: u64,
    /// Fused score after `HybridWeights` and MMR-aware reranking (higher = more relevant).
    pub score: f64,
    pub bm25: f64,
    pub overlap: f64,
    pub recency_importance: f64,
    /// `None` when no query embedding or stored blob was available.
    pub cosine: Option<f64>,
}
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
        self.conn.lock().unwrap_or_else(|e| {
            eprintln!("vikram: memory store mutex poisoned; recovering");
            e.into_inner()
        })
    }

    /// Save with an explicit importance weight (0..1) and an optional f32 embedding.
    pub fn save_with_embedding<K, V, S>(
        &self,
        key: K,
        value: V,
        scope: S,
        importance: f64,
        embedding: Option<&[f32]>,
    ) -> Result<()>
    where
        K: AsRef<str>,
        V: AsRef<str>,
        S: AsRef<str>,
    {
        let key = key.as_ref();
        let value = value.as_ref();
        let scope = scope.as_ref();
        let importance = clamp01(importance);
        let embedding_blob: Option<Vec<u8>> = embedding.map(embed_to_bytes);
        let now = now_ms() as i64;
        let conn = self.lock();
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM memory WHERE key = ?1 AND scope = ?2
                 ORDER BY CASE WHEN status = 'active' THEN 0 ELSE 1 END, updated_ms DESC
                 LIMIT 1",
                params![key, scope],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;

        let id = if let Some(id) = existing {
            conn.execute(
                "UPDATE memory SET value = ?1, updated_ms = ?2, importance = ?3, embedding = ?4, status = 'active', superseded_by = NULL WHERE id = ?5",
                params![value, now, importance, embedding_blob.as_deref(), id],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
            id
        } else {
            let id = format!("{scope}\u{1f}{key}");
            conn.execute(
                "INSERT INTO memory (id, key, value, scope, created_ms, updated_ms, importance, embedding, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active')",
                params![id, key, value, scope, now, now, importance, embedding_blob.as_deref()],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
            id
        };
        // Keep the FTS5 index in step. Best-effort: a failed insert never fails the save.
        let _ = conn.execute("DELETE FROM memory_fts WHERE id = ?1", params![id]);
        let _ = conn.execute(
            "INSERT INTO memory_fts(id, key, value) VALUES (?1, ?2, ?3)",
            params![id, key, value],
        );
        Ok(())
    }

    /// Convenience: save keeping default importance (0.5) and no embedding.
    pub fn save<K, V, S>(&self, key: K, value: V, scope: S) -> Result<()>
    where
        K: AsRef<str>,
        V: AsRef<str>,
        S: AsRef<str>,
    {
        self.save_with_embedding(key, value, scope, 0.5, None)
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
            "SELECT value FROM memory WHERE key = ?1 AND scope = ?2 AND status = 'active'",
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
            .prepare("SELECT key, value FROM memory WHERE scope = ?1 AND status = 'active' ORDER BY key ASC")
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

    /// Record which shared shelves a session may recall: its `user:<id>` and
    /// `project:<slug>` scopes, or `None` for either dimension the session does not have.
    ///
    /// Only host code calls this — never a model-facing tool — so recall expansion can add
    /// exactly the shelves the runtime vouched for and nothing a caller invented. Passing
    /// `None` for both clears the binding, so a session that leaves its project stops
    /// seeing that project's shelf on the next recall.
    pub fn bind_session_scopes(
        &self,
        session: &str,
        user_scope: Option<&str>,
        project_scope: Option<&str>,
    ) -> Result<()> {
        let session = session.trim();
        if session.is_empty() {
            return Ok(());
        }
        let user_scope = user_scope.map(str::to_owned);
        let project_scope = project_scope.map(str::to_owned);
        let conn = self.lock();
        let existing = conn
            .query_row(
                "SELECT user_scope, project_scope FROM memory_scopes WHERE session = ?1",
                params![session],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let next = (user_scope, project_scope);
        if existing.as_ref() == Some(&next) {
            return Ok(());
        }
        if next.0.is_none() && next.1.is_none() {
            conn.execute(
                "DELETE FROM memory_scopes WHERE session = ?1",
                params![session],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
            return Ok(());
        }
        conn.execute(
            "INSERT INTO memory_scopes (session, user_scope, project_scope) VALUES (?1, ?2, ?3)
             ON CONFLICT(session) DO UPDATE SET
                 user_scope = excluded.user_scope,
                 project_scope = excluded.project_scope",
            params![session, next.0, next.1],
        )
        .map_err(|e| RuntimeError::Store(e.to_string()))?;
        Ok(())
    }

    /// The shared shelves a session was bound to, so a writer that only holds a session id (the
    /// CLI approval path, a bench) can land a memory where recall will look for it later instead of
    /// in a session scope that dies with the session.
    pub fn session_scopes(&self, session: &str) -> Result<(Option<String>, Option<String>)> {
        let session = session.trim();
        if session.is_empty() {
            return Ok((None, None));
        }
        let conn = self.lock();
        conn.query_row(
            "SELECT user_scope, project_scope FROM memory_scopes WHERE session = ?1",
            params![session],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()
        .map_err(|e| RuntimeError::Store(e.to_string()))
        .map(|row| row.unwrap_or((None, None)))
    }

    /// Ranked recall for the context builder, restricted to the scopes the caller is allowed to see.
    ///
    /// Delegate to `hybrid_recall` with default weights so the two entry points never drift apart
    /// in scope handling, scoring or reranking. The returned tuples are `(scope, key, value)`,
    /// as before — new callers that need the score breakdown should call `hybrid_recall` directly.
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
        let scored = self.hybrid_recall(terms, scopes, limit, &HybridOptions::default())?;
        Ok(scored
            .into_iter()
            .map(|s| (s.scope, s.key, s.value))
            .collect())
    }

    /// Hybrid recall with full score breakdown and MMR reranking.
    ///
    /// Same deny-first scope guard as `recall_terms`. The components (§module docs) are fused
    /// by `opts.weights` and the ranked pool is reranked with MMR before the `limit` cut.
    pub fn hybrid_recall(
        &self,
        terms: &[String],
        scopes: &[String],
        limit: usize,
        opts: &HybridOptions,
    ) -> Result<Vec<ScoredMemory>> {
        if terms.is_empty() || limit == 0 || scopes.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();

        // A scope that names a session also recalls the shared shelves the host bound to it
        // (`user:`/`project:`). The binding table is written only by runtime code, so the
        // expansion can widen recall by exactly what the session was already entitled to —
        // a caller naming another session's id gains nothing it could not already read by
        // passing that id's own memories.
        let scopes = expand_recall_scopes(&conn, scopes)?;

        // 1. Gather candidate rows confined to the granted scopes. The raw LIKE sweep mirrors
        //    the historical recall; the FTS5 BM25 signal is computed on top of this same pool
        //    so the union of signals never widens beyond the granted scopes.
        let scope_placeholders = scopes.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let clauses = terms
            .iter()
            .map(|_| "(key LIKE ? ESCAPE '\\' OR value LIKE ? ESCAPE '\\')")
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT id, scope, key, value, updated_ms, COALESCE(importance, 0.5), embedding \
             FROM memory WHERE scope IN ({scope_placeholders}) AND status = 'active' AND ({clauses}) \
             ORDER BY updated_ms DESC LIMIT 500"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let mut bindings: Vec<String> = scopes.to_vec();
        for term in terms {
            let pat = format!("%{}%", escape_like(term));
            bindings.push(pat.clone());
            bindings.push(pat);
        }
        let params: Vec<&dyn rusqlite::ToSql> =
            bindings.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        struct Row {
            id: String,
            scope: String,
            key: String,
            value: String,
            updated_ms: u64,
            importance: f64,
            embedding: Option<Vec<u8>>,
        }
        let rows: Vec<Row> = {
            let r = stmt
                .query_map(params.as_slice(), |row| {
                    Ok(Row {
                        id: row.get(0)?,
                        scope: row.get(1)?,
                        key: row.get(2)?,
                        value: row.get(3)?,
                        updated_ms: row.get::<_, i64>(4)? as u64,
                        importance: row.get::<_, f64>(5)?,
                        embedding: row.get(6)?,
                    })
                })
                .map_err(|e| RuntimeError::Store(e.to_string()))?;
            r.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| RuntimeError::Store(e.to_string()))?
        };
        drop(stmt);

        // 2. BM25 per memory id, via FTS5 with a token-overlap fallback.
        let bm25 = fts_or_like_scores(&conn, terms).unwrap_or_default();

        // 3. Fuse signals.
        let now = now_ms();
        let distinct_terms: HashSet<String> = terms
            .iter()
            .map(|t| t.to_lowercase())
            .collect::<HashSet<_>>();
        let mut scored: Vec<ScoredMemory> = rows
            .into_iter()
            .filter_map(|row| {
                let haystack = format!("{} {}", row.key.to_lowercase(), row.value.to_lowercase());
                let hits = distinct_terms
                    .iter()
                    .filter(|term| haystack.contains(term.as_str()))
                    .count();
                let overlap = if distinct_terms.is_empty() {
                    0.0
                } else {
                    hits as f64 / distinct_terms.len() as f64
                };
                // A true zero-overlap entry must not survive on recency alone.
                if hits == 0 && bm25.get(&row.id).copied().unwrap_or(0.0) <= 0.0 {
                    return None;
                }
                let b = bm25.get(&row.id).copied().unwrap_or(0.0);
                let recency = recency_importance_score(
                    row.updated_ms,
                    row.importance,
                    now,
                    opts.recency_half_life_days,
                );
                let cosine = opts.query_embedding.as_deref().and_then(|q| {
                    row.embedding
                        .as_deref()
                        .and_then(|blob| cosine_from_blob(q, blob))
                });
                let w = opts.weights;
                let score = w.bm25 * b
                    + w.overlap * overlap
                    + w.recency_importance * recency
                    + w.cosine * cosine.unwrap_or(0.0);
                Some(ScoredMemory {
                    scope: row.scope,
                    key: row.key,
                    value: row.value,
                    updated_ms: row.updated_ms,
                    score,
                    bm25: b,
                    overlap,
                    recency_importance: recency,
                    cosine,
                })
            })
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(opts.candidate_pool.max(limit));
        Ok(mmr_rerank(scored, opts.mmr_lambda, limit))
    }

    /// Every key in every scope, ordered by scope then key. The interface shows what the model has
    /// remembered; a scope-filtered list would make the panel depend on a word the operator has to
    /// guess first.
    pub fn list_all(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT scope, key, value FROM memory WHERE status = 'active' ORDER BY scope ASC, key ASC")
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let mut collected = Vec::new();
        for row in rows {
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
            .prepare("SELECT key, value FROM memory WHERE scope = ?1 AND status = 'active' ORDER BY key ASC")
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
        // Drop the FTS rows before the memory rows so the index never points at a gone id.
        let _ = conn.execute(
            "DELETE FROM memory_fts WHERE id IN (SELECT id FROM memory WHERE key = ?1 AND scope = ?2)",
            params![key, scope],
        );
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
            "SELECT COUNT(*) FROM memory WHERE scope = ?1 AND status = 'active'",
            params![scope],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n as usize)
        .map_err(|e| RuntimeError::Store(e.to_string()))
    }

    /// Total number of entries across all scopes.
    pub fn total_count(&self) -> Result<usize> {
        let conn = self.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM memory WHERE status = 'active'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n as usize)
        .map_err(|e| RuntimeError::Store(e.to_string()))
    }

    /// Consolidate the `memory` table: collapse duplicate rows, supersede
    /// contradictory values for the same `(scope, key)`, and expire stale
    /// low-importance entries.
    ///
    /// The `memory` table is a current-state key/value store — `save` already
    /// upserts by `(scope, key)`, so duplicate `(scope, key)` rows only appear
    /// through legacy data or foreign writers. Within such a group:
    ///
    /// - Rows whose values match after normalization keep the winner
    ///   (highest `importance`, then newest `updated_ms`); losers are deleted
    ///   outright — they carry no information the winner doesn't.
    /// - Rows with genuinely different values are a contradiction: the winner
    ///   stays `active`, losers become `status = 'superseded'` with a
    ///   `superseded_by` pointer, exactly like the observation lifecycle in
    ///   `consolidation.rs`. Nothing is deleted, so the history stays auditable.
    /// - Active rows older than [`MEMORY_MAX_AGE_MS`] whose importance-decayed
    ///   score falls below [`MEMORY_MIN_KEEP_SCORE`] become `expired`.
    ///
    /// Non-active rows are removed from `memory_fts` so recall never surfaces
    /// them, and every read path (`get`, `list`, `search`, `recall_terms`,
    /// counts) already filters on `status = 'active'`. Re-saving a key revives
    /// its row to `active`. Idempotent: a second run reports all zeros.
    pub fn consolidate(&self) -> Result<MemoryConsolidationReport> {
        let mut report = MemoryConsolidationReport::default();
        let now = now_ms() as i64;
        let mut conn = self.lock();

        struct Row {
            id: String,
            scope: String,
            key: String,
            value: String,
            importance: f64,
            updated_ms: i64,
            status: String,
        }
        let rows: Vec<Row> = {
            let mut stmt = conn
                .prepare("SELECT id, scope, key, value, importance, updated_ms, status FROM memory")
                .map_err(|e| RuntimeError::Store(e.to_string()))?;
            let mapped = stmt
                .query_map([], |r| {
                    Ok(Row {
                        id: r.get(0)?,
                        scope: r.get(1)?,
                        key: r.get(2)?,
                        value: r.get(3)?,
                        importance: r.get::<_, f64>(4)?,
                        updated_ms: r.get(5)?,
                        status: r.get(6)?,
                    })
                })
                .map_err(|e| RuntimeError::Store(e.to_string()))?;
            mapped
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| RuntimeError::Store(e.to_string()))?
        };
        report.examined = rows.len();

        // Winner ordering shared by dedup and contradiction resolution:
        // more important first, then more recently written.
        let cmp_rows = |a: usize, b: usize| {
            rows[a]
                .importance
                .partial_cmp(&rows[b].importance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(rows[a].updated_ms.cmp(&rows[b].updated_ms))
        };

        // Group candidate (active) rows by (scope, key); already-retired rows
        // are left untouched and never re-enter candidacy.
        let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            if row.status == "active" {
                groups
                    .entry((row.scope.clone(), row.key.clone()))
                    .or_default()
                    .push(i);
            }
        }

        let mut supersede: Vec<(String, String)> = Vec::new(); // (loser id, winner id)
        let mut delete_ids: Vec<String> = Vec::new();

        for idxs in groups.values() {
            if idxs.len() < 2 {
                continue;
            }
            // Dedup first: cluster by normalized value, keep one winner per
            // cluster, delete the rest (identical information, no lineage).
            let mut by_value: HashMap<String, Vec<usize>> = HashMap::new();
            for &i in idxs {
                by_value
                    .entry(normalize_memory_value(&rows[i].value))
                    .or_default()
                    .push(i);
            }
            let mut survivors: Vec<usize> = Vec::new();
            for cluster in by_value.values() {
                let winner = *cluster.iter().max_by(|&&a, &&b| cmp_rows(a, b)).unwrap();
                survivors.push(winner);
                for &i in cluster {
                    if i != winner {
                        delete_ids.push(rows[i].id.clone());
                        report.duplicates_removed += 1;
                    }
                }
            }
            // Contradiction: more than one distinct value survived dedup.
            if survivors.len() < 2 {
                continue;
            }
            report.contradictions += 1;
            let winner = *survivors.iter().max_by(|&&a, &&b| cmp_rows(a, b)).unwrap();
            let winner_id = rows[winner].id.clone();
            for &i in &survivors {
                if i != winner {
                    supersede.push((rows[i].id.clone(), winner_id.clone()));
                    report.superseded += 1;
                }
            }
        }

        // Aging: expire active rows that are old and unimportant.
        let mut expire_ids: Vec<String> = Vec::new();
        for row in &rows {
            if row.status != "active" {
                continue;
            }
            let age_ms = (now - row.updated_ms).max(0) as u64;
            if age_ms > MEMORY_MAX_AGE_MS
                && memory_keep_score(row.importance, age_ms) < MEMORY_MIN_KEEP_SCORE
            {
                expire_ids.push(row.id.clone());
                report.expired += 1;
            }
        }

        let tx = conn
            .transaction()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        for id in &delete_ids {
            tx.execute("DELETE FROM memory WHERE id = ?1", params![id])
                .map_err(|e| RuntimeError::Store(e.to_string()))?;
            let _ = tx.execute("DELETE FROM memory_fts WHERE id = ?1", params![id]);
        }
        for (loser, winner) in &supersede {
            tx.execute(
                "UPDATE memory SET status = 'superseded', superseded_by = ?1 WHERE id = ?2",
                params![winner, loser],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
            let _ = tx.execute("DELETE FROM memory_fts WHERE id = ?1", params![loser]);
        }
        for id in &expire_ids {
            tx.execute(
                "UPDATE memory SET status = 'expired' WHERE id = ?1",
                params![id],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
            let _ = tx.execute("DELETE FROM memory_fts WHERE id = ?1", params![id]);
        }
        tx.commit()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        Ok(report)
    }
}

/// The granted scope list plus the shared shelves bound to any session id in it, order
/// preserved and duplicates collapsed. Scopes with no binding contribute nothing.
fn expand_recall_scopes(conn: &Connection, scopes: &[String]) -> Result<Vec<String>> {
    let placeholders = scopes.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT session, user_scope, project_scope FROM memory_scopes WHERE session IN ({placeholders})"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| RuntimeError::Store(e.to_string()))?;
    let params: Vec<&dyn rusqlite::ToSql> =
        scopes.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let rows = stmt
        .query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| RuntimeError::Store(e.to_string()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| RuntimeError::Store(e.to_string()))?;
    drop(stmt);
    let mut bound: HashMap<String, Vec<String>> = HashMap::new();
    for (session, user, project) in rows {
        let extras: Vec<String> = [user, project].into_iter().flatten().collect();
        if !extras.is_empty() {
            bound.insert(session, extras);
        }
    }
    let mut out = scopes.to_vec();
    for scope in scopes {
        if let Some(extras) = bound.get(scope) {
            out.extend(extras.iter().cloned());
        }
    }
    let mut seen = HashSet::new();
    out.retain(|s| seen.insert(s.clone()));
    Ok(out)
}

fn read_pairs<I>(rows: I) -> Result<Vec<(String, String)>>
where
    I: Iterator<Item = std::result::Result<(String, String), rusqlite::Error>>,
{
    rows.map(|row| row.map_err(|e| RuntimeError::Store(e.to_string())))
        .collect()
}

/// BM25 scores per memory id from FTS5, falling back to per-term token-overlap when FTS5 is
/// unavailable or the query can't be turned into a MATCH expression. Never returns an Err —
/// a missing signal degrades to the fallback rather than emptying the result set.
fn fts_or_like_scores(conn: &Connection, terms: &[String]) -> Result<HashMap<String, f64>> {
    if terms.is_empty() {
        return Ok(HashMap::new());
    }
    let mut scores: HashMap<String, f64> = HashMap::new();

    // Try FTS5 BM25. The MATCH expression is an OR of quoted terms: quoting prevents FTS5
    // syntax injection from punctuation in the query and keeps the porter's stemming active.
    let match_expr = terms
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', " ")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let fts_ok = (|| -> std::result::Result<(), rusqlite::Error> {
        let mut stmt =
            conn.prepare("SELECT id, rank FROM memory_fts WHERE memory_fts MATCH ?1 LIMIT 500")?;
        let rows = stmt.query_map(params![match_expr], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?;
        // FTS5 `rank` is negative BM25: 0 means no score contributed, and more
        // negative = *worse*. A perfect match scores ~0, not ~1, so normalise by
        // distance from the best (least-negative) rank in the result set rather
        // than assuming an absolute scale. The squasher keeps the component in
        // [0, 1] next to the other normalised signals; when every candidate has the
        // same rank (a tiny corpus) the BM25 component is uniformly high so overlap
        // decides, which is the honest tie-break.
        let collected: Vec<(String, f64)> = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        let best_rank = collected
            .iter()
            .map(|(_, r)| *r)
            .fold(f64::INFINITY, f64::min);
        let _ = best_rank;
        for (id, rank) in collected {
            // Lower (more negative) rank = stronger BM25 evidence. Map the raw
            // rank through a smooth squasher anchored at the empirical best so the
            // strongest candidate lands near 1.0 and weaker ones decay from there.
            let gap: f64 = (-rank).max(0.0); // evidence strength, >= 0
            let bm25: f64 = 1.0 - (-gap).exp(); // saturating: strong evidence -> 1
            scores.insert(id, bm25.clamp(0.01_f64, 1.0_f64));
        }
        Ok(())
    })()
    .is_ok();
    if fts_ok && !scores.is_empty() {
        return Ok(scores);
    }

    // Fallback: token-overlap over the live table (covers FTS5 missing, a MATCH error, or a
    // corpus where the FTS index was never backfilled).
    let store_err = |e: rusqlite::Error| RuntimeError::Store(e.to_string());
    let mut stmt = conn
        .prepare("SELECT id, key, value FROM memory WHERE status = 'active'")
        .map_err(store_err)?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(store_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(store_err)?;
    let distinct: HashSet<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    for (id, key, value) in rows {
        let hay = format!("{} {}", key.to_lowercase(), value.to_lowercase());
        let hits = distinct.iter().filter(|t| hay.contains(t.as_str())).count();
        if hits > 0 {
            scores.insert(id, hits as f64 / distinct.len() as f64);
        }
    }
    Ok(scores)
}

/// Exponential recency decay (half-life) scaled by how important the entry is. The result is
/// floored at a small value so a very old, very important note still out-ranks a brand-new
/// trivial one by a hair rather than vanishing entirely.
fn recency_importance_score(
    updated_ms: u64,
    importance: f64,
    now_ms: u64,
    half_life_days: f64,
) -> f64 {
    let age_days = now_ms.saturating_sub(updated_ms) as f64 / 86_400_000.0;
    let decay = 0.5_f64.powf(age_days / half_life_days.max(0.1));
    (0.05 + 0.95 * decay) * clamp01(importance)
}

/// Escape `%`, `_` and `\` so a query term cannot act as a LIKE pattern. Pairs with the
/// `ESCAPE '\'` clause on every LIKE in `hybrid_recall`.
fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn clamp01(v: f64) -> f64 {
    if v.is_nan() { 0.5 } else { v.clamp(0.0, 1.0) }
}

/// Rows older than this are candidates for expiration during consolidation.
const MEMORY_MAX_AGE_MS: u64 = 90 * 24 * 3_600_000;
/// Keep-score floor for expiration, mirroring `ConsolidationPolicy::min_keep_score`.
const MEMORY_MIN_KEEP_SCORE: f64 = 0.05;
/// Half-life for the recency factor in `memory_keep_score`.
const MEMORY_HALF_LIFE_MS: u64 = 30 * 24 * 3_600_000;

/// What [`MemoryStore::consolidate`] changed. Counts are per-run; a clean store
/// reports all zeros, which also makes the method idempotent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryConsolidationReport {
    /// Total rows inspected (active and already-retired).
    pub examined: usize,
    /// Exact-duplicate rows deleted (same scope, key, normalized value).
    pub duplicates_removed: usize,
    /// `(scope, key)` groups that held genuinely different values.
    pub contradictions: usize,
    /// Rows marked `superseded` by a contradiction winner.
    pub superseded: usize,
    /// Rows marked `expired` for age + low importance.
    pub expired: usize,
}

/// Normalized-value comparison for dedup: case- and whitespace-insensitive,
/// matching `consolidation::normalize`.
fn normalize_memory_value(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Importance decayed by age — the memory-table analogue of the observation
/// score's importance*recency factors (no confidence/access columns exist here).
fn memory_keep_score(importance: f64, age_ms: u64) -> f64 {
    let decay = 0.5_f64.powf(age_ms as f64 / MEMORY_HALF_LIFE_MS as f64);
    clamp01(importance) * decay
}

fn embed_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

fn blob_to_f32(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.is_empty() || !blob.len().is_multiple_of(4) {
        return None;
    }
    Some(
        blob.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        dot += (*x as f64) * (*y as f64);
        na += (*x as f64) * (*x as f64);
        nb += (*y as f64) * (*y as f64);
    }
    if na <= 0.0 || nb <= 0.0 {
        return None;
    }
    Some((dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0))
}

fn cosine_from_blob(query: &[f32], blob: &[u8]) -> Option<f64> {
    cosine(query, &blob_to_f32(blob)?)
}

/// Token set used for MMR's redundancy measure: lowered whitespace/punctuation split, mirroring
/// the retrieval tokenizer so "what counts as the same note" matches "what counts as a match".
pub(crate) fn token_set(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

/// Maximal Marginal Relevance reranking over token-set Jaccard similarity. `lambda` trades
/// relevance (1.0) against diversity (0.0); each pick maximises
/// `lambda * relevance - (1 - lambda) * max_similarity_to_already_picked`.
fn mmr_rerank(candidates: Vec<ScoredMemory>, lambda: f64, limit: usize) -> Vec<ScoredMemory> {
    if candidates.is_empty() || limit == 0 {
        return Vec::new();
    }
    let lambda = lambda.clamp(0.0, 1.0);
    let tokens: Vec<HashSet<String>> = candidates
        .iter()
        .map(|c| token_set(&format!("{} {}", c.key, c.value)))
        .collect();
    let mut remaining: Vec<usize> = (0..candidates.len()).collect();
    let mut selected: Vec<usize> = Vec::new();

    while !remaining.is_empty() {
        let mut best_pos = 0usize;
        let mut best_val = f64::NEG_INFINITY;
        for (pos, &cand_idx) in remaining.iter().enumerate() {
            let cand = &candidates[cand_idx];
            let redundancy = selected
                .iter()
                .map(|&s| jaccard(&tokens[cand_idx], &tokens[s]))
                .fold(0.0f64, f64::max);
            let val = lambda * cand.score - (1.0 - lambda) * redundancy;
            if val > best_val {
                best_val = val;
                best_pos = pos;
            }
        }
        selected.push(remaining.remove(best_pos));
        if selected.len() >= limit {
            break;
        }
    }
    selected
        .into_iter()
        .map(|i| candidates[i].clone())
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
        -- FTS5 backing for BM25. Best-effort: see fts_or_like_scores fallback.
        CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(id UNINDEXED, key, value, tokenize='porter');
        -- Session -> shared-scope binding for recall expansion (`user:`/`project:` shelves).
        -- Written only by host code via `bind_session_scopes`, never by a memory tool.
        CREATE TABLE IF NOT EXISTS memory_scopes (
            session       TEXT PRIMARY KEY,
            user_scope    TEXT,
            project_scope TEXT
        );
        "#,
    )
    .map_err(|e| RuntimeError::Store(e.to_string()))?;
    // Columns added after the first release. SQLite has no `ADD COLUMN IF NOT EXISTS`, so each
    // one is checked against the live schema instead of being folded into the batch above.
    ensure_column(conn, "memory", "importance", "REAL NOT NULL DEFAULT 0.5")?;
    ensure_column(conn, "memory", "embedding", "BLOB")?;
    // Consolidation lineage: `active` rows are the only ones recall/list/get see;
    // `superseded`/`expired` rows are retained for provenance, mirroring the
    // observation lifecycle in `consolidation.rs`.
    ensure_column(conn, "memory", "status", "TEXT NOT NULL DEFAULT 'active'")?;
    ensure_column(conn, "memory", "superseded_by", "TEXT")?;
    // Backfill the FTS5 index for rows that predate it (idempotent; no-op when already indexed).
    let _ = conn.execute_batch(
        "INSERT INTO memory_fts(id, key, value)
         SELECT m.id, m.key, m.value FROM memory m
         WHERE NOT EXISTS (SELECT 1 FROM memory_fts f WHERE f.id = m.id);",
    );
    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{column}'"
        ))
        .map_err(|e| RuntimeError::Store(e.to_string()))?;
    let exists: i64 = stmt
        .query_row([], |r| r.get(0))
        .map_err(|e| RuntimeError::Store(e.to_string()))?;
    if exists == 0 {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))
        .map_err(|e| RuntimeError::Store(e.to_string()))?;
    }
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
    fn delete_removes_from_recall_and_search() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("deploy", "ship the gateway", "global").unwrap();
        let terms = vec!["gateway".to_string()];
        assert_eq!(
            store
                .recall_terms(&terms, &["global".into()], 8)
                .unwrap()
                .len(),
            1
        );
        assert!(!store.search("gateway", "global").unwrap().is_empty());
        assert!(store.delete("deploy", "global").unwrap());
        assert!(
            store
                .recall_terms(&terms, &["global".into()], 8)
                .unwrap()
                .is_empty()
        );
        assert!(store.search("gateway", "global").unwrap().is_empty());
        // A same-key entry in another scope survives: delete is scope-confined.
        store
            .save("deploy", "other scope note", "session-1")
            .unwrap();
        assert!(!store.delete("deploy", "global").unwrap());
        assert_eq!(
            store.get("deploy", "session-1").unwrap(),
            Some("other scope note".to_string())
        );
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
    fn a_poisoned_lock_recovers_instead_of_panicking() {
        let store = MemoryStore::open_in_memory().unwrap();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.conn.lock().unwrap();
            panic!("poison the memory mutex");
        }));
        assert!(poisoned.is_err());
        store.save("k", "v", "global").unwrap();
        assert_eq!(store.get("k", "global").unwrap(), Some("v".to_string()));
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

    // ---- hybrid retrieval ------------------------------------------------

    use crate::memory::{cosine, jaccard, mmr_rerank, recency_importance_score, token_set};

    fn terms(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bm25_and_overlap_rank_the_best_match_first() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .save(
                "deploy",
                "deploy the payment gateway to production",
                "global",
            )
            .unwrap();
        store
            .save(
                "other",
                "gateway routing rules for weekly updates",
                "global",
            )
            .unwrap();
        store
            .save("unrelated", "lunch menu for next week", "global")
            .unwrap();

        let scored = store
            .hybrid_recall(
                &terms(&["payment", "gateway"]),
                &["global".into()],
                8,
                &HybridOptions::default(),
            )
            .unwrap();
        assert!(!scored.is_empty(), "hybrid recall finds a match");
        assert_eq!(
            scored[0].key, "deploy",
            "highest fused score first: {scored:?}"
        );
        assert!(scored[0].overlap > 0.0, "overlap signal is populated");
        assert!(
            scored.iter().all(|s| s.key != "unrelated"),
            "a zero-overlap entry must not survive on recency alone"
        );
    }

    #[test]
    fn hybrid_recall_stays_inside_granted_scopes_and_empty_grants() {
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
        let t = terms(&["gateway"]);

        let mine = store
            .hybrid_recall(
                &t,
                &["global".into(), "session-1".into()],
                8,
                &HybridOptions::default(),
            )
            .unwrap();
        let scopes: Vec<&str> = mine.iter().map(|s| s.scope.as_str()).collect();
        assert!(scopes.contains(&"global"));
        assert!(scopes.contains(&"session-1"));
        assert!(
            !scopes.contains(&"session-2"),
            "foreign scope never surfaces: {mine:?}"
        );

        assert!(
            store
                .hybrid_recall(&t, &[], 8, &HybridOptions::default())
                .unwrap()
                .is_empty(),
            "no granted scope = no recall, deny-first"
        );
        assert!(
            store
                .hybrid_recall(&[], &["global".into()], 8, &HybridOptions::default())
                .unwrap()
                .is_empty(),
            "no terms = no recall"
        );
        assert!(
            store
                .hybrid_recall(&t, &["global".into()], 0, &HybridOptions::default())
                .unwrap()
                .is_empty(),
            "limit 0 = no recall"
        );
    }

    #[test]
    fn importance_and_recency_shift_the_ranking() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Two identical-content entries; only importance differs. Higher importance wins.
        store
            .save_with_embedding("low", "identical gateway note body", "global", 0.1, None)
            .unwrap();
        store
            .save_with_embedding("high", "identical gateway note body", "global", 0.95, None)
            .unwrap();
        // MMR is disabled for this assertion (lambda=1.0, pure relevance) since the two entries
        // are identical and MMR would demote the duplicate regardless of relevance.
        let opts = HybridOptions {
            mmr_lambda: 1.0,
            ..HybridOptions::default()
        };
        let scored = store
            .hybrid_recall(&terms(&["gateway"]), &["global".into()], 8, &opts)
            .unwrap();
        let pos = |k: &str| scored.iter().position(|s| s.key == k).unwrap();
        assert!(
            pos("high") < pos("low"),
            "the important entry out-ranks the trivial twin: {scored:?}"
        );
        assert!(scored[pos("high")].recency_importance > scored[pos("low")].recency_importance);

        // Recency decay is monotone: newer beats older at equal importance.
        let now = crate::types::now_ms();
        let recent = recency_importance_score(now, 0.5, now, 7.0);
        let week_old = recency_importance_score(now - 7 * 86_400_000, 0.5, now, 7.0);
        assert!(recent > week_old, "recency decay is monotone");
    }

    #[test]
    fn cosine_signal_rewards_a_local_embedding_match() {
        let store = MemoryStore::open_in_memory().unwrap();
        let aligned = vec![1.0f32, 0.0, 0.0, 0.0];
        let orthogonal = vec![0.0f32, 1.0, 0.0, 0.0];
        store
            .save_with_embedding(
                "near",
                "gateway deployment note",
                "global",
                0.5,
                Some(&aligned),
            )
            .unwrap();
        store
            .save_with_embedding(
                "far",
                "gateway deployment note",
                "global",
                0.5,
                Some(&orthogonal),
            )
            .unwrap();
        store
            .save("plain", "gateway deployment note", "global")
            .unwrap();

        let opts = HybridOptions {
            query_embedding: Some(vec![1.0, 0.0, 0.0, 0.0]),
            weights: HybridWeights {
                bm25: 0.0,
                overlap: 0.0,
                recency_importance: 0.0,
                cosine: 1.0,
            },
            mmr_lambda: 1.0,
            ..HybridOptions::default()
        };
        let scored = store
            .hybrid_recall(&terms(&["gateway"]), &["global".into()], 8, &opts)
            .unwrap();
        let get = |k: &str| scored.iter().find(|s| s.key == k).expect("entry present");
        assert_eq!(get("near").cosine, Some(1.0), "aligned vector has cosine 1");
        assert_eq!(
            get("far").cosine,
            Some(0.0),
            "orthogonal vector has cosine 0"
        );
        assert_eq!(get("plain").cosine, None, "no blob = no cosine signal");
        assert_eq!(
            scored[0].key, "near",
            "cosine-only ranking puts the aligned entry first"
        );

        // Without a query vector the cosine component is inert.
        let scored = store
            .hybrid_recall(
                &terms(&["gateway"]),
                &["global".into()],
                8,
                &HybridOptions::default(),
            )
            .unwrap();
        assert!(scored.iter().all(|s| s.cosine.is_none()));

        // Unit-level cosine / blob checks.
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0]), Some(1.0));
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), Some(0.0));
        assert_eq!(
            cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]),
            None,
            "length mismatch = no score"
        );
        assert_eq!(
            cosine(&[0.0, 0.0], &[1.0, 0.0]),
            None,
            "zero vector = no score"
        );
    }

    #[test]
    fn mmr_suppresses_near_duplicates_in_favour_of_distinct_entries() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Three entries; two are near-duplicates of each other. Pure relevance (lambda=1 via
        // weight tweaks) would order by score; MMR must demote the duplicate so the distinct
        // entry surfaces before it.
        store
            .save(
                "dup-a",
                "deploy the payment gateway to production now",
                "global",
            )
            .unwrap();
        store
            .save(
                "dup-b",
                "deploy the payment gateway to production now please",
                "global",
            )
            .unwrap();
        store
            .save(
                "distinct",
                "gateway failover plan for traffic spikes",
                "global",
            )
            .unwrap();

        let diverse = HybridOptions {
            mmr_lambda: 0.0,
            candidate_pool: 8,
            ..HybridOptions::default()
        };
        let out = store
            .hybrid_recall(
                &terms(&["gateway", "deploy", "payment"]),
                &["global".into()],
                3,
                &diverse,
            )
            .unwrap();
        assert_eq!(out.len(), 3);
        // With lambda=0 the order is purely by diversity: the distinct entry must not be last.
        let distinct_pos = out.iter().position(|s| s.key == "distinct").unwrap();
        assert!(
            distinct_pos < 2,
            "the distinct entry surfaces ahead of a near-duplicate: {out:?}"
        );
    }

    #[test]
    fn fts5_failure_falls_back_to_token_overlap_without_panicking() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .save("note", "the payment gateway is live", "global")
            .unwrap();
        {
            // Break FTS5 entirely: drop the virtual table and a junk MATCH path.
            let conn = store.lock();
            conn.execute("DROP TABLE IF EXISTS memory_fts", []).unwrap();
        }
        // Recall must still find the entry through the LIKE token-overlap fallback.
        let scored = store
            .hybrid_recall(
                &terms(&["payment", "gateway"]),
                &["global".into()],
                8,
                &HybridOptions::default(),
            )
            .unwrap();
        assert_eq!(scored.len(), 1, "fallback still recalls: {scored:?}");
        assert_eq!(scored[0].key, "note");
        assert!(scored[0].overlap > 0.0);
    }

    #[test]
    fn token_set_jaccard_and_mmr_units_behave() {
        let a = token_set("Deploy the Payment Gateway!");
        let b = token_set("deploy payment gateway");
        assert!(
            jaccard(&a, &b) > 0.4,
            "near-duplicates score high: {a:?} vs {b:?}"
        );
        let c = token_set("unrelated lunch menu");
        assert_eq!(jaccard(&a, &c), 0.0);
        assert_eq!(jaccard(&token_set(""), &token_set("")), 1.0);

        let mk = |key: &str, value: &str, score: f64| ScoredMemory {
            scope: "global".into(),
            key: key.into(),
            value: value.into(),
            updated_ms: 0,
            score,
            bm25: 0.0,
            overlap: score,
            recency_importance: 0.0,
            cosine: None,
        };
        // lambda=1 must preserve pure relevance order (identity).
        let pool = vec![
            mk("a", "x y", 0.9),
            mk("b", "x z", 0.5),
            mk("c", "p q", 0.7),
        ];
        let out = mmr_rerank(pool.clone(), 1.0, 3);
        assert_eq!(
            out.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
            vec!["a", "c", "b"]
        );
        // Empty / zero limit are safe.
        assert!(mmr_rerank(vec![], 0.7, 3).is_empty());
        assert!(mmr_rerank(pool, 0.7, 0).is_empty());
    }

    #[test]
    fn saving_twice_keeps_one_fts_row_and_refreshes_it() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("k", "first gateway version", "global").unwrap();
        store.save("k", "second payment version", "global").unwrap();
        // The stale first value must not be findable through FTS; the current one must.
        assert!(
            store
                .hybrid_recall(
                    &terms(&["payment"]),
                    &["global".into()],
                    8,
                    &HybridOptions::default()
                )
                .unwrap()
                .iter()
                .any(|s| s.key == "k" && s.value.contains("payment"))
        );
        assert!(
            store
                .hybrid_recall(
                    &terms(&["first"]),
                    &["global".into()],
                    8,
                    &HybridOptions::default()
                )
                .unwrap()
                .is_empty(),
            "the replaced value leaves the index"
        );
    }

    // ---- shared user:/project: scopes ------------------------------------

    #[test]
    fn shared_scope_names_are_prefixed_and_sanitized() {
        assert_eq!(user_scope("user-1").as_deref(), Some("user:user-1"));
        assert_eq!(
            project_scope("proj-abc").as_deref(),
            Some("project:proj-abc")
        );
        assert_eq!(
            project_scope("/tmp/my proj").as_deref(),
            Some("project:_tmp_my_proj")
        );
        assert_eq!(user_scope("   "), None, "no segment, no scope");
        assert_eq!(project_scope(""), None);
        assert_eq!(hive_scope("hive-1").as_deref(), Some("hive:hive-1"));
        assert_eq!(hive_scope("forge team").as_deref(), Some("hive:forge_team"));
        assert_eq!(hive_scope(""), None);
    }

    #[test]
    fn a_hive_shelf_recalls_only_when_the_run_names_it() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .save("runbook", "rotate staging credentials", "hive:h1")
            .unwrap();
        store
            .save("runbook", "rotate staging credentials too", "hive:h2")
            .unwrap();
        store.save("note", "session only note", "ses-a").unwrap();
        let terms = vec!["rotate".to_string(), "credentials".to_string()];

        // A session-bound recall never names the hive shelf: deny-first.
        let session = store
            .recall_terms(&terms, &["global".into(), "ses-a".into()], 8)
            .unwrap();
        assert!(
            session.iter().all(|(s, _, _)| !s.starts_with("hive:")),
            "an ordinary session must not see hive shelves: {session:?}"
        );

        // The hive run names its own shelf explicitly, and only its own.
        let hive = store
            .recall_terms(&terms, &["global".into(), "hive:h1".into()], 8)
            .unwrap();
        let scopes: Vec<&str> = hive.iter().map(|(s, _, _)| s.as_str()).collect();
        assert!(scopes.contains(&"hive:h1"), "hive h1 recalls: {hive:?}");
        assert!(
            !scopes.contains(&"hive:h2"),
            "another hive's shelf must not leak: {hive:?}"
        );
    }

    #[test]
    fn a_session_reports_the_shelves_it_was_bound_to() {
        let store = MemoryStore::open_in_memory().unwrap();
        assert_eq!(store.session_scopes("ses-a").unwrap(), (None, None));
        assert_eq!(store.session_scopes("  ").unwrap(), (None, None));
        store
            .bind_session_scopes("ses-a", Some("user:u1"), Some("project:p1"))
            .unwrap();
        assert_eq!(
            store.session_scopes("ses-a").unwrap(),
            (Some("user:u1".to_owned()), Some("project:p1".to_owned()))
        );
        store.bind_session_scopes("ses-a", None, None).unwrap();
        assert_eq!(
            store.session_scopes("ses-a").unwrap(),
            (None, None),
            "a cleared binding reports nothing rather than a stale shelf"
        );
    }

    #[test]
    fn a_bound_session_recalls_its_user_and_project_shelves() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .save("pref", "prefers rustfmt nightly", "user:u1")
            .unwrap();
        store
            .save("conv", "uses tabs in this project", "project:p1")
            .unwrap();
        store.save("note", "session only note", "ses-a").unwrap();
        store
            .bind_session_scopes("ses-a", Some("user:u1"), Some("project:p1"))
            .unwrap();

        let hits = store
            .recall_terms(
                &["rustfmt".to_string(), "tabs".to_string()],
                &["global".to_owned(), "ses-a".to_owned()],
                8,
            )
            .unwrap();
        let scopes: Vec<&str> = hits.iter().map(|(s, _, _)| s.as_str()).collect();
        assert!(
            scopes.contains(&"user:u1"),
            "the bound user shelf recalls: {hits:?}"
        );
        assert!(
            scopes.contains(&"project:p1"),
            "the bound project shelf recalls: {hits:?}"
        );
    }

    #[test]
    fn unbound_and_other_project_sessions_do_not_see_shared_shelves() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .save("pref", "prefers rustfmt nightly", "user:u1")
            .unwrap();
        store
            .save("conv", "uses tabs in this project", "project:p1")
            .unwrap();
        store
            .save("conv", "spaces in the other project", "project:p2")
            .unwrap();
        store
            .bind_session_scopes("ses-a", Some("user:u1"), Some("project:p1"))
            .unwrap();
        store
            .bind_session_scopes("ses-b", Some("user:u2"), Some("project:p2"))
            .unwrap();

        let terms = vec![
            "prefers".to_string(),
            "spaces".to_string(),
            "tabs".to_string(),
        ];
        let a = store
            .recall_terms(&terms, &["global".into(), "ses-a".into()], 8)
            .unwrap();
        let a_scopes: Vec<&str> = a.iter().map(|(s, _, _)| s.as_str()).collect();
        assert!(a_scopes.contains(&"project:p1"));
        assert!(
            !a_scopes.contains(&"project:p2"),
            "project p2's shelf must not leak into p1's session: {a:?}"
        );
        assert!(
            !a_scopes.contains(&"user:u2"),
            "another user's shelf must not leak: {a:?}"
        );

        // A session with no binding recalls nothing shared — deny-first.
        let c = store
            .recall_terms(&terms, &["global".into(), "ses-c".into()], 8)
            .unwrap();
        assert!(
            c.iter().all(|(s, _, _)| s == "global" || s == "ses-c"),
            "unbound session sees only its granted scopes: {c:?}"
        );

        // And granting only `global` never surfaces shared shelves either.
        let g = store.recall_terms(&terms, &["global".into()], 8).unwrap();
        assert!(g.iter().all(|(s, _, _)| s == "global"));
    }

    #[test]
    fn clearing_a_binding_stops_shared_recall() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .save("conv", "uses tabs in this project", "project:p1")
            .unwrap();
        store
            .bind_session_scopes("ses-a", None, Some("project:p1"))
            .unwrap();
        let terms = vec!["tabs".to_string()];
        let scopes = ["global".to_owned(), "ses-a".to_owned()];
        assert!(
            store
                .recall_terms(&terms, &scopes, 8)
                .unwrap()
                .iter()
                .any(|(s, _, _)| s == "project:p1")
        );

        // Leaving the project clears the shelf: the next recall must not see it.
        store.bind_session_scopes("ses-a", None, None).unwrap();
        let hits = store.recall_terms(&terms, &scopes, 8).unwrap();
        assert!(
            hits.iter().all(|(s, _, _)| s != "project:p1"),
            "a cleared binding recalls nothing shared: {hits:?}"
        );
    }

    // ---- memory-table consolidation --------------------------------------

    /// Insert a row bypassing `save` so tests can build the duplicate
    /// `(scope, key)` groups that only legacy/foreign writers produce — `save`
    /// itself upserts by the deterministic `{scope}\u{1f}{key}` id.
    fn insert_raw_memory(
        store: &MemoryStore,
        id: &str,
        scope: &str,
        key: &str,
        value: &str,
        importance: f64,
        updated_ms: i64,
    ) {
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO memory (id, key, value, scope, created_ms, updated_ms, importance, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, 'active')",
            rusqlite::params![id, key, value, scope, updated_ms, importance],
        )
        .unwrap();
        let _ = conn.execute(
            "INSERT INTO memory_fts(id, key, value) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, key, value],
        );
    }

    fn status_of(store: &MemoryStore, id: &str) -> Option<String> {
        store
            .lock()
            .query_row(
                "SELECT status FROM memory WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    }

    #[test]
    fn consolidate_collapses_duplicate_rows_for_one_key() {
        let store = MemoryStore::open_in_memory().unwrap();
        let now = now_ms() as i64;
        insert_raw_memory(
            &store,
            "global\u{1f}dup",
            "global",
            "dup",
            "Same Value",
            0.4,
            now - 1000,
        );
        insert_raw_memory(
            &store,
            "global\u{1f}dup#legacy",
            "global",
            "dup",
            "  same   value ",
            0.9,
            now,
        );

        let report = store.consolidate().unwrap();
        assert_eq!(report.duplicates_removed, 1);
        assert_eq!(report.contradictions, 0);
        assert_eq!(store.count("global").unwrap(), 1);
        // The higher-importance row wins and keeps its raw (un-normalized) value.
        assert_eq!(
            store.get("dup", "global").unwrap(),
            Some("  same   value ".to_string())
        );
        assert_eq!(status_of(&store, "global\u{1f}dup"), None, "loser deleted");
        assert_eq!(
            status_of(&store, "global\u{1f}dup#legacy"),
            Some("active".to_string())
        );
    }

    #[test]
    fn consolidate_supersedes_the_older_side_of_a_contradiction() {
        let store = MemoryStore::open_in_memory().unwrap();
        let now = now_ms() as i64;
        insert_raw_memory(
            &store,
            "global\u{1f}conflict#a",
            "global",
            "conflict",
            "editor uses tabs",
            0.3,
            now - 5000,
        );
        insert_raw_memory(
            &store,
            "global\u{1f}conflict#b",
            "global",
            "conflict",
            "editor uses spaces",
            0.8,
            now,
        );

        let report = store.consolidate().unwrap();
        assert_eq!(report.contradictions, 1);
        assert_eq!(report.superseded, 1);
        assert_eq!(report.duplicates_removed, 0);

        // The important/newer value is what recall and get now see.
        assert_eq!(
            store.get("conflict", "global").unwrap(),
            Some("editor uses spaces".to_string())
        );
        assert_eq!(
            status_of(&store, "global\u{1f}conflict#a"),
            Some("superseded".to_string())
        );
        let superseded_by: Option<String> = store
            .lock()
            .query_row(
                "SELECT superseded_by FROM memory WHERE id = ?1",
                rusqlite::params!["global\u{1f}conflict#a"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            superseded_by.as_deref(),
            Some("global\u{1f}conflict#b"),
            "honest lineage pointer to the winner"
        );
        // The loser is retained for provenance, not deleted.
        assert_eq!(store.total_count().unwrap(), 1);
        let raw_rows: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM memory", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw_rows, 2, "superseded row stays in the table");
    }

    #[test]
    fn consolidate_never_merges_across_scopes() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.save("k", "global value", "global").unwrap();
        store.save("k", "session value", "session-1").unwrap();

        let report = store.consolidate().unwrap();
        assert_eq!(report.contradictions, 0);
        assert_eq!(report.superseded, 0);
        assert_eq!(report.duplicates_removed, 0);
        assert_eq!(
            store.get("k", "global").unwrap(),
            Some("global value".to_string())
        );
        assert_eq!(
            store.get("k", "session-1").unwrap(),
            Some("session value".to_string())
        );
    }

    #[test]
    fn consolidate_is_idempotent() {
        let store = MemoryStore::open_in_memory().unwrap();
        let now = now_ms() as i64;
        insert_raw_memory(&store, "s\u{1f}k#1", "s", "k", "alpha", 0.5, now);
        insert_raw_memory(&store, "s\u{1f}k#2", "s", "k", "beta", 0.5, now);
        insert_raw_memory(&store, "s\u{1f}k#3", "s", "k", "beta ", 0.5, now);

        let first = store.consolidate().unwrap();
        assert!(first.contradictions >= 1 || first.duplicates_removed >= 1);
        let second = store.consolidate().unwrap();
        assert_eq!(second.duplicates_removed, 0);
        assert_eq!(second.contradictions, 0);
        assert_eq!(second.superseded, 0);
        assert_eq!(second.expired, 0);
    }

    #[test]
    fn recall_reflects_the_post_consolidation_state() {
        let store = MemoryStore::open_in_memory().unwrap();
        let now = now_ms() as i64;
        insert_raw_memory(
            &store,
            "g\u{1f}pref#a",
            "g",
            "pref",
            "uses tabs for indentation",
            0.9,
            now,
        );
        insert_raw_memory(
            &store,
            "g\u{1f}pref#b",
            "g",
            "pref",
            "uses spaces for indentation",
            0.2,
            now - 10_000,
        );
        store.consolidate().unwrap();

        let tabs = store
            .recall_terms(&["tabs".to_string()], &["g".to_string()], 8)
            .unwrap();
        assert_eq!(tabs.len(), 1, "only the surviving value recalls");
        assert_eq!(tabs[0].2, "uses tabs for indentation");
        let spaces = store
            .recall_terms(&["spaces".to_string()], &["g".to_string()], 8)
            .unwrap();
        assert!(
            spaces.is_empty(),
            "the superseded value must not surface through recall: {spaces:?}"
        );
    }

    #[test]
    fn consolidate_expires_old_low_importance_rows() {
        let store = MemoryStore::open_in_memory().unwrap();
        let now = now_ms() as i64;
        let ancient = now - (MEMORY_MAX_AGE_MS as i64) - 3_600_000;
        insert_raw_memory(
            &store,
            "old\u{1f}trivial",
            "old",
            "trivial",
            "meh",
            0.0,
            ancient,
        );
        insert_raw_memory(
            &store,
            "old\u{1f}key",
            "old",
            "key",
            "keep me",
            0.9,
            ancient,
        );

        let report = store.consolidate().unwrap();
        assert_eq!(report.expired, 1);
        assert_eq!(store.get("trivial", "old").unwrap(), None);
        assert_eq!(
            store.get("key", "old").unwrap(),
            Some("keep me".to_string()),
            "high importance survives aging"
        );
        assert_eq!(
            status_of(&store, "old\u{1f}trivial"),
            Some("expired".to_string())
        );
    }

    #[test]
    fn resaving_a_superseded_key_revives_it() {
        let store = MemoryStore::open_in_memory().unwrap();
        let now = now_ms() as i64;
        insert_raw_memory(
            &store,
            "g\u{1f}k#old",
            "g",
            "k",
            "stale value",
            0.1,
            now - 1000,
        );
        insert_raw_memory(&store, "g\u{1f}k#new", "g", "k", "fresh value", 0.9, now);
        store.consolidate().unwrap();
        assert_eq!(
            store.get("k", "g").unwrap(),
            Some("fresh value".to_string())
        );

        // A later save updates whichever row the (key, scope) lookup finds and
        // flips it back to active — consolidation never bricks a key.
        store.save("k", "revised value", "g").unwrap();
        let active: i64 = store
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM memory WHERE scope = 'g' AND key = 'k' AND status = 'active'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(active, 1);
        assert_eq!(
            store.get("k", "g").unwrap(),
            Some("revised value".to_string())
        );
    }
}
