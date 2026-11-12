use std::path::Path;
use vikram_contracts::{BotId, JournalSink, NewRecord, ProjectId, RecordKind, SessionId};
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
    let agent = BotId::new("rudra");

    let art = ArtifactRow {
        id: "art-1".into(),
        session: s.id.clone(),
        project_id: None,
        bot: Some(agent.clone()),
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
    assert_eq!(mine[0].bot.as_ref(), Some(&agent));

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
        bot: None,
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
            bot: None,
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

#[test]
fn session_fts_filtered_and_browse() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let agent = BotId::new("rudra");
    let s1 = store
        .create_session("Deploy notes", Path::new("/work"), None, now)
        .unwrap();
    store.set_session_bot(&s1.id, Some(&agent), now).unwrap();
    let p = store
        .create_project("website", Path::new("/tmp/website"), now)
        .unwrap();
    store.set_session_project(&s1.id, Some(&p.id), now).unwrap();
    store
        .index_session_text(&s1.id, "rotate the kafka broker certs")
        .unwrap();

    let s2 = store
        .create_session("Other", Path::new("/work"), None, now + 10)
        .unwrap();
    store.index_session_text(&s2.id, "kafka again").unwrap();

    let hits = store
        .search_session_text_filtered("kafka", Some(&agent), Some(&p.id), 0, 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, s1.id);

    let hits = store
        .search_session_text_filtered("kafka", None, None, now + 5, 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, s2.id);

    let browsed = store.browse_sessions().unwrap();
    assert_eq!(browsed.len(), 2);
    let total: u64 = browsed.iter().map(|(_, c)| *c).sum();
    assert_eq!(total, 2);
}

fn journal_message(store: &Store, session: &SessionId, text: &str) {
    store
        .journal()
        .append(&NewRecord::new(
            session.clone(),
            RecordKind::Message,
            serde_json::json!({"message": vikram_contracts::Message::user(text)}),
        ))
        .unwrap();
}

#[test]
fn a_fork_copies_the_journal_and_points_at_its_source() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s = store
        .create_session("Build Rust browser", Path::new("/work"), None, now)
        .unwrap();
    journal_message(&store, &s.id, "hello");
    journal_message(&store, &s.id, "and then");

    let fork = store.fork_session(&s.id, now).unwrap().unwrap();
    assert_ne!(fork.id, s.id);
    assert_eq!(fork.title, "Build Rust browser (fork)");
    assert_eq!(fork.parent_session.as_ref(), Some(&s.id));
    assert_eq!(fork.forked_from.as_ref(), Some(&s.id));

    let source = store.replay_messages(&s.id).unwrap();
    let copied = store.replay_messages(&fork.id).unwrap();
    assert_eq!(source.len(), 2);
    assert_eq!(copied.len(), 2, "the fork carries the whole journal");
    assert_eq!(copied[0].seq, source[0].seq);
    assert_eq!(copied[1].payload, source[1].payload);

    // The lineage is queryable both ways.
    let forks = store.list_session_forks(&s.id).unwrap();
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].id, fork.id);
    let stored = store.get_session(&fork.id).unwrap().unwrap();
    assert_eq!(stored.forked_from.as_ref(), Some(&s.id));

    // A missing source forks to nothing.
    assert!(
        store
            .fork_session(&SessionId::new("ses-missing"), now)
            .unwrap()
            .is_none()
    );

    // A fork of a fork points at the immediate source.
    let nested = store.fork_session(&fork.id, now).unwrap().unwrap();
    assert_eq!(nested.forked_from.as_ref(), Some(&fork.id));
}

#[test]
fn a_duplicate_of_a_fork_stays_in_the_same_lineage() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s = store
        .create_session("Story", Path::new("/work"), None, now)
        .unwrap();
    let fork = store.fork_session(&s.id, now).unwrap().unwrap();
    let copy = store.duplicate_session(&fork.id, now).unwrap().unwrap();
    assert_eq!(copy.forked_from.as_ref(), Some(&s.id));
    assert_eq!(copy.parent_session.as_ref(), Some(&s.id));
    // Duplication does not copy the journal; forking does.
    journal_message(&store, &fork.id, "in the fork");
    assert!(store.replay_messages(&copy.id).unwrap().is_empty());
    assert_eq!(store.replay_messages(&fork.id).unwrap().len(), 1);
}

