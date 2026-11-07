//! Unified Ticket System manager.
//!
//! One engine for all three participant models (Hive teams, independent bots,
//! sub-agents). Participant type only changes collaboration/visibility rules,
//! never the storage or lifecycle. Persistence goes through `vikram_store`,
//! so tickets, participants, messages, assignments and events survive restarts.

// Every method maps a row of the ticket schema; grouping those fields into a struct would only
// move the same parameters one level down.
#![allow(clippy::too_many_arguments)]

use std::sync::Arc;

use vikram_contracts::{
    AssignmentId, EntityId, HiveId, SessionId, TicketEventId, TicketId, TicketMessageId, UserId,
};
use vikram_store::{
    AssignmentRow, Store, TicketEventRow, TicketMessageRow, TicketParticipantRow, TicketRow,
};

pub struct TicketManager {
    store: Arc<Store>,
}

impl TicketManager {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> Arc<Store> {
        Arc::clone(&self.store)
    }

    // ---- tickets ---------------------------------------------------------

    pub fn create(
        &self,
        owner_key: &str,
        owner_kind: &str,
        kind: &str,
        title: &str,
        request: &str,
        description: &str,
        strategy: Option<&str>,
        parent_ticket: Option<TicketId>,
        hive_id: Option<HiveId>,
        session_id: Option<SessionId>,
        now_ms: u64,
    ) -> Result<TicketRow, String> {
        let row = TicketRow {
            id: TicketId::generate(),
            owner_key: owner_key.to_owned(),
            owner_kind: owner_kind.to_owned(),
            title: title.to_owned(),
            description: description.to_owned(),
            request: request.to_owned(),
            kind: kind.to_owned(),
            status: "open".to_owned(),
            parent_ticket,
            hive_id,
            session_id,
            strategy: strategy.map(str::to_owned),
            result: None,
            failure_reason: None,
            created_ms: now_ms,
            updated_ms: now_ms,
            closed_ms: None,
        };
        self.store.create_ticket(&row).map_err(|e| e.to_string())
    }

