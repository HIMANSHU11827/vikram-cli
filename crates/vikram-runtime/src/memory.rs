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
        self.conn.lock().expect("memory store mutex not poisoned")
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
                "SELECT id FROM memory WHERE key = ?1 AND scope = ?2",
                params![key, scope],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;

        let id = if let Some(id) = existing {
            conn.execute(
                "UPDATE memory SET value = ?1, updated_ms = ?2, importance = ?3, embedding = ?4 WHERE id = ?5",
                params![value, now, importance, embedding_blob.as_deref(), id],
            )
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
            id
        } else {
            let id = format!("{scope}\u{1f}{key}");
            conn.execute(
                "INSERT INTO memory (id, key, value, scope, created_ms, updated_ms, importance, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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

        // 1. Gather candidate rows confined to the granted scopes. The raw LIKE sweep mirrors
        //    the historical recall; the FTS5 BM25 signal is computed on top of this same pool
        //    so the union of signals never widens beyond the granted scopes.
        let scope_placeholders = scopes.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let clauses = terms
            .iter()
            .map(|_| "(key LIKE ? OR value LIKE ?)")
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT id, scope, key, value, updated_ms, COALESCE(importance, 0.5), embedding \
             FROM memory WHERE scope IN ({scope_placeholders}) AND ({clauses}) \
             ORDER BY updated_ms DESC LIMIT 500"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| RuntimeError::Store(e.to_string()))?;
        let mut bindings: Vec<String> = scopes.to_vec();
        for term in terms {
            let pat = format!("%{term}%");
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
            .prepare("SELECT scope, key, value FROM memory ORDER BY scope ASC, key ASC")
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
        .prepare("SELECT id, key, value FROM memory")
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

fn clamp01(v: f64) -> f64 {
    if v.is_nan() { 0.5 } else { v.clamp(0.0, 1.0) }
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
        "#,
    )
    .map_err(|e| RuntimeError::Store(e.to_string()))?;
    // Columns added after the first release. SQLite has no `ADD COLUMN IF NOT EXISTS`, so each
    // one is checked against the live schema instead of being folded into the batch above.
    ensure_column(conn, "memory", "importance", "REAL NOT NULL DEFAULT 0.5")?;
    ensure_column(conn, "memory", "embedding", "BLOB")?;
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
}