#[test]
fn an_interrupted_session_is_found_and_recovered_by_completing_its_calls() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s = store
        .create_session("Crash mid-tool", Path::new("/work"), None, now)
        .unwrap();
    journal_message(&store, &s.id, "do the thing");
    let call = vikram_contracts::ToolCallId::new("call-crash-1");
    store
        .journal()
        .append(&NewRecord::new(
            s.id.clone(),
            RecordKind::ToolCall,
            serde_json::json!({"call": call.as_str(), "name": "shell.run"}),
        ))
        .unwrap();

    let interrupted = store.list_interrupted_sessions().unwrap();
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0].id, s.id);
    assert_eq!(interrupted[0].pending_calls, vec![call.clone()]);
    assert_eq!(interrupted[0].tool_preview.as_deref(), Some("shell.run"));

    let answered = store.recover_interrupted_session(&s.id, now).unwrap();
    assert_eq!(answered, vec![call]);
    assert!(
        store.list_interrupted_sessions().unwrap().is_empty(),
        "recovery completes the pending calls"
    );

    // The journal gained rows: the synthetic result and the recovery marker.
    let records = store.replay_messages(&s.id).unwrap();
    assert_eq!(records.len(), 4, "nothing was lost, two rows were added");
    assert_eq!(records[2].kind, RecordKind::ToolResult);
    assert_eq!(records[3].kind, RecordKind::Status);
    assert_eq!(
        records[3].payload.get("recovery").and_then(|v| v.as_str()),
        Some("interrupted_session")
    );

    // Recovering again is a no-op: nothing pending, no new rows.
    let again = store.recover_interrupted_session(&s.id, now).unwrap();
    assert!(again.is_empty());
    assert_eq!(store.replay_messages(&s.id).unwrap().len(), 4);
}

#[test]
fn a_clean_session_is_not_interrupted() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s = store
        .create_session("Clean", Path::new("/work"), None, now)
        .unwrap();
    journal_message(&store, &s.id, "finished");
    assert!(store.list_interrupted_sessions().unwrap().is_empty());
    // A session with no journal at all is not interrupted either.
    let empty = store
        .create_session("Empty", Path::new("/work"), None, now)
        .unwrap();
    assert!(store.list_interrupted_sessions().unwrap().is_empty());
    let _ = empty;
}

#[test]
fn export_round_trips_through_json_and_renders_markdown() {
    let store = Store::open_in_memory().unwrap();
    let now = now();
    let s = store
        .create_session("Export me", Path::new("/work"), None, now)
        .unwrap();
    journal_message(&store, &s.id, "exportable words");
    store
        .record_usage(&vikram_store::UsageRow {
            id: 0,
            session: s.id.clone(),
            ts_ms: now,
            model: "test".into(),
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            measured: true,
        })
        .unwrap();

    let json = store
        .export_session_text(&s.id, vikram_store::ExportFormat::Json, now)
        .unwrap()
        .unwrap();
    let parsed: vikram_store::SessionExport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.session.id, s.id);
    assert_eq!(parsed.journal.len(), 1);
    assert_eq!(parsed.usage.len(), 1);
    assert_eq!(parsed.usage[0].total_tokens, 15);

    let markdown = store
        .export_session_text(&s.id, vikram_store::ExportFormat::Markdown, now)
        .unwrap()
        .unwrap();
    assert!(markdown.starts_with("# Export me"));
    assert!(markdown.contains("exportable words"));
    assert!(markdown.contains("## Usage"));

    assert!(
        store
            .export_session_text(
                &SessionId::new("ses-missing"),
                vikram_store::ExportFormat::Json,
                now
            )
            .unwrap()
            .is_none()
    );

    assert_eq!(
        vikram_store::ExportFormat::parse("json"),
        Some(vikram_store::ExportFormat::Json)
    );
    assert_eq!(
        vikram_store::ExportFormat::parse("md"),
        Some(vikram_store::ExportFormat::Markdown)
    );
    assert!(vikram_store::ExportFormat::parse("yaml").is_none());
}