    pub fn get(&self, id: &TicketId) -> Result<Option<TicketRow>, String> {
        self.store.get_ticket(id).map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Result<Vec<TicketRow>, String> {
        self.store.list_tickets().map_err(|e| e.to_string())
    }

    pub fn list_by_owner(&self, owner_key: &str) -> Result<Vec<TicketRow>, String> {
        self.store
            .list_tickets_by_owner(owner_key)
            .map_err(|e| e.to_string())
    }

    pub fn list_by_kind(&self, kind: &str) -> Result<Vec<TicketRow>, String> {
        self.store
            .list_tickets_by_kind(kind)
            .map_err(|e| e.to_string())
    }

    pub fn list_by_status(&self, status: &str) -> Result<Vec<TicketRow>, String> {
        self.store
            .list_tickets_by_status(status)
            .map_err(|e| e.to_string())
    }

    pub fn children(&self, parent: &TicketId) -> Result<Vec<TicketRow>, String> {
        self.store
            .list_child_tickets(parent)
            .map_err(|e| e.to_string())
    }

    pub fn search(&self, query: &str) -> Result<Vec<TicketRow>, String> {
        self.store.search_tickets(query).map_err(|e| e.to_string())
    }

    pub fn update(&self, row: &TicketRow) -> Result<(), String> {
        self.store.update_ticket(row).map_err(|e| e.to_string())
    }

    pub fn set_status(
        &self,
        id: &TicketId,
        status: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.store
            .set_ticket_status(id, status, now_ms)
            .map_err(|e| e.to_string())?;
        let _ = self.record_event(
            id,
            "status_change",
            Some(actor),
            serde_json::json!({"status": status}),
            now_ms,
        );
        Ok(())
    }

    pub fn close(&self, id: &TicketId, actor: &str, now_ms: u64) -> Result<(), String> {
        self.store
            .set_ticket_status(id, "completed", now_ms)
            .map_err(|e| e.to_string())?;
        let _ = self.record_event(id, "closed", Some(actor), serde_json::json!({}), now_ms);
        Ok(())
    }

    pub fn reopen(&self, id: &TicketId, actor: &str, now_ms: u64) -> Result<(), String> {
        self.store
            .reopen_ticket(id, now_ms)
            .map_err(|e| e.to_string())?;
        let _ = self.record_event(id, "reopened", Some(actor), serde_json::json!({}), now_ms);
        Ok(())
    }

    pub fn cancel(&self, id: &TicketId, actor: &str, now_ms: u64) -> Result<(), String> {
        self.store
            .cancel_ticket(id, now_ms)
            .map_err(|e| e.to_string())?;
        let _ = self.record_event(id, "cancelled", Some(actor), serde_json::json!({}), now_ms);
        Ok(())
    }

    pub fn set_result(
        &self,
        id: &TicketId,
        result: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.store
            .set_ticket_result(id, result, now_ms)
            .map_err(|e| e.to_string())?;
        let _ = self.record_event(
            id,
            "result",
            Some(actor),
            serde_json::json!({"result": result}),
            now_ms,
        );
        Ok(())
    }

    pub fn set_failure(
        &self,
        id: &TicketId,
        reason: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.store
            .set_ticket_failure(id, reason, now_ms)
            .map_err(|e| e.to_string())?;
        let _ = self.record_event(
            id,
            "failed",
            Some(actor),
            serde_json::json!({"reason": reason}),
            now_ms,
        );
        Ok(())
    }

    pub fn delete(&self, id: &TicketId) -> Result<(), String> {
        self.store.delete_ticket(id).map_err(|e| e.to_string())
    }

    // ---- participants ----------------------------------------------------

    pub fn add_participant(&self, row: &TicketParticipantRow) -> Result<(), String> {
        self.store
            .add_ticket_participant(row)
            .map_err(|e| e.to_string())?;
        let _ = self.record_event(
            &row.ticket,
            "participant_added",
            Some(&row.participant_key),
            serde_json::json!({"key": row.participant_key, "kind": row.participant_kind, "role": row.role}),
            row.joined_ms,
        );
        Ok(())
    }

    pub fn remove_participant(
        &self,
        ticket: &TicketId,
        key: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.store
            .remove_ticket_participant(ticket, key)
            .map_err(|e| e.to_string())?;
        let _ = self.record_event(
            ticket,
            "participant_removed",
            Some(actor),
            serde_json::json!({"key": key}),
            now_ms,
        );
        Ok(())
    }

    pub fn participants(&self, ticket: &TicketId) -> Result<Vec<TicketParticipantRow>, String> {
        self.store
            .list_ticket_participants(ticket)
            .map_err(|e| e.to_string())
    }

    pub fn participant(
        &self,
        ticket: &TicketId,
        key: &str,
    ) -> Result<Option<TicketParticipantRow>, String> {
        self.store
            .get_ticket_participant(ticket, key)
            .map_err(|e| e.to_string())
    }

    pub fn is_participant(&self, ticket: &TicketId, key: &str) -> bool {
        self.store
            .get_ticket_participant(ticket, key)
            .map(|o| o.is_some())
            .unwrap_or(false)
    }

    pub fn can_read(&self, ticket: &TicketId, key: &str) -> bool {
        self.store
            .get_ticket_participant(ticket, key)
            .map(|o| o.map(|p| p.can_read).unwrap_or(false))
            .unwrap_or(false)
    }

    pub fn can_write(&self, ticket: &TicketId, key: &str) -> bool {
        self.store
            .get_ticket_participant(ticket, key)
            .map(|o| o.map(|p| p.can_write || p.can_admin).unwrap_or(false))
            .unwrap_or(false)
    }

    pub fn can_admin(&self, ticket: &TicketId, key: &str) -> bool {
        self.store
            .get_ticket_participant(ticket, key)
            .map(|o| o.map(|p| p.can_admin).unwrap_or(false))
            .unwrap_or(false)
    }

    pub fn is_owner(&self, ticket: &TicketRow, key: &str) -> bool {
        ticket.owner_key == key
    }

    // ---- messages --------------------------------------------------------

    pub fn post_message(
        &self,
        ticket: &TicketId,
        author_key: &str,
        author_kind: &str,
        content: &str,
        visibility: &str,
        task_ref: Option<&str>,
        now_ms: u64,
    ) -> Result<TicketMessageRow, String> {
        // Permission check: author must be a participant with write permission,
        // OR the ticket owner. Enforced here (backend), not in the UI.
        if !self.can_write(ticket, author_key) && !self.is_owner_of(ticket, author_key)? {
            return Err(format!(
                "participant `{author_key}` may not post to ticket {ticket}"
            ));
        }
        // Visibility rules by participant type are enforced at read time; posting
        // `internal` requires admin.
        if visibility == "internal"
            && !self.can_admin(ticket, author_key)
            && !self.is_owner_of(ticket, author_key)?
        {
            return Err(format!(
                "participant `{author_key}` may not post internal notes"
            ));
        }
        let row = TicketMessageRow {
            id: TicketMessageId::generate(),
            ticket: ticket.clone(),
            author_key: author_key.to_owned(),
            author_kind: author_kind.to_owned(),
            content: content.to_owned(),
            visibility: visibility.to_owned(),
            ts_ms: now_ms,
            task_ref: task_ref.map(str::to_owned),
        };
        let saved = self
            .store
            .append_ticket_message(&row)
            .map_err(|e| e.to_string())?;
        let _ = self.record_event(
            ticket,
            "message",
            Some(author_key),
            serde_json::json!({"visibility": visibility, "len": content.len()}),
            now_ms,
        );
        Ok(saved)
    }

    fn is_owner_of(&self, ticket: &TicketId, key: &str) -> Result<bool, String> {
        Ok(self
            .store
            .get_ticket(ticket)
            .map_err(|e| e.to_string())?
            .map(|t| t.owner_key == key)
            .unwrap_or(false))
    }

    /// Returns messages the viewer is allowed to see, applying visibility rules.
    pub fn messages_for(
        &self,
        ticket: &TicketId,
        viewer_key: &str,
    ) -> Result<Vec<TicketMessageRow>, String> {
        let ticket_row = self
            .store
            .get_ticket(ticket)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("ticket {ticket} not found"))?;
        let is_owner = ticket_row.owner_key == viewer_key;
        let is_admin = self.can_admin(ticket, viewer_key);
        let is_participant = self.is_participant(ticket, viewer_key);
        if !is_participant && !is_owner && !is_admin {
            return Err(format!(
                "participant `{viewer_key}` may not read ticket {ticket}"
            ));
        }
        let all = self
            .store
            .list_ticket_messages(ticket)
            .map_err(|e| e.to_string())?;
        let out: Vec<TicketMessageRow> = all
            .into_iter()
            .filter(|m| {
                match m.visibility.as_str() {
                    "shared" => true,
                    "private" => {
                        // author, owner, admins may read. For sub-agent tickets, the
                        // parent (owner) already covered by is_owner.
                        m.author_key == viewer_key || is_owner || is_admin
                    }
                    "internal" => is_owner || is_admin,
                    _ => false,
                }
            })
            .collect();
        Ok(out)
    }

