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
        if let Some(mut row) = self.store.get_hive(id).map_err(|e| e.to_string())? {
            row.state = state.to_owned();
            row.updated_ms = now_ms;
            self.store.update_hive(&row).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn add_participant(
        &self,
        hive: &HiveId,
        entity: &EntityId,
        kind: vikram_store::EntityKind,
        role: &str,
        now_ms: u64,
    ) -> Result<(), String> {
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

    pub fn messages(&self, hive: &HiveId) -> Result<Vec<HiveMessageRow>, String> {
        self.store
            .list_hive_messages(hive)
            .map_err(|e| e.to_string())
    }
}
