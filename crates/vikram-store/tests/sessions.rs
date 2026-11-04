use std::path::Path;
use vikram_contracts::{AgentId, JournalSink, NewRecord, ProjectId, RecordKind, SessionId};
use vikram_store::{ArtifactRow, FileChangeRow, Store};

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[test]
fn session_lifecycle_status_search_duplicate() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s = store
        .create_session("Build Rust browser", Path::new("/work"), None, now)
        .unwrap();
    assert_eq!(s.status, "active");
    assert!(store.get_session(&s.id).unwrap().is_some());

    store.rename_session(&s.id, "Fix JWT auth", now).unwrap();
    assert_eq!(
        store.get_session(&s.id).unwrap().unwrap().title,
        "Fix JWT auth"
    );

    store.set_session_status(&s.id, "archived", now).unwrap();
    assert!(store.list_sessions().unwrap().is_empty()); // active-only listing hides it
    assert_eq!(store.list_all_sessions().unwrap().len(), 1);

    store.set_session_status(&s.id, "active", now).unwrap();

    let hits = store.search_sessions("jwt", false).unwrap();
    assert_eq!(hits.len(), 1);

    let copy = store.duplicate_session(&s.id, now).unwrap().unwrap();
    assert_ne!(copy.id, s.id);
    assert_eq!(copy.title, "Fix JWT auth (copy)");
    assert_eq!(store.list_all_sessions().unwrap().len(), 2);
}

#[test]
fn project_crud_and_session_association() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s = store
        .create_session("s", Path::new("/work"), None, now)
        .unwrap();
    let p = store
        .create_project("website", Path::new("/tmp/website"), now)
        .unwrap();

    store.set_session_project(&s.id, Some(&p.id), now).unwrap();
    let row = store.get_session(&s.id).unwrap().unwrap();
    assert_eq!(row.project_id.as_ref(), Some(&p.id));

    let found = store
        .find_project_by_path(Path::new("/tmp/website"))
        .unwrap()
        .unwrap();
    assert_eq!(found.id, p.id);

    store.set_project_status(&p.id, "missing", now).unwrap();
    assert_eq!(store.get_project(&p.id).unwrap().unwrap().status, "missing");

    store.set_session_project(&s.id, None, now).unwrap();
    assert!(
        store
            .get_session(&s.id)
            .unwrap()
            .unwrap()
            .project_id
            .is_none()
    );

    store.delete_project(&p.id).unwrap();
    assert!(store.get_project(&p.id).unwrap().is_none());
}

#[test]
fn session_workspace_row_round_trip() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s = store
        .create_session("s", Path::new("/work"), None, now)
        .unwrap();

    store
        .set_session_workspace(
            &s.id,
            "session",
            Some(Path::new("/home/u/.vikram/sessions/x/workspace")),
            None,
            now,
        )
        .unwrap();
    let ws = store.get_session_workspace(&s.id).unwrap().unwrap();
    assert_eq!(ws.mode, "session");
    assert!(ws.workspace_path.is_some());

    let p = store
        .create_project("proj", Path::new("/proj"), now)
        .unwrap();
    store
        .set_session_workspace(&s.id, "project", None, Some(&p.id), now)
        .unwrap();
    let ws = store.get_session_workspace(&s.id).unwrap().unwrap();
    assert_eq!(ws.mode, "project");
    assert_eq!(ws.project_id.as_ref(), Some(&p.id));
}

#[test]
fn artifacts_index_without_copying() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s = store
        .create_session("s", Path::new("/work"), None, now)
        .unwrap();
    let agent = AgentId::new("rudra");

    let art = ArtifactRow {
        id: "art-1".into(),
        session: s.id.clone(),
        project_id: None,
        agent: Some(agent.clone()),
        name: "chart.png".into(),
        path: Path::new("/work/out/chart.png").into(),
        kind: "image".into(),
        mime: Some("image/png".into()),
        size: 42,
        checksum: Some("abc".into()),
        source: Some("tool:image.generate".into()),
        created_ms: now,
        updated_ms: now,
    };
    store.register_artifact(&art).unwrap();
    // re-registering the same path updates instead of duplicating
    store.register_artifact(&art).unwrap();

    let all = store.list_artifacts(None).unwrap();
    assert_eq!(all.len(), 1);
    let mine = store.list_artifacts(Some(&s.id)).unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].agent.as_ref(), Some(&agent));

    assert!(store.remove_artifact("art-1").unwrap());
    assert!(store.list_artifacts(None).unwrap().is_empty());
}