    // ---- assignments -----------------------------------------------------

    pub fn create_assignment(
        &self,
        ticket: &TicketId,
        assignee_key: &str,
        assignee_kind: &str,
        entity_id: Option<EntityId>,
        title: &str,
        description: &str,
        strategy: Option<&str>,
        parent_assignment: Option<AssignmentId>,
        now_ms: u64,
    ) -> Result<AssignmentRow, String> {
        // Only admins/owner may create assignments.
        // (Caller identity is not passed here; enforcement happens at runtime layer.
        //  The store-level manager trusts the runtime to gate; we still record the event.)
        let row = AssignmentRow {
            id: AssignmentId::generate(),
            ticket: ticket.clone(),
            parent_assignment,
            assignee_key: assignee_key.to_owned(),
            assignee_kind: assignee_kind.to_owned(),
            entity_id,
            title: title.to_owned(),
            description: description.to_owned(),
            status: "assigned".to_owned(),
            strategy: strategy.map(str::to_owned),
            result: None,
            error: None,
            created_ms: now_ms,
            updated_ms: now_ms,
            completed_ms: None,
        };
        let saved = self
            .store
            .create_assignment(&row)
            .map_err(|e| e.to_string())?;
        // Update ticket status to in_progress if it was open.
        if let Ok(Some(t)) = self.store.get_ticket(ticket)
            && (t.status == "open" || t.status == "reopened")
        {
            let _ = self.store.set_ticket_status(ticket, "in_progress", now_ms);
        }
        let _ = self.record_event(
            ticket,
            "assignment_created",
            Some(assignee_key),
            serde_json::json!({"assignment": saved.id.as_str(), "assignee": assignee_key, "title": title}),
            now_ms,
        );
        Ok(saved)
    }

