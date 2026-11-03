use vikram_contracts::{AssignmentId, TicketId, TicketMessageId};
use vikram_store::{AssignmentRow, Store, TicketMessageRow, TicketParticipantRow, TicketRow};

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[test]
fn ticket_create_get_list() {
    let store = Store::open_in_memory().unwrap();
    let n = now();
    let row = TicketRow {
        id: TicketId::generate(),
        owner_key: "bot:codebot".into(),
        owner_kind: "bot".into(),
        title: "Fix Python app".into(),
        description: "".into(),
        request: "fix my python application".into(),
        kind: "bot".into(),
        status: "open".into(),
        parent_ticket: None,
        hive_id: None,
        session_id: None,
        strategy: Some("single".into()),
        result: None,
        failure_reason: None,
        created_ms: n,
        updated_ms: n,
        closed_ms: None,
    };
    let saved = store.create_ticket(&row).unwrap();
    assert_eq!(saved.id, row.id);
    assert!(store.get_ticket(&row.id).unwrap().is_some());
    assert_eq!(store.list_tickets().unwrap().len(), 1);
    assert_eq!(store.list_tickets_by_kind("bot").unwrap().len(), 1);
    assert_eq!(store.list_tickets_by_owner("bot:codebot").unwrap().len(), 1);
    assert_eq!(store.list_tickets_by_status("open").unwrap().len(), 1);
}

#[test]
fn ticket_status_lifecycle() {
    let store = Store::open_in_memory().unwrap();
    let n = now();
    let id = TicketId::generate();
    store
        .create_ticket(&TicketRow {
            id: id.clone(),
            owner_key: "u".into(),
            owner_kind: "user".into(),
            title: "t".into(),
            description: "".into(),
            request: "r".into(),
            kind: "general".into(),
            status: "open".into(),
            parent_ticket: None,
            hive_id: None,
            session_id: None,
            strategy: None,
            result: None,
            failure_reason: None,
            created_ms: n,
            updated_ms: n,
            closed_ms: None,
        })
        .unwrap();
    store.set_ticket_status(&id, "in_progress", n + 1).unwrap();
    assert_eq!(
        store.get_ticket(&id).unwrap().unwrap().status,
        "in_progress"
    );
    store.set_ticket_result(&id, "done", n + 2).unwrap();
    let t = store.get_ticket(&id).unwrap().unwrap();
    assert_eq!(t.result.as_deref(), Some("done"));
    store.reopen_ticket(&id, n + 3).unwrap();
    assert_eq!(store.get_ticket(&id).unwrap().unwrap().status, "reopened");
    store.cancel_ticket(&id, n + 4).unwrap();
    assert_eq!(store.get_ticket(&id).unwrap().unwrap().status, "cancelled");
}

#[test]
fn participants_and_messages() {
    let store = Store::open_in_memory().unwrap();
    let n = now();
    let id = TicketId::generate();
    store
        .create_ticket(&TicketRow {
            id: id.clone(),
            owner_key: "hive:coord".into(),
            owner_kind: "hive_member".into(),
            title: "Browser".into(),
            description: "".into(),
            request: "build".into(),
            kind: "hive".into(),
            status: "open".into(),
            parent_ticket: None,
            hive_id: None,
            session_id: None,
            strategy: Some("parallel".into()),
            result: None,
            failure_reason: None,
            created_ms: n,
            updated_ms: n,
            closed_ms: None,
        })
        .unwrap();

    store
        .add_ticket_participant(&TicketParticipantRow {
            ticket: id.clone(),
            participant_key: "hive:coord".into(),
            participant_kind: "hive_member".into(),
            entity_id: None,
            user_id: None,
            role: "coordinator".into(),
            can_read: true,
            can_write: true,
            can_admin: true,
            joined_ms: n,
        })
        .unwrap();
    store
        .add_ticket_participant(&TicketParticipantRow {
            ticket: id.clone(),
            participant_key: "hive:karma".into(),
            participant_kind: "hive_member".into(),
            entity_id: None,
            user_id: None,
            role: "member".into(),
            can_read: true,
            can_write: true,
            can_admin: false,
            joined_ms: n,
        })
        .unwrap();
    assert_eq!(store.list_ticket_participants(&id).unwrap().len(), 2);
    assert!(
        store
            .get_ticket_participant(&id, "hive:karma")
            .unwrap()
            .is_some()
    );

    let msg = TicketMessageRow {
        id: TicketMessageId::generate(),
        ticket: id.clone(),
        author_key: "hive:coord".into(),
        author_kind: "hive_member".into(),
        content: "We need to decide rendering".into(),
        visibility: "shared".into(),
        ts_ms: n,
        task_ref: None,
    };
    store.append_ticket_message(&msg).unwrap();
    let msgs = store.list_ticket_messages(&id).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].visibility, "shared");

    // remove participant
    store.remove_ticket_participant(&id, "hive:karma").unwrap();
    assert_eq!(store.list_ticket_participants(&id).unwrap().len(), 1);
}