#[test]
fn file_changes_are_recorded_and_scoped_to_session() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s1 = store
        .create_session("one", Path::new("/work"), None, now)
        .unwrap();
    let s2 = store
        .create_session("two", Path::new("/work"), None, now)
        .unwrap();

    let change = FileChangeRow {
        id: "chg-1".into(),
        session: s1.id.clone(),
        agent: None,
        path: Path::new("/work/a.rs").into(),
        operation: "modify".into(),
        checksum_before: Some("before".into()),
        checksum_after: Some("after".into()),
        size: Some(12),
        checkpoint_path: Some(Path::new("/home/u/.vikram/sessions/x/checkpoints/cp").into()),
        ts_ms: now,
    };
    store.record_file_change(&change).unwrap();

    assert_eq!(store.list_file_changes(&s1.id).unwrap().len(), 1);
    assert!(store.list_file_changes(&s2.id).unwrap().is_empty());

    store.delete_file_changes_for_session(&s1.id).unwrap();
    assert!(store.list_file_changes(&s1.id).unwrap().is_empty());
}

#[test]
fn deleting_a_session_removes_its_metadata_not_projects() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s = store
        .create_session("s", Path::new("/work"), None, now)
        .unwrap();
    let p = store
        .create_project("website", Path::new("/proj"), now)
        .unwrap();
    store.set_session_project(&s.id, Some(&p.id), now).unwrap();
    store
        .set_session_workspace(&s.id, "session", Some(Path::new("/ws")), None, now)
        .unwrap();
    store
        .record_file_change(&FileChangeRow {
            id: "chg".into(),
            session: s.id.clone(),
            agent: None,
            path: Path::new("/ws/f.txt").into(),
            operation: "create".into(),
            checksum_before: None,
            checksum_after: Some("h".into()),
            size: Some(1),
            checkpoint_path: None,
            ts_ms: now,
        })
        .unwrap();

    store.delete_session(&s.id).unwrap();
    assert!(store.get_session(&s.id).unwrap().is_none());
    assert!(store.get_session_workspace(&s.id).unwrap().is_none());
    assert!(store.list_artifacts(Some(&s.id)).unwrap().is_empty());
    assert!(store.list_file_changes(&s.id).unwrap().is_empty());
    // the project survives session deletion
    assert!(store.get_project(&p.id).unwrap().is_some());
}

#[test]
fn session_ids_and_project_ids_are_distinct_types() {
    let s = SessionId::new("same");
    let p = ProjectId::new("same");
    assert_eq!(s.as_str(), p.as_str());
    assert_ne!(SessionId::KIND, ProjectId::KIND);
}

/// A database written before provenance existed: the positional names the old runtime gave out,
/// and no `title_source` column at all.
fn legacy_database(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("open legacy db");
    conn.execute_batch(
        "CREATE TABLE sessions (
            id         TEXT PRIMARY KEY,
            title      TEXT NOT NULL,
            cwd        TEXT NOT NULL,
            model      TEXT,
            agent_id   TEXT,
            created_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL
        );
        INSERT INTO sessions (id, title, cwd, created_ms, updated_ms) VALUES
            ('ses-old-1', 'Session 1', '/work', 1, 1),
            ('ses-old-2', 'New Session', '/work', 2, 2),
            ('ses-old-3', 'Build a login page for the…', '/work', 3, 3),
            ('ses-old-4', 'Quarterly review', '/work', 4, 4);",
    )
    .expect("write legacy rows");
}

#[test]
fn a_legacy_database_gains_provenance_without_losing_a_row_or_a_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vikram.db");
    legacy_database(&path);

    let store = Store::open(&path).unwrap();
    let rows = store.list_all_sessions().unwrap();
    assert_eq!(rows.len(), 4, "the upgrade keeps every session it found");

    let by_id = |id: &str| {
        store
            .get_session(&SessionId::new(id))
            .unwrap()
            .unwrap_or_else(|| panic!("{id} survived the upgrade"))
    };
    // The positional placeholders are worth replacing, so they come back waiting for a real name.
    assert_eq!(by_id("ses-old-1").title_source, vikram_store::TITLE_GENERIC);
    assert_eq!(by_id("ses-old-1").title_status, vikram_store::TITLE_PENDING);
    assert_eq!(by_id("ses-old-2").title_source, vikram_store::TITLE_GENERIC);
    // A name the runtime derived from the conversation is meaningful already: replaceable, not pending.
    assert_eq!(by_id("ses-old-3").title_source, vikram_store::TITLE_AUTO);
    assert_eq!(by_id("ses-old-3").title_status, vikram_store::TITLE_READY);
    // Anything else is treated as the operator's own choice and is never touched again.
    assert_eq!(by_id("ses-old-4").title_source, vikram_store::TITLE_MANUAL);
    assert_eq!(by_id("ses-old-4").title, "Quarterly review");

    // One-time: a rename after the upgrade is a human choice, and reopening must not re-judge it.
    store
        .rename_session(&SessionId::new("ses-old-1"), "Auth sprint", 50)
        .unwrap();
    drop(store);
    let reopened = Store::open(&path).unwrap();
    let renamed = reopened
        .get_session(&SessionId::new("ses-old-1"))
        .unwrap()
        .unwrap();
    assert_eq!(renamed.title, "Auth sprint");
    assert_eq!(renamed.title_source, vikram_store::TITLE_MANUAL);
    assert_eq!(renamed.title_status, vikram_store::TITLE_READY);
}

