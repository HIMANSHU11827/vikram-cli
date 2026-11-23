use std::path::Path;

use vikram_runtime::memory::MemoryStore;

use crate::MemoryAction;

pub fn run(action: MemoryAction, db: &Path) -> Result<(), String> {
    if let MemoryAction::Snapshot = action {
        for line in snapshot(&vikram_runtime::config::vikram_home())? {
            println!("{line}");
        }
        return Ok(());
    }
    let store = open_existing(db)?;
    let lines = match action {
        MemoryAction::List { scope } => list(&store, scope.as_deref(), db)?,
        MemoryAction::Search { query, scope } => search(&store, &query, scope.as_deref())?,
        MemoryAction::Forget {
            key,
            scope,
            dry_run,
        } => forget(&store, &key, scope.as_deref(), dry_run)?,
        MemoryAction::Count => count(&store)?,
        MemoryAction::Snapshot => unreachable!("handled before the store is opened"),
    };
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// The block the prompt actually carries: the same files, the same budgets, the same delimiter.
pub fn snapshot(home: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for (label, file) in [
        ("MEMORY", vikram_runtime::memory_files::memory_file(home)),
        ("USER", vikram_runtime::memory_files::user_file(home)),
    ] {
        let entries = file.try_load()?;
        out.push(format!(
            "{label}: {} entry(s), {} / {} chars — {}",
            entries.len(),
            file.char_count(),
            file.budget(),
            file.path().display()
        ));
        if entries.is_empty() {
            out.push("  (empty: nothing from this file reaches the prompt)".to_string());
            continue;
        }
        for entry in entries {
            out.push(format!("  {entry}"));
        }
    }
    Ok(out)
}

fn open_existing(db: &Path) -> Result<MemoryStore, String> {
    if !db.exists() {
        return Err(format!(
            "no memory database at {} — the runtime has not stored anything yet",
            db.display()
        ));
    }
    MemoryStore::open(db).map_err(|e| e.to_string())
}

fn scoped_entries(
    store: &MemoryStore,
    scope: Option<&str>,
) -> Result<Vec<(String, String, String)>, String> {
    match scope {
        Some(scope) => Ok(store
            .list(scope)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(key, value)| (scope.to_string(), key, value))
            .collect()),
        None => store.list_all().map_err(|e| e.to_string()),
    }
}

fn entry_line(scope: &str, key: &str, value: &str) -> String {
    format!("{scope}  {key}  {}", crate::evolution::truncate(value, 120))
}

pub fn list(store: &MemoryStore, scope: Option<&str>, db: &Path) -> Result<Vec<String>, String> {
    let entries = scoped_entries(store, scope)?;
    if entries.is_empty() {
        return Ok(vec![match scope {
            Some(scope) => format!("no memory entries in scope `{scope}`"),
            None => format!("no memory entries in {}", db.display()),
        }]);
    }
    Ok(entries
        .into_iter()
        .map(|(scope, key, value)| entry_line(&scope, &key, &value))
        .collect())
}

pub fn search(
    store: &MemoryStore,
    query: &str,
    scope: Option<&str>,
) -> Result<Vec<String>, String> {
    let needle = query.to_lowercase();
    let matches: Vec<String> = scoped_entries(store, scope)?
        .into_iter()
        .filter(|(_, key, value)| {
            key.to_lowercase().contains(&needle) || value.to_lowercase().contains(&needle)
        })
        .map(|(scope, key, value)| entry_line(&scope, &key, &value))
        .collect();
    if matches.is_empty() {
        return Ok(vec![match scope {
            Some(scope) => format!("nothing matches \"{query}\" in scope `{scope}`"),
            None => format!("nothing matches \"{query}\""),
        }]);
    }
    Ok(matches)
}

pub fn forget(
    store: &MemoryStore,
    key: &str,
    scope: Option<&str>,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let target = match scope {
        Some(scope) => {
            if store.get(key, scope).map_err(|e| e.to_string())?.is_none() {
                return Ok(vec![format!("no memory entry `{key}` in scope `{scope}`")]);
            }
            scope.to_string()
        }
        None => {
            let mut scopes: Vec<String> = store
                .list_all()
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|(_, existing, _)| existing == key)
                .map(|(scope, _, _)| scope)
                .collect();
            scopes.sort();
            scopes.dedup();
            match scopes.as_slice() {
                [] => return Ok(vec![format!("no memory entry with key `{key}`")]),
                [only] => only.clone(),
                _ => {
                    return Err(format!(
                        "memory key `{key}` exists in scopes {}; pass --scope <scope> to choose one",
                        scopes.join(", ")
                    ));
                }
            }
        }
    };
    if dry_run {
        return Ok(vec![format!("would forget {target}/{key}")]);
    }
    if !store.delete(key, &target).map_err(|e| e.to_string())? {
        return Ok(vec![format!("no memory entry `{key}` in scope `{target}`")]);
    }
    Ok(vec![format!("forgot {target}/{key}")])
}

