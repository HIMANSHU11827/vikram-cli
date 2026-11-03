use std::path::Path;
use vikram_contracts::{AgentId, ProjectId, SessionId};
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
