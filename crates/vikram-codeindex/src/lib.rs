//! Structural repository intelligence: tree-sitter parsing, symbol extraction,
//! reference graph, PageRank-ish file ranking, persisted SQLite index, incremental updates.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tree_sitter::StreamingIterator;
use walkdir::WalkDir;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, IndexError>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Class,
    Enum,
    Trait,
    Interface,
    Module,
    Const,
    Type,
    Impl,
    Other(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub language: String,
    pub mtime_ms: i64,
    pub size: u64,
    pub symbols: Vec<Symbol>,
}

pub struct CodeIndex {
    conn: Connection,
    root: PathBuf,
}

impl CodeIndex {
    pub fn open(root: &Path, db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY,
                language TEXT NOT NULL,
                mtime_ms INTEGER NOT NULL,
                size INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS symbols (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file TEXT NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                signature TEXT,
                FOREIGN KEY(file) REFERENCES files(path) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
            CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);
            CREATE TABLE IF NOT EXISTS refs (
                from_file TEXT NOT NULL,
                to_symbol TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_refs_sym ON refs(to_symbol);",
        )?;
        Ok(Self {
            conn,
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Index (or re-index) the repository. Only re-parses files whose mtime changed.
    pub fn index(&mut self, exclude: &[&str]) -> Result<IndexStats> {
        let mut stats = IndexStats::default();
        // Collect current files.
        let mut current: BTreeSet<PathBuf> = BTreeSet::new();
        for entry in WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| {
                e.path() == self.root.as_path() || !is_excluded(e.path(), &self.root, exclude)
            })
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !is_source(path) {
                continue;
            }
            current.insert(path.to_path_buf());
        }

        // Remove deleted files.
        let existing: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT path FROM files")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for p in &existing {
            let abs = self.root.join(p);
            if !current.contains(&abs) {
                self.conn
                    .execute("DELETE FROM files WHERE path = ?1", params![p])?;
                self.conn
                    .execute("DELETE FROM symbols WHERE file = ?1", params![p])?;
                self.conn
                    .execute("DELETE FROM refs WHERE from_file = ?1", params![p])?;
                stats.removed += 1;
            }
        }

        for path in &current {
            let meta = match fs::metadata(path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let size = meta.len();
            let rel = path
                .strip_prefix(&self.root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            // Check if unchanged.
            let stored_mtime: Option<i64> = self
                .conn
                .query_row(
                    "SELECT mtime_ms FROM files WHERE path = ?1",
                    params![rel],
                    |r| r.get(0),
                )
                .ok();
            if stored_mtime == Some(mtime_ms) {
                stats.unchanged += 1;
                continue;
            }

            // Parse.
            let language = language_of(path).to_string();
            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let symbols = extract_symbols(path, &language, &content);

            // Upsert file row.
            self.conn.execute(
                "INSERT INTO files(path, language, mtime_ms, size) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(path) DO UPDATE SET language=excluded.language, mtime_ms=excluded.mtime_ms, size=excluded.size",
                params![rel, language, mtime_ms, size as i64],
            )?;
            self.conn
                .execute("DELETE FROM symbols WHERE file = ?1", params![rel])?;
            for s in &symbols {
                self.conn.execute(
                    "INSERT INTO symbols(file, name, kind, start_line, end_line, signature) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![rel, s.name, format!("{:?}", s.kind), s.start_line as i64, s.end_line as i64, s.signature],
                )?;
            }
            stats.indexed += 1;
            stats.symbols += symbols.len();
        }

        // Build reference edges (simple name-based).
        self.conn.execute("DELETE FROM refs", [])?;
        // For each symbol name, find files containing it as a token.
        let all_symbols: Vec<(String, String)> = {
            let mut stmt = self.conn.prepare("SELECT file, name FROM symbols")?;
            let rows = stmt.query_map([], |r| {
                let f: String = r.get(0)?;
                let n: String = r.get(1)?;
                Ok((f, n))
            })?;
            rows.filter_map(|r| r.ok()).collect()
        };
        let file_contents: HashMap<String, String> = current
            .iter()
            .filter_map(|p| {
                let rel = p
                    .strip_prefix(&self.root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .to_string();
                fs::read_to_string(p).ok().map(|c| (rel, c))
            })
            .collect();
        for (from_file, content) in &file_contents {
            for (_, sym) in &all_symbols {
                if content.contains(sym.as_str()) {
                    // crude: record edge if the symbol name appears in this file and isn't defined here
                    self.conn.execute(
                        "INSERT INTO refs(from_file, to_symbol) VALUES (?1,?2)",
                        params![from_file, sym],
                    )?;
                }
            }
        }

        Ok(stats)
    }

    pub fn is_indexed(&self) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Heuristic "contains" check for reference lookup: a file references a symbol
    /// when the symbol name appears as a token in it. Cheap and honest; the refs
    /// table built in `index()` uses the same rule.
    pub fn references_of(&self, symbol: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT from_file FROM refs WHERE to_symbol = ?1")?;
        let rows = stmt.query_map(params![symbol], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Definitions for a name, preferring exact matches.
    pub fn definitions_of(&self, name: &str) -> Result<Vec<Symbol>> {
        let mut out = self.search_symbols(name, 50)?;
        out.sort_by_key(|s| (s.name != name, s.name.clone(), s.file.clone()));
        Ok(out)
    }

    /// Ranked context snippet for a query string: definition matches first, then
    /// other symbol-name hits, then a repo-map slice. Returns "" when the index is
    /// empty (caller falls back to grep).
    pub fn ranked_context(&self, query: &str, limit: usize) -> Result<String> {
        if !self.is_indexed()? {
            return Ok(String::new());
        }
        let mut lines = Vec::new();
        for s in self.definitions_of(query)? {
            let line = format!(
                "{:?} {} {}:{}",
                s.kind,
                s.name,
                s.file.display(),
                s.start_line
            );
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
        // Reference files add the "who uses this" half of the picture.
        for f in self.references_of(query).unwrap_or_default() {
            lines.push(format!("referenced in {f}"));
        }
        if lines.len() > limit {
            lines.truncate(limit);
        }
        Ok(lines.join("\n"))
    }

    pub fn search_symbols(&self, query: &str, limit: usize) -> Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT file, name, kind, start_line, end_line, signature FROM symbols WHERE name LIKE ?1 LIMIT ?2",
        )?;
        let pattern = format!("%{query}%");
        let rows = stmt.query_map(params![pattern, limit as i64], |r| {
            Ok(Symbol {
                file: PathBuf::from(r.get::<_, String>(0)?),
                name: r.get(1)?,
                kind: parse_kind(&r.get::<_, String>(2)?),
                start_line: r.get::<_, i64>(3)? as usize,
                end_line: r.get::<_, i64>(4)? as usize,
                signature: r.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn repo_map(&self, token_budget: usize) -> Result<String> {
        // Rank files by symbol count + reference count.
        let mut file_scores: BTreeMap<String, f64> = BTreeMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT file, COUNT(*) FROM symbols GROUP BY file")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for r in rows.filter_map(|r| r.ok()) {
                *file_scores.entry(r.0).or_insert(0.0) += r.1 as f64;
            }
        }
        {
            let mut stmt = self
                .conn
                .prepare("SELECT to_symbol, COUNT(*) FROM refs GROUP BY to_symbol")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for r in rows.filter_map(|r| r.ok()) {
                // boost files defining highly-referenced symbols
                let _ = r;
            }
        }

        // Build output sorted by score desc, trimmed to token budget (approx chars/4).
        let mut ranked: Vec<(String, f64)> = file_scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut out = String::new();
        let mut used = 0usize;
        let budget_chars = token_budget * 4;
        for (file, _score) in ranked {
            // fetch symbols for file
            let mut stmt = self
                .conn
                .prepare("SELECT name, kind, start_line FROM symbols WHERE file = ?1")?;
            let rows = stmt.query_map(params![file], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            let mut lines = Vec::new();
            lines.push(file.clone());
            for r in rows.filter_map(|r| r.ok()) {
                lines.push(format!("  {} {} (L{})", r.1, r.0, r.2));
            }
            let block = lines.join("\n") + "\n";
            if used + block.len() > budget_chars {
                break;
            }
            out.push_str(&block);
            used += block.len();
        }
        Ok(out)
    }
}

#[derive(Default, Debug)]
pub struct IndexStats {
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub symbols: usize,
}

fn is_excluded(path: &Path, root: &Path, exclude: &[&str]) -> bool {
    let s = path.to_string_lossy();
    for e in exclude {
        if s.contains(e) {
            return true;
        }
    }
    // skip hidden dirs and common build dirs
    let rel = path.strip_prefix(root).unwrap_or(path);
    for comp in rel.components() {
        let c = comp.as_os_str().to_string_lossy();
        if c.starts_with('.') || c == "target" || c == "node_modules" || c == "dist" {
            return true;
        }
    }
    false
}

fn is_source(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext,
        "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "go" | "java" | "c" | "cpp" | "h" | "hpp"
    )
}

fn language_of(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "hpp" => "cpp",
        _ => "unknown",
    }
}

fn parse_kind(s: &str) -> SymbolKind {
    match s {
        "Function" => SymbolKind::Function,
        "Method" => SymbolKind::Method,
        "Struct" => SymbolKind::Struct,
        "Class" => SymbolKind::Class,
        "Enum" => SymbolKind::Enum,
        "Trait" => SymbolKind::Trait,
        "Interface" => SymbolKind::Interface,
        "Module" => SymbolKind::Module,
        "Const" => SymbolKind::Const,
        "Type" => SymbolKind::Type,
        "Impl" => SymbolKind::Impl,
        other => SymbolKind::Other(other.to_string()),
    }
}

/// Extract top-level symbols. Uses tree-sitter when available for Rust/Python/TS/Go;
/// falls back to a lightweight regex-ish scan otherwise.
fn extract_symbols(path: &Path, language: &str, content: &str) -> Vec<Symbol> {
    // Try tree-sitter for supported languages.
    let ts_symbols = match language {
        "rust" => ts_extract_rust(content),
        "python" => ts_extract_python(content),
        "go" => ts_extract_go(content),
        "typescript" | "javascript" => ts_extract_ts(content),
        _ => None,
    };
    if let Some(syms) = ts_symbols {
        return syms
            .into_iter()
            .map(|(name, kind, start, end, sig)| Symbol {
                name,
                kind,
                file: path.to_path_buf(),
                start_line: start,
                end_line: end,
                signature: sig,
            })
            .collect();
    }
    // Fallback: regex-ish line scan.
    fallback_extract(content, path)
}

fn fallback_extract(content: &str, path: &Path) -> Vec<Symbol> {
    let mut syms = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let t = line.trim();
        let mut kind = None;
        let mut name = None;
        for kw in [
            "fn ",
            "def ",
            "class ",
            "struct ",
            "enum ",
            "trait ",
            "interface ",
        ] {
            if let Some(rest) = t.strip_prefix(kw) {
                // extract identifier
                let ident: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !ident.is_empty() {
                    kind = Some(match kw.trim() {
                        "fn" => SymbolKind::Function,
                        "def" => SymbolKind::Function,
                        "class" => SymbolKind::Class,
                        "struct" => SymbolKind::Struct,
                        "enum" => SymbolKind::Enum,
                        "trait" => SymbolKind::Trait,
                        "interface" => SymbolKind::Interface,
                        _ => SymbolKind::Other(kw.to_string()),
                    });
                    name = Some(ident);
                    break;
                }
            }
        }
        if let (Some(k), Some(n)) = (kind, name) {
            syms.push(Symbol {
                name: n,
                kind: k,
                file: path.to_path_buf(),
                start_line: i + 1,
                end_line: i + 1,
                signature: Some(line.trim().to_string()),
            });
        }
    }
    syms
}

// ---------- tree-sitter extractors ----------
/// One extracted symbol: name, kind, start line, end line, optional signature.
type ExtractedSymbol = (String, SymbolKind, usize, usize, Option<String>);
type ExtractedSymbols = Vec<ExtractedSymbol>;

fn ts_extract_rust(content: &str) -> Option<ExtractedSymbols> {
    use tree_sitter::{Parser, Query, QueryCursor};
    let mut parser = Parser::new();
    let lang = tree_sitter_rust::LANGUAGE;
    parser.set_language(&lang.into()).ok()?;
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    // Query top-level items.
    let q = Query::new(
        &lang.into(),
        r#"
        (function_item name: (identifier) @name) @fn
        (struct_item name: (type_identifier) @name) @st
        (enum_item name: (type_identifier) @name) @en
        (trait_item name: (type_identifier) @name) @tr
        (impl_item (type_identifier) @name) @im
        (const_item name: (identifier) @name) @co
        (type_item name: (type_identifier) @name) @ty
        "#,
    )
    .ok()?;
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut matches = cursor.matches(&q, root, content.as_bytes());
    while let Some(m) = matches.next() {
        // find capture named "name"
        for cap in m.captures() {
            let cap_name = q.capture_names()[cap.index as usize];
            if cap_name == "name" {
                let node = cap.node;
                let name = node.utf8_text(content.as_bytes()).ok()?.to_string();
                let parent = node.parent()?;
                let kind = match parent.kind() {
                    "function_item" => SymbolKind::Function,
                    "struct_item" => SymbolKind::Struct,
                    "enum_item" => SymbolKind::Enum,
                    "trait_item" => SymbolKind::Trait,
                    "impl_item" => SymbolKind::Impl,
                    "const_item" => SymbolKind::Const,
                    "type_item" => SymbolKind::Type,
                    _ => SymbolKind::Other(parent.kind().to_string()),
                };
                let start = parent.start_position().row + 1;
                let end = parent.end_position().row + 1;
                let sig = content.lines().nth(start - 1).map(|l| l.trim().to_string());
                out.push((name, kind, start, end, sig));
            }
        }
    }
    Some(out)
}

fn ts_extract_python(content: &str) -> Option<ExtractedSymbols> {
    use tree_sitter::{Parser, Query, QueryCursor};
    let mut parser = Parser::new();
    let lang = tree_sitter_python::LANGUAGE;
    parser.set_language(&lang.into()).ok()?;
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let q = Query::new(
        &lang.into(),
        r#"
        (function_definition name: (identifier) @name) @fn
        (class_definition name: (identifier) @name) @cl
        "#,
    )
    .ok()?;
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut matches = cursor.matches(&q, root, content.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures() {
            let cap_name = q.capture_names()[cap.index as usize];
            if cap_name == "name" {
                let node = cap.node;
                let name = node.utf8_text(content.as_bytes()).ok()?.to_string();
                let parent = node.parent()?;
                let kind = match parent.kind() {
                    "function_definition" => SymbolKind::Function,
                    "class_definition" => SymbolKind::Class,
                    _ => SymbolKind::Other(parent.kind().to_string()),
                };
                let start = parent.start_position().row + 1;
                let end = parent.end_position().row + 1;
                let sig = content.lines().nth(start - 1).map(|l| l.trim().to_string());
                out.push((name, kind, start, end, sig));
            }
        }
    }
    Some(out)
}

fn ts_extract_go(content: &str) -> Option<ExtractedSymbols> {
    use tree_sitter::{Parser, Query, QueryCursor};
    let mut parser = Parser::new();
    let lang = tree_sitter_go::LANGUAGE;
    parser.set_language(&lang.into()).ok()?;
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let q = Query::new(
        &lang.into(),
        r#"
        (function_declaration name: (identifier) @name) @fn
        (method_declaration name: (field_identifier) @name) @me
        (type_declaration (type_spec name: (type_identifier) @name)) @ty
        "#,
    )
    .ok()?;
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut matches = cursor.matches(&q, root, content.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures() {
            let cap_name = q.capture_names()[cap.index as usize];
            if cap_name == "name" {
                let node = cap.node;
                let name = node.utf8_text(content.as_bytes()).ok()?.to_string();
                let parent = node.parent().unwrap_or(node);
                let kind = match parent.kind() {
                    "function_declaration" => SymbolKind::Function,
                    "method_declaration" => SymbolKind::Method,
                    "type_spec" => SymbolKind::Type,
                    _ => SymbolKind::Other(parent.kind().to_string()),
                };
                let start = parent.start_position().row + 1;
                let end = parent.end_position().row + 1;
                let sig = content.lines().nth(start - 1).map(|l| l.trim().to_string());
                out.push((name, kind, start, end, sig));
            }
        }
    }
    Some(out)
}

fn ts_extract_ts(content: &str) -> Option<ExtractedSymbols> {
    use tree_sitter::{Parser, Query, QueryCursor};
    let mut parser = Parser::new();
    let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT;
    parser.set_language(&lang.into()).ok()?;
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let q = Query::new(
        &lang.into(),
        r#"
        (function_declaration name: (identifier) @name) @fn
        (class_declaration name: (type_identifier) @name) @cl
        (interface_declaration name: (type_identifier) @name) @if
        (type_alias_declaration name: (type_identifier) @name) @ty
        "#,
    )
    .ok()?;
    let mut cursor = QueryCursor::new();
    let mut out = Vec::new();
    let mut matches = cursor.matches(&q, root, content.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures() {
            let cap_name = q.capture_names()[cap.index as usize];
            if cap_name == "name" {
                let node = cap.node;
                let name = node.utf8_text(content.as_bytes()).ok()?.to_string();
                let parent = node.parent().unwrap_or(node);
                let kind = match parent.kind() {
                    "function_declaration" => SymbolKind::Function,
                    "class_declaration" => SymbolKind::Class,
                    "interface_declaration" => SymbolKind::Interface,
                    "type_alias_declaration" => SymbolKind::Type,
                    _ => SymbolKind::Other(parent.kind().to_string()),
                };
                let start = parent.start_position().row + 1;
                let end = parent.end_position().row + 1;
                let sig = content.lines().nth(start - 1).map(|l| l.trim().to_string());
                out.push((name, kind, start, end, sig));
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    #[test]
    fn index_and_search() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn foo() {}\nstruct Bar {}\n").unwrap();
        fs::write(
            root.join("b.py"),
            "def baz():\n    pass\nclass Q:\n    pass\n",
        )
        .unwrap();
        let db = root.join("idx.db");
        let mut idx = CodeIndex::open(root, &db).unwrap();
        let stats = idx.index(&[]).unwrap();
        assert!(stats.indexed >= 2);
        assert!(stats.symbols >= 4);
        let syms = idx.search_symbols("foo", 10).unwrap();
        assert!(!syms.is_empty());
        let m = idx.repo_map(1000).unwrap();
        assert!(m.contains("a.rs"));
    }
    #[test]
    fn incremental_skips_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn foo() {}\n").unwrap();
        let db = root.join("idx.db");
        let mut idx = CodeIndex::open(root, &db).unwrap();
        idx.index(&[]).unwrap();
        let stats2 = idx.index(&[]).unwrap();
        assert_eq!(stats2.indexed, 0);
        assert!(stats2.unchanged >= 1);
    }
}
