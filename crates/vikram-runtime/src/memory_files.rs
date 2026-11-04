//! Free-form durable memory files (MEMORY.md, USER.md), Hermes-style.
//!
//! Entries are separated by the `§` delimiter. Each file has a char budget; writes that would
//! exceed it are refused. A frozen snapshot is captured on first load so the system prompt prefix
//! stays byte-stable for caching.

use std::fs;
use std::path::{Path, PathBuf};

pub const ENTRY_DELIM: &str = "\n§\n";
pub const MEMORY_BUDGET: usize = 8_000;
pub const USER_BUDGET: usize = 4_000;

#[derive(Clone, Debug)]
pub struct MemoryFile {
    path: PathBuf,
    budget: usize,
    snapshot: Option<String>,
}

impl MemoryFile {
    pub fn new(path: PathBuf, budget: usize) -> Self {
        Self {
            path,
            budget,
            snapshot: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load entries from disk (or empty if missing).
    pub fn load(&self) -> Vec<String> {
        match fs::read_to_string(&self.path) {
            Ok(s) => s
                .split(ENTRY_DELIM)
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(str::to_owned)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Frozen snapshot captured on first load (prefix-cache stability).
    pub fn snapshot(&mut self) -> String {
        if self.snapshot.is_none() {
            self.snapshot = Some(self.load().join(ENTRY_DELIM));
        }
        self.snapshot.clone().unwrap_or_default()
    }

    pub fn char_count(&self) -> usize {
        self.load().iter().map(|e| e.chars().count()).sum::<usize>()
            + (self.load().len().saturating_sub(1)) * ENTRY_DELIM.len()
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Append an entry if it fits; returns Ok(added) or Err(reason).
    pub fn add(&mut self, entry: &str) -> Result<(), String> {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err("empty entry".into());
        }
        let mut entries = self.load();
        // dedup exact
        if entries.iter().any(|e| e == entry) {
            return Ok(());
        }
        let new_count = self.char_count() + entry.chars().count() + ENTRY_DELIM.len();
        if new_count > self.budget {
            return Err(format!(
                "memory file full ({}/{} chars); remove an entry first",
                self.char_count(),
                self.budget
            ));
        }
        entries.push(entry.to_owned());
        self.write(&entries)?;
        self.snapshot = None;
        Ok(())
    }

    pub fn remove(&mut self, needle: &str) -> Result<usize, String> {
        let mut entries = self.load();
        let before = entries.len();
        entries.retain(|e| !e.contains(needle));
        let removed = before - entries.len();
        if removed > 0 {
            self.write(&entries)?;
            self.snapshot = None;
        }
        Ok(removed)
    }

    fn write(&self, entries: &[String]) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let content = entries.join(ENTRY_DELIM);
        fs::write(&self.path, content).map_err(|e| e.to_string())
    }
}

/// Convenience: default memory file paths under vikram home.
pub fn memory_file(home: &Path) -> MemoryFile {
    MemoryFile::new(home.join("memories").join("MEMORY.md"), MEMORY_BUDGET)
}

pub fn user_file(home: &Path) -> MemoryFile {
    MemoryFile::new(home.join("memories").join("USER.md"), USER_BUDGET)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn add_and_load() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut f = memory_file(home);
        f.add("I prefer Rust").unwrap();
        f.add("I prefer Rust").unwrap();
        let entries = f.load();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].contains("Rust"));
    }
    #[test]
    fn budget_enforced() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut f = MemoryFile::new(home.join("memories").join("M.md"), 20);
        f.add("short").unwrap();
        assert!(
            f.add("this is a very long entry that exceeds budget")
                .is_err()
        );
    }
    #[test]
    fn remove_works() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let mut f = memory_file(home);
        f.add("alpha").unwrap();
        f.add("beta").unwrap();
        let n = f.remove("alpha").unwrap();
        assert_eq!(n, 1);
        assert_eq!(f.load().len(), 1);
    }
}