#[test]
fn a_derived_title_never_replaces_one_a_human_chose() {
    let store = Store::open_in_memory().unwrap();
    let unnamed = store
        .create_session_with_title(
            "New Session",
            Path::new("/work"),
            None,
            1,
            vikram_store::TITLE_GENERIC,
            vikram_store::TITLE_PENDING,
        )
        .unwrap();
    assert!(
        store
            .set_session_auto_title(&unnamed.id, "Rust JWT Authentication", 2)
            .unwrap(),
        "a session with no name of its own accepts the derived one"
    );
    let named = store.get_session(&unnamed.id).unwrap().unwrap();
    assert_eq!(named.title, "Rust JWT Authentication");
    assert_eq!(named.title_source, vikram_store::TITLE_AUTO);
    assert_eq!(named.title_status, vikram_store::TITLE_READY);

    let kept = store
        .create_session("Auth sprint", Path::new("/work"), None, 1)
        .unwrap();
    store
        .rename_session(&kept.id, "Auth sprint — week 3", 2)
        .unwrap();
    assert!(
        !store
            .set_session_auto_title(&kept.id, "Rust JWT Authentication", 3)
            .unwrap(),
        "the guard answers no, so nothing downstream reports a rename that did not happen"
    );
    let kept = store.get_session(&kept.id).unwrap().unwrap();
    assert_eq!(kept.title, "Auth sprint — week 3");
    assert_eq!(kept.title_source, vikram_store::TITLE_MANUAL);
}

#[test]
fn deleting_a_session_retires_its_identity_and_leaves_the_others_named() {
    let store = Store::open_in_memory().unwrap();
    let first = store
        .create_session("Alpha work", Path::new("/work"), None, 1)
        .unwrap();
    let second = store
        .create_session("Beta work", Path::new("/work"), None, 1)
        .unwrap();

    store.delete_session(&second.id).unwrap();
    let mut issued = vec![first.id.clone(), second.id.clone()];
    for n in 3..6 {
        let next = store
            .create_session("New Session", Path::new("/work"), None, n)
            .unwrap();
        assert!(
            !issued.contains(&next.id),
            "a retired identity is never handed out again"
        );
        issued.push(next.id.clone());
        store.delete_session(&next.id).unwrap();
    }

    assert!(store.get_session(&second.id).unwrap().is_none());
    let survivor = store.get_session(&first.id).unwrap().unwrap();
    assert_eq!(
        survivor.title, "Alpha work",
        "the name belongs to the session, not its slot"
    );
    assert_eq!(survivor.title_source, vikram_store::TITLE_AUTO);
}

#[test]
fn a_session_is_found_by_what_was_said_inside_it() {
    let store = Store::open_in_memory().unwrap();
    let talk = store
        .create_session("Deploy notes", Path::new("/work"), None, 1)
        .unwrap();
    store
        .create_session("Nothing related", Path::new("/work"), None, 1)
        .unwrap();
    store
        .journal()
        .append(&NewRecord::new(
            talk.id.clone(),
            RecordKind::Message,
            serde_json::json!({"text": "rotate the kafka broker certs before Friday"}),
        ))
        .expect("journal the line");

    let hits = store.search_sessions("kafka", false).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "the conversation, not just the title, is searchable"
    );
    assert_eq!(hits[0].id, talk.id);
    assert!(
        store.search_sessions("100%", false).unwrap().is_empty(),
        "a percent sign is text, not a wildcard"
    );
}

#[test]
fn recent_sessions_are_listed_by_activity_not_by_creation() {
    let store = Store::open_in_memory().unwrap();
    let older = store
        .create_session("Older", Path::new("/work"), None, 1)
        .unwrap();
    let newer = store
        .create_session("Newer", Path::new("/work"), None, 2)
        .unwrap();
    store.touch_session(&older.id, 99).unwrap();

    let ids: Vec<String> = store
        .list_recent_sessions()
        .unwrap()
        .into_iter()
        .map(|row| row.id.to_string())
        .collect();
    assert_eq!(
        ids,
        vec![older.id.to_string(), newer.id.to_string()],
        "the session used most recently is the one listed first"
    );
}