#[test]
fn assignments_and_events() {
    let store = Store::open_in_memory().unwrap();
    let n = now();
    let tid = TicketId::generate();
    store
        .create_ticket(&TicketRow {
            id: tid.clone(),
            owner_key: "parent".into(),
            owner_kind: "agent".into(),
            title: "JWT".into(),
            description: "".into(),
            request: "implement jwt".into(),
            kind: "subagent".into(),
            status: "open".into(),
            parent_ticket: None,
            hive_id: None,
            session_id: None,
            strategy: None,
            result: None,
            failure_reason: None,
            created_ms: n,
            updated_ms: n,
            closed_ms: None,
        })
        .unwrap();

    let aid = AssignmentId::generate();
    store
        .create_assignment(&AssignmentRow {
            id: aid.clone(),
            ticket: tid.clone(),
            parent_assignment: None,
            assignee_key: "sub:karma".into(),
            assignee_kind: "subagent".into(),
            entity_id: None,
            title: "Implement JWT".into(),
            description: "do it".into(),
            status: "assigned".into(),
            strategy: None,
            result: None,
            error: None,
            created_ms: n,
            updated_ms: n,
            completed_ms: None,
        })
        .unwrap();
    assert_eq!(store.list_assignments(&tid).unwrap().len(), 1);
    assert_eq!(
        store
            .list_assignments_by_assignee("sub:karma")
            .unwrap()
            .len(),
        1
    );
    store
        .set_assignment_result(&aid, "jwt done", n + 1)
        .unwrap();
    let a = store.get_assignment(&aid).unwrap().unwrap();
    assert_eq!(a.status, "completed");
    assert_eq!(a.result.as_deref(), Some("jwt done"));

    // event
    use vikram_contracts::TicketEventId;
    store
        .append_ticket_event(&vikram_store::TicketEventRow {
            id: TicketEventId::generate(),
            ticket: tid.clone(),
            ts_ms: n,
            kind: "assignment_completed".into(),
            actor_key: Some("sub:karma".into()),
            payload: serde_json::json!({"ok": true}),
        })
        .unwrap();
    assert_eq!(store.list_ticket_events(&tid).unwrap().len(), 1);
}

#[test]
fn delete_ticket_cascades() {
    let store = Store::open_in_memory().unwrap();
    let n = now();
    let tid = TicketId::generate();
    store
        .create_ticket(&TicketRow {
            id: tid.clone(),
            owner_key: "o".into(),
            owner_kind: "user".into(),
            title: "t".into(),
            description: "".into(),
            request: "r".into(),
            kind: "general".into(),
            status: "open".into(),
            parent_ticket: None,
            hive_id: None,
            session_id: None,
            strategy: None,
            result: None,
            failure_reason: None,
            created_ms: n,
            updated_ms: n,
            closed_ms: None,
        })
        .unwrap();
    store
        .add_ticket_participant(&TicketParticipantRow {
            ticket: tid.clone(),
            participant_key: "o".into(),
            participant_kind: "user".into(),
            entity_id: None,
            user_id: None,
            role: "owner".into(),
            can_read: true,
            can_write: true,
            can_admin: true,
            joined_ms: n,
        })
        .unwrap();
    store
        .append_ticket_message(&TicketMessageRow {
            id: TicketMessageId::generate(),
            ticket: tid.clone(),
            author_key: "o".into(),
            author_kind: "user".into(),
            content: "hi".into(),
            visibility: "shared".into(),
            ts_ms: n,
            task_ref: None,
        })
        .unwrap();
    store.delete_ticket(&tid).unwrap();
    assert!(store.get_ticket(&tid).unwrap().is_none());
    assert!(store.list_ticket_participants(&tid).unwrap().is_empty());
    assert!(store.list_ticket_messages(&tid).unwrap().is_empty());
    assert!(store.list_ticket_events(&tid).unwrap().is_empty());
}

#[test]
fn child_tickets() {
    let store = Store::open_in_memory().unwrap();
    let n = now();
    let parent = TicketId::generate();
    store
        .create_ticket(&TicketRow {
            id: parent.clone(),
            owner_key: "o".into(),
            owner_kind: "agent".into(),
            title: "Main".into(),
            description: "".into(),
            request: "build browser".into(),
            kind: "general".into(),
            status: "open".into(),
            parent_ticket: None,
            hive_id: None,
            session_id: None,
            strategy: None,
            result: None,
            failure_reason: None,
            created_ms: n,
            updated_ms: n,
            closed_ms: None,
        })
        .unwrap();
    let child = TicketId::generate();
    store
        .create_ticket(&TicketRow {
            id: child.clone(),
            owner_key: "sub".into(),
            owner_kind: "subagent".into(),
            title: "Child".into(),
            description: "".into(),
            request: "rendering".into(),
            kind: "subagent".into(),
            status: "open".into(),
            parent_ticket: Some(parent.clone()),
            hive_id: None,
            session_id: None,
            strategy: None,
            result: None,
            failure_reason: None,
            created_ms: n,
            updated_ms: n,
            closed_ms: None,
        })
        .unwrap();
    let children = store.list_child_tickets(&parent).unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, child);
}
