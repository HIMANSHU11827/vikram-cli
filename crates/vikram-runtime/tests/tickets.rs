// The process-global home lock is deliberately held across the turn: it is what keeps two tests
// from sharing one `VIKRAM_HOME`, and the guard is not used as a channel between tasks.
#![allow(clippy::await_holding_lock)]

use vikram_runtime::{Config, Runtime};

fn isolated_home(dir: &std::path::Path) -> std::sync::MutexGuard<'static, ()> {
    let guard = vikram_runtime::config::lock_security_env();
    unsafe { std::env::set_var("VIKRAM_HOME", dir.join("home")) };
    guard
}

fn config_for(dir: &std::path::Path) -> Config {
    Config {
        model: "test-model".into(),
        workspace_root: Some(dir.to_path_buf()),
        ..Config::default()
    }
}

#[tokio::test]
async fn bot_tickets_are_independent_and_private() {
    let dir = tempfile::tempdir().unwrap();
    let _home = isolated_home(dir.path());
    let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
    let handle = runtime.handle();

    let bot_a = "bot:codebot";
    let bot_b = "bot:reviewbot";
    let ta = handle
        .ticket_create_bot(bot_a, "Fix Python app", "fix it")
        .unwrap();
    let tb = handle
        .ticket_create_bot(bot_b, "Review code", "review it")
        .unwrap();

    // Each bot owns its own ticket.
    let a_rows = handle.ticket_list_by_owner(bot_a).unwrap();
    let b_rows = handle.ticket_list_by_owner(bot_b).unwrap();
    assert_eq!(a_rows.len(), 1);
    assert_eq!(b_rows.len(), 1);
    assert_ne!(ta, tb);

    // Bot A posts a private message.
    handle
        .ticket_post_message(&ta, bot_a, "bot", "secret progress", "private")
        .unwrap();

    // Bot A can read its own private message.
    let a_msgs = handle.ticket_messages(&ta, bot_a).unwrap();
    assert_eq!(a_msgs.len(), 1);

    // Bot B cannot read Bot A's ticket messages (not a participant).
    let b_read = handle.ticket_messages(&ta, bot_b);
    assert!(
        b_read.is_err(),
        "bot B must not read bot A's private ticket"
    );

    // Bot B's ticket has no messages from A.
    let b_msgs = handle.ticket_messages(&tb, bot_b).unwrap();
    assert!(b_msgs.is_empty());
}

#[tokio::test]
async fn hive_ticket_shared_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let _home = isolated_home(dir.path());
    let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
    let handle = runtime.handle();

    let coord = "hive:rudra";
    let members = vec![("hive:karma".to_string(), "hive_member".to_string())];
    let tid = handle
        .ticket_create_hive(
            &vikram_contracts::HiveId::generate(),
            coord,
            "Build Rust browser",
            "build browser",
            "parallel",
            &members,
        )
        .unwrap();

    // Coordinator posts shared message.
    handle
        .ticket_post_message(&tid, coord, "hive_member", "decide rendering", "shared")
        .unwrap();
    // Member posts shared reply.
    handle
        .ticket_post_message(
            &tid,
            "hive:karma",
            "hive_member",
            "separate layout/paint",
            "shared",
        )
        .unwrap();
    // Member posts private note.
    handle
        .ticket_post_message(
            &tid,
            "hive:karma",
            "hive_member",
            "private scratch",
            "private",
        )
        .unwrap();

    // Coordinator sees all (shared + private as admin).
    let coord_msgs = handle.ticket_messages(&tid, coord).unwrap();
    assert_eq!(coord_msgs.len(), 3);

    // Member sees shared (2) + own private (1) = 3; cannot see other members' private (none here).
    let member_msgs = handle.ticket_messages(&tid, "hive:karma").unwrap();
    assert_eq!(member_msgs.len(), 3);

    // A non-participant cannot read.
    assert!(handle.ticket_messages(&tid, "outsider").is_err());
}