    pub fn get_assignment(&self, id: &AssignmentId) -> Result<Option<AssignmentRow>, String> {
        self.store.get_assignment(id).map_err(|e| e.to_string())
    }

    pub fn assignments(&self, ticket: &TicketId) -> Result<Vec<AssignmentRow>, String> {
        self.store
            .list_assignments(ticket)
            .map_err(|e| e.to_string())
    }

    pub fn assignments_for(&self, assignee_key: &str) -> Result<Vec<AssignmentRow>, String> {
        self.store
            .list_assignments_by_assignee(assignee_key)
            .map_err(|e| e.to_string())
    }

    pub fn update_assignment_status(
        &self,
        id: &AssignmentId,
        status: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.store
            .set_assignment_status(id, status, now_ms)
            .map_err(|e| e.to_string())?;
        if let Ok(Some(a)) = self.store.get_assignment(id) {
            let _ = self.record_event(
                &a.ticket,
                "assignment_status",
                Some(actor),
                serde_json::json!({"assignment": id.as_str(), "status": status}),
                now_ms,
            );
        }
        Ok(())
    }

    pub fn complete_assignment(
        &self,
        id: &AssignmentId,
        result: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.store
            .set_assignment_result(id, result, now_ms)
            .map_err(|e| e.to_string())?;
        if let Ok(Some(a)) = self.store.get_assignment(id) {
            // Post a report message to the parent ticket (shared visibility) so the
            // parent/owner receives the result.
            let _ = self.post_message(
                &a.ticket,
                &a.assignee_key,
                &a.assignee_kind,
                &format!("[report] {}: {}", a.title, result),
                "shared",
                Some(id.as_str()),
                now_ms,
            );
            let _ = self.record_event(
                &a.ticket,
                "assignment_completed",
                Some(actor),
                serde_json::json!({"assignment": id.as_str(), "result": result}),
                now_ms,
            );
        }
        Ok(())
    }

    pub fn fail_assignment(
        &self,
        id: &AssignmentId,
        error: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.store
            .set_assignment_error(id, error, now_ms)
            .map_err(|e| e.to_string())?;
        if let Ok(Some(a)) = self.store.get_assignment(id) {
            let _ = self.post_message(
                &a.ticket,
                &a.assignee_key,
                &a.assignee_kind,
                &format!("[blocked] {}: {}", a.title, error),
                "shared",
                Some(id.as_str()),
                now_ms,
            );
            let _ = self.record_event(
                &a.ticket,
                "assignment_failed",
                Some(actor),
                serde_json::json!({"assignment": id.as_str(), "error": error}),
                now_ms,
            );
        }
        Ok(())
    }

    pub fn cancel_assignment(
        &self,
        id: &AssignmentId,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.update_assignment_status(id, "cancelled", actor, now_ms)
    }

    // ---- events ----------------------------------------------------------

