//! Hive group chat manager.
//!
//! Hive is a *group conversation* shared by AI entities (agents, bots, or a mix). It is not an
//! entity itself. This manager persists hive sessions, participants and messages through the store
//! and exposes operations the runtime handle and tools can call.

use std::sync::Arc;

use vikram_contracts::{EntityId, HiveId, HiveMessageId, SessionId, UserId};
use vikram_store::{HiveMessageRow, HiveParticipantRow, HiveRow, Store};

pub struct HiveManager {
    store: Arc<Store>,
}

impl HiveManager {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> Arc<Store> {
        Arc::clone(&self.store)
    }

    pub fn create(
        &self,
        owner: UserId,
        profile: Option<vikram_contracts::ProfileId>,
        name: &str,
        objective: &str,
        policy: &str,
        now_ms: u64,
    ) -> Result<HiveRow, String> {
        let row = HiveRow {
            id: HiveId::generate(),
            owner_user: owner,
            profile_id: profile,
            name: name.to_owned(),
            objective: objective.to_owned(),
            session_id: None,
            state: "active".to_owned(),
            policy: policy.to_owned(),
            created_ms: now_ms,
            updated_ms: now_ms,
        };
        self.store.create_hive(&row).map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Result<Vec<HiveRow>, String> {
        self.store.list_hives().map_err(|e| e.to_string())
    }

    pub fn list_by_user(&self, user: &UserId) -> Result<Vec<HiveRow>, String> {
        self.store
            .list_hives_by_user(user)
            .map_err(|e| e.to_string())
    }

    pub fn get(&self, id: &HiveId) -> Result<Option<HiveRow>, String> {
        self.store.get_hive(id).map_err(|e| e.to_string())
    }

    pub fn delete(&self, id: &HiveId) -> Result<(), String> {
        self.store.delete_hive(id).map_err(|e| e.to_string())
    }

    pub fn set_session(
        &self,
        id: &HiveId,
        session: Option<&SessionId>,
        now_ms: u64,
    ) -> Result<(), String> {
        self.store
            .set_hive_session(id, session, now_ms)
            .map_err(|e| e.to_string())
    }

    pub fn update_state(&self, id: &HiveId, state: &str, now_ms: u64) -> Result<(), String> {
        let Some(mut row) = self.store.get_hive(id).map_err(|e| e.to_string())? else {
            return Err(format!("hive {} not found", id.as_str()));
        };
        row.state = state.to_owned();
        row.updated_ms = now_ms;
        self.store.update_hive(&row).map_err(|e| e.to_string())
    }

    pub fn add_participant(
        &self,
        hive: &HiveId,
        entity: &EntityId,
        kind: vikram_store::EntityKind,
        role: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        if self
            .store
            .get_hive(hive)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err(format!("hive {} not found", hive.as_str()));
        }
        if self
            .store
            .get_entity(entity)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err(format!("entity {} not found", entity.as_str()));
        }
        let row = HiveParticipantRow {
            hive: hive.clone(),
            entity: entity.clone(),
            kind,
            role: role.to_owned(),
            joined_ms: now_ms,
        };
        self.store
            .add_hive_participant(&row)
            .map_err(|e| e.to_string())
    }

    pub fn remove_participant(&self, hive: &HiveId, entity: &EntityId) -> Result<(), String> {
        self.store
            .remove_hive_participant(hive, entity)
            .map_err(|e| e.to_string())
    }

    pub fn participants(&self, hive: &HiveId) -> Result<Vec<HiveParticipantRow>, String> {
        self.store
            .list_hive_participants(hive)
            .map_err(|e| e.to_string())
    }

    pub fn append_message(
        &self,
        hive: &HiveId,
        sender: &str,
        sender_kind: &str,
        content: &str,
        task_ref: Option<&str>,
        now_ms: u64,
    ) -> Result<HiveMessageRow, String> {
        if self
            .store
            .get_hive(hive)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err(format!("hive {} not found", hive.as_str()));
        }
        self.check_sender(hive, sender)?;
        let row = HiveMessageRow {
            id: HiveMessageId::generate(),
            hive: hive.clone(),
            sender: sender.to_owned(),
            sender_kind: sender_kind.to_owned(),
            content: content.to_owned(),
            task_ref: task_ref.map(str::to_owned),
            ts_ms: now_ms,
        };
        self.store
            .append_hive_message(&row)
            .map_err(|e| e.to_string())
    }

    /// A sender may speak only as a registered participant (matched against the
    /// participant entity's id, name or slug) or as `vikram`, the runtime's own
    /// voice that relays the user's prompt in `hive_run`.
    fn check_sender(&self, hive: &HiveId, sender: &str) -> Result<(), String> {
        if sender == "vikram" {
            return Ok(());
        }
        let participants = self
            .store
            .list_hive_participants(hive)
            .map_err(|e| e.to_string())?;
        for p in &participants {
            if let Ok(Some(entity)) = self.store.get_entity(&p.entity)
                && (sender == entity.id.as_str() || sender == entity.name || sender == entity.slug)
            {
                return Ok(());
            }
        }
        Err(format!(
            "sender `{sender}` is not a participant of hive {}",
            hive.as_str()
        ))
    }

    pub fn messages(&self, hive: &HiveId) -> Result<Vec<HiveMessageRow>, String> {
        self.store
            .list_hive_messages(hive)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_store::EntityKind;

    fn manager() -> HiveManager {
        HiveManager::new(Arc::new(Store::open_in_memory().unwrap()))
    }

    fn entity(name: &str, slug: &str) -> vikram_store::EntityRow {
        vikram_store::EntityRow {
            id: vikram_contracts::EntityId::generate(),
            owner_user: None,
            kind: EntityKind::Agent,
            name: name.to_owned(),
            slug: slug.to_owned(),
            title: String::new(),
            role: String::new(),
            specialization: String::new(),
            description: String::new(),
            icon: None,
            model: None,
            profile_id: None,
            status: "active".to_owned(),
            created_ms: 1,
            updated_ms: 1,
        }
    }

    fn hive_with_member(m: &HiveManager) -> (HiveId, vikram_store::EntityRow) {
        let store = m.store();
        let user = store.create_user("U", 1).unwrap();
        let hive = m
            .create(user.id, None, "Forge", "build", "cooperative", 1)
            .unwrap();
        let ent = entity("Rudra", "rudra");
        store.save_entity(&ent).unwrap();
        m.add_participant(&hive.id, &ent.id, EntityKind::Agent, "member", 1)
            .unwrap();
        (hive.id, ent)
    }

    #[test]
    fn update_state_errors_on_missing_hive() {
        let m = manager();
        let missing = HiveId::generate();
        assert!(m.update_state(&missing, "paused", 2).is_err());
        let (hive, _) = hive_with_member(&m);
        m.update_state(&hive, "paused", 2).unwrap();
        assert_eq!(m.get(&hive).unwrap().unwrap().state, "paused");
    }

    #[test]
    fn append_message_requires_a_registered_sender() {
        let m = manager();
        let (hive, ent) = hive_with_member(&m);
        assert!(
            m.append_message(&hive, "mallory", "agent", "hi", None, 2)
                .is_err()
        );
        // Slug, name and entity id all resolve to the participant.
        m.append_message(&hive, "rudra", "agent", "by slug", None, 2)
            .unwrap();
        m.append_message(&hive, "Rudra", "agent", "by name", None, 3)
            .unwrap();
        m.append_message(&hive, ent.id.as_str(), "agent", "by id", None, 4)
            .unwrap();
        // The runtime's own voice is allowed for relaying the user prompt.
        m.append_message(&hive, "vikram", "main_agent", "prompt", None, 5)
            .unwrap();
        assert_eq!(m.messages(&hive).unwrap().len(), 4);
        // Once removed, the same sender is rejected.
        m.remove_participant(&hive, &ent.id).unwrap();
        assert!(
            m.append_message(&hive, "rudra", "agent", "hi", None, 6)
                .is_err()
        );
    }
}