#[tokio::test]
async fn subagent_assignment_isolation_and_parent_report() {
    let dir = tempfile::tempdir().unwrap();
    let _home = isolated_home(dir.path());
    let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
    let handle = runtime.handle();

    let parent = "agent:rudra";
    let tid = handle
        .ticket_create(
            parent,
            "agent",
            "general",
            "Implement JWT",
            "implement jwt auth",
        )
        .unwrap();

    // Parent creates two sub-agent assignments.
    let a1 = handle
        .ticket_create_subagent_assignment(
            &tid,
            "sub:karma",
            "subagent",
            None,
            "Implement JWT",
            "impl",
        )
        .unwrap();
    let a2 = handle
        .ticket_create_subagent_assignment(
            &tid,
            "sub:rohit",
            "subagent",
            None,
            "Write tests",
            "tests",
        )
        .unwrap();

    // Assignments are linked to parent ticket.
    let asgs = handle.ticket_assignments(&tid).unwrap();
    assert_eq!(asgs.len(), 2);

    // Sub-agent A posts a private execution message.
    // First add sub-agent A as participant with write (owner adds).
    handle
        .ticket_add_participant(&vikram_store::TicketParticipantRow {
            ticket: tid.clone(),
            participant_key: "sub:karma".into(),
            participant_kind: "subagent".into(),
            entity_id: None,
            user_id: None,
            role: "assignee".into(),
            can_read: true,
            can_write: true,
            can_admin: false,
            joined_ms: vikram_runtime::types::now_ms(),
        })
        .unwrap();
    handle
        .ticket_add_participant(&vikram_store::TicketParticipantRow {
            ticket: tid.clone(),
            participant_key: "sub:rohit".into(),
            participant_kind: "subagent".into(),
            entity_id: None,
            user_id: None,
            role: "assignee".into(),
            can_read: true,
            can_write: true,
            can_admin: false,
            joined_ms: vikram_runtime::types::now_ms(),
        })
        .unwrap();

    handle
        .ticket_post_message(
            &tid,
            "sub:karma",
            "subagent",
            "private impl detail",
            "private",
        )
        .unwrap();

    // Sub-agent B cannot see A's private message.
    let b_msgs = handle.ticket_messages(&tid, "sub:rohit").unwrap();
    assert!(
        !b_msgs.iter().any(|m| m.content == "private impl detail"),
        "sub-agent B must not read A's private message"
    );

    // Parent (owner) can see A's private message.
    handle
        .ticket_add_participant(&vikram_store::TicketParticipantRow {
            ticket: tid.clone(),
            participant_key: parent.into(),
            participant_kind: "agent".into(),
            entity_id: None,
            user_id: None,
            role: "owner".into(),
            can_read: true,
            can_write: true,
            can_admin: true,
            joined_ms: vikram_runtime::types::now_ms(),
        })
        .unwrap();
    let parent_msgs = handle.ticket_messages(&tid, parent).unwrap();
    assert!(
        parent_msgs
            .iter()
            .any(|m| m.content == "private impl detail")
    );

    // Complete assignment A -> posts a shared report; parent sees it.
    handle
        .ticket_complete_assignment(&a1.id, "JWT implemented", "sub:karma")
        .unwrap();
    let parent_msgs2 = handle.ticket_messages(&tid, parent).unwrap();
    assert!(parent_msgs2.iter().any(|m| m.content.contains("[report]")));

    // Assignment completion does not complete the parent ticket.
    let t = handle.ticket_get(&tid).unwrap().unwrap();
    assert_ne!(t.status, "completed");

    // But assignment status is completed.
    let a1r = handle.tickets().get_assignment(&a1.id).unwrap().unwrap();
    assert_eq!(a1r.status, "completed");
    let _ = a2;
}

#[tokio::test]
async fn ticket_persistence_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let _home = isolated_home(dir.path());
    let tid = {
        let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
        let handle = runtime.handle();
        let t = handle
            .ticket_create("bot:x", "bot", "bot", "Persist me", "req")
            .unwrap();
        handle
            .ticket_post_message(&t, "bot:x", "bot", "hello", "shared")
            .unwrap();
        t
    };
    // reopen (new runtime, same home)
    let runtime2 = Runtime::start(config_for(dir.path())).await.unwrap();
    let handle2 = runtime2.handle();
    let t2 = handle2.ticket_get(&tid).unwrap().unwrap();
    assert_eq!(t2.title, "Persist me");
    let msgs = handle2.ticket_messages(&tid, "bot:x").unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "hello");
}

#[tokio::test]
async fn unauthorized_participant_cannot_post() {
    let dir = tempfile::tempdir().unwrap();
    let _home = isolated_home(dir.path());
    let runtime = Runtime::start(config_for(dir.path())).await.unwrap();
    let handle = runtime.handle();
    let tid = handle
        .ticket_create("owner", "user", "general", "T", "r")
        .unwrap();
    // outsider not participant -> cannot post
    let res = handle.ticket_post_message(&tid, "outsider", "user", "hi", "shared");
    assert!(res.is_err());
}