    pub fn record_event(
        &self,
        ticket: &TicketId,
        kind: &str,
        actor_key: Option<&str>,
        payload: serde_json::Value,
        now_ms: u64,
    ) -> Result<TicketEventRow, String> {
        let row = TicketEventRow {
            id: TicketEventId::generate(),
            ticket: ticket.clone(),
            ts_ms: now_ms,
            kind: kind.to_owned(),
            actor_key: actor_key.map(str::to_owned),
            payload,
        };
        self.store
            .append_ticket_event(&row)
            .map_err(|e| e.to_string())
    }

    pub fn events(&self, ticket: &TicketId) -> Result<Vec<TicketEventRow>, String> {
        self.store
            .list_ticket_events(ticket)
            .map_err(|e| e.to_string())
    }

    // ---- participant-type helpers ---------------------------------------

    /// Create a Hive team ticket with shared collaboration: all hive members are
    /// added as participants with shared read/write permission.
    pub fn create_hive_ticket(
        &self,
        hive_id: &HiveId,
        owner_key: &str,
        title: &str,
        request: &str,
        strategy: &str,
        member_keys: &[(String, String)], // (key, kind)
        now_ms: u64,
    ) -> Result<TicketRow, String> {
        let ticket = self.create(
            owner_key,
            "hive_member",
            "hive",
            title,
            request,
            "",
            Some(strategy),
            None,
            Some(hive_id.clone()),
            None,
            now_ms,
        )?;
        // Owner as admin participant.
        let _ = self.add_participant(&TicketParticipantRow {
            ticket: ticket.id.clone(),
            participant_key: owner_key.to_owned(),
            participant_kind: "hive_member".to_owned(),
            entity_id: None,
            user_id: None,
            role: "coordinator".to_owned(),
            can_read: true,
            can_write: true,
            can_admin: true,
            joined_ms: now_ms,
        });
        for (key, kind) in member_keys {
            let _ = self.add_participant(&TicketParticipantRow {
                ticket: ticket.id.clone(),
                participant_key: key.clone(),
                participant_kind: kind.clone(),
                entity_id: None,
                user_id: None,
                role: "member".to_owned(),
                can_read: true,
                can_write: true,
                can_admin: false,
                joined_ms: now_ms,
            });
        }
        Ok(ticket)
    }

    /// Create an independent bot ticket: only the bot (owner) and explicitly
    /// added participants can access it. No automatic cross-bot visibility.
    pub fn create_bot_ticket(
        &self,
        bot_key: &str,
        title: &str,
        request: &str,
        now_ms: u64,
    ) -> Result<TicketRow, String> {
        let ticket = self.create(
            bot_key,
            "bot",
            "bot",
            title,
            request,
            "",
            Some("single"),
            None,
            None,
            None,
            now_ms,
        )?;
        let _ = self.add_participant(&TicketParticipantRow {
            ticket: ticket.id.clone(),
            participant_key: bot_key.to_owned(),
            participant_kind: "bot".to_owned(),
            entity_id: None,
            user_id: None,
            role: "owner".to_owned(),
            can_read: true,
            can_write: true,
            can_admin: true,
            joined_ms: now_ms,
        });
        Ok(ticket)
    }

    /// Create a sub-agent assignment under a parent ticket. The sub-agent gets
    /// its own assignment; its private execution messages are isolated from
    /// other sub-agents (visibility=private).
    pub fn create_subagent_assignment(
        &self,
        ticket: &TicketId,
        assignee_key: &str,
        assignee_kind: &str,
        entity_id: Option<EntityId>,
        title: &str,
        description: &str,
        now_ms: u64,
    ) -> Result<AssignmentRow, String> {
        self.create_assignment(
            ticket,
            assignee_key,
            assignee_kind,
            entity_id,
            title,
            description,
            None,
            None,
            now_ms,
        )
    }

    /// Participant key helpers.
    pub fn user_key(id: &UserId) -> String {
        format!("user:{}", id.as_str())
    }
    pub fn entity_key(id: &EntityId) -> String {
        format!("entity:{}", id.as_str())
    }
}