pub fn count(store: &MemoryStore) -> Result<Vec<String>, String> {
    let total = store.total_count().map_err(|e| e.to_string())?;
    let mut scopes: Vec<String> = store
        .list_all()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(scope, _, _)| scope)
        .collect();
    scopes.sort();
    scopes.dedup();
    let mut lines = vec![format!("total: {total}")];
    for scope in scopes {
        let n = store.count(&scope).map_err(|e| e.to_string())?;
        lines.push(format!("{scope}: {n}"));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(entries: &[(&str, &str, &str)]) -> MemoryStore {
        let store = MemoryStore::open_in_memory().unwrap();
        for (scope, key, value) in entries {
            store.save(key, value, scope).unwrap();
        }
        store
    }

    fn db_path() -> &'static Path {
        Path::new("/tmp/vikram-test/vikram.db")
    }

    #[test]
    fn list_prints_every_scope_and_key() {
        let store = store_with(&[
            ("user:a", "name", "Alice"),
            ("user:a", "city", "Paris"),
            ("project:x", "stack", "Rust"),
        ]);

        let all = list(&store, None, db_path()).unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.contains(&"user:a  name  Alice".to_string()), "{all:?}");
        assert!(all.contains(&"user:a  city  Paris".to_string()), "{all:?}");
        assert!(
            all.contains(&"project:x  stack  Rust".to_string()),
            "{all:?}"
        );

        let scoped = list(&store, Some("user:a"), db_path()).unwrap();
        assert_eq!(scoped.len(), 2);
        assert!(scoped.iter().all(|line| line.starts_with("user:a  ")));

        let empty = MemoryStore::open_in_memory().unwrap();
        let lines = list(&empty, None, db_path()).unwrap();
        assert_eq!(
            lines,
            vec![format!("no memory entries in {}", db_path().display())]
        );
    }

    #[test]
    fn search_is_case_insensitive_and_respects_scope() {
        let store = store_with(&[("user:a", "name", "Alice"), ("user:b", "city", "Berlin")]);

        let hit = search(&store, "ALICE", None).unwrap();
        assert_eq!(hit, vec!["user:a  name  Alice".to_string()]);

        let hit = search(&store, "berlin", None).unwrap();
        assert_eq!(hit, vec!["user:b  city  Berlin".to_string()]);

        let scoped = search(&store, "berlin", Some("user:a")).unwrap();
        assert_eq!(
            scoped,
            vec!["nothing matches \"berlin\" in scope `user:a`".to_string()]
        );

        let miss = search(&store, "zebra", None).unwrap();
        assert_eq!(miss, vec!["nothing matches \"zebra\"".to_string()]);
    }

    #[test]
    fn forget_refuses_an_ambiguous_key_without_scope() {
        let store = store_with(&[("user:a", "k", "one"), ("user:b", "k", "two")]);

        let err = forget(&store, "k", None, false).unwrap_err();
        assert!(err.contains("exists in scopes"), "{err}");
        assert!(err.contains("user:a"), "{err}");
        assert!(err.contains("user:b"), "{err}");
        assert_eq!(store.get("k", "user:a").unwrap().as_deref(), Some("one"));
        assert_eq!(store.get("k", "user:b").unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn forget_with_scope_deletes_exactly_that_entry() {
        let store = store_with(&[("user:a", "k", "one"), ("user:b", "k", "two")]);

        let lines = forget(&store, "k", Some("user:a"), false).unwrap();
        assert_eq!(lines, vec!["forgot user:a/k".to_string()]);
        assert!(store.get("k", "user:a").unwrap().is_none());
        assert_eq!(store.get("k", "user:b").unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn forget_reports_a_miss_for_an_unknown_key() {
        let store = store_with(&[("user:a", "k", "one")]);

        let unscoped = forget(&store, "missing", None, false).unwrap();
        assert_eq!(
            unscoped,
            vec!["no memory entry with key `missing`".to_string()]
        );

        let scoped = forget(&store, "missing", Some("user:a"), false).unwrap();
        assert_eq!(
            scoped,
            vec!["no memory entry `missing` in scope `user:a`".to_string()]
        );
        assert_eq!(store.total_count().unwrap(), 1);
    }

    #[test]
    fn forget_dry_run_leaves_the_store_unchanged() {
        let store = store_with(&[("user:a", "k", "one")]);

        let lines = forget(&store, "k", Some("user:a"), true).unwrap();
        assert_eq!(lines, vec!["would forget user:a/k".to_string()]);
        assert_eq!(store.get("k", "user:a").unwrap().as_deref(), Some("one"));
    }

    #[test]
    fn count_reports_totals_per_scope() {
        let store = store_with(&[
            ("user:a", "name", "Alice"),
            ("user:a", "city", "Paris"),
            ("user:b", "city", "Berlin"),
        ]);

        assert_eq!(
            count(&store).unwrap(),
            vec![
                "total: 3".to_string(),
                "user:a: 2".to_string(),
                "user:b: 1".to_string(),
            ]
        );
    }

    #[test]
    fn snapshot_says_an_empty_file_contributes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let lines = snapshot(dir.path()).unwrap();
        assert!(
            lines.iter().any(|l| l.starts_with("MEMORY: 0 entry(s)")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("nothing from this file reaches the prompt")),
            "{lines:?}"
        );
    }

    #[test]
    fn snapshot_lists_the_entries_the_prompt_would_carry() {
        let dir = tempfile::tempdir().unwrap();
        vikram_runtime::memory_files::memory_file(dir.path())
            .add("the operator prefers tabs")
            .unwrap();
        let lines = snapshot(dir.path()).unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.trim() == "the operator prefers tabs"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("MEMORY: 1 entry(s)")),
            "{lines:?}"
        );
    }
}
