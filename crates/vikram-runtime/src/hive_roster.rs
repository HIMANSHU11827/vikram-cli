//! In-memory roster management with JSON persistence.

use crate::config::vikram_home;
use crate::types::now_ms;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use vikram_contracts::BotId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiveRoster {
    pub name: String,
    pub members: Vec<BotId>,
    pub created_ms: u64,
    pub task: Option<String>,
    /// Persisted identity. Old `hive-roster.json` files without it get a deterministic id
    /// derived from the name on load (`id_from_name`); new rosters generate one.
    #[serde(default = "default_roster_id", deserialize_with = "de_roster_id")]
    pub id: vikram_contracts::HiveRosterId,
}

fn default_roster_id() -> vikram_contracts::HiveRosterId {
    vikram_contracts::HiveRosterId::new("hrost-legacy")
}

fn de_roster_id<'de, D>(d: D) -> std::result::Result<vikram_contracts::HiveRosterId, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Old `hive-roster.json` files have no persisted identity; a JSON string value (if a
    // newer file carries one) is accepted and wrapped, anything else falls back
    // to the deterministic legacy id.
    match Option::<String>::deserialize(d)? {
        Some(name) if !name.is_empty() => Ok(vikram_contracts::HiveRosterId::new(name)),
        _ => Ok(vikram_contracts::HiveRosterId::new("hrost-legacy")),
    }
}

impl HiveRoster {
    pub fn id_from_name(name: &str) -> vikram_contracts::HiveRosterId {
        // Deterministic legacy id: stable for a given name so snapshots don't churn
        // across restarts for pre-id files.
        vikram_contracts::HiveRosterId::new(format!("hrost-{name}"))
    }
}

#[derive(Debug)]
pub struct HiveRosterManager {
    rosters: Mutex<Vec<HiveRoster>>,
    path: Option<PathBuf>,
}

impl Default for HiveRosterManager {
    fn default() -> Self {
        Self::new()
    }
}

impl HiveRosterManager {
    pub fn new() -> Self {
        Self {
            rosters: Mutex::new(Vec::new()),
            path: None,
        }
    }

    pub fn load() -> Self {
        let path = vikram_home().join("hive-roster.json");
        // One-time carry-over: a pre-rename `rosters.json` is still read so existing rosters survive.
        if !path.exists() {
            let legacy = vikram_home().join("rosters.json");
            if legacy.exists() {
                return Self::load_from(legacy);
            }
        }
        Self::load_from(path)
    }

    pub fn load_from(path: PathBuf) -> Self {
        let mgr = Self {
            rosters: Mutex::new(Vec::new()),
            path: Some(path),
        };
        mgr.reload();
        mgr
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self {
            rosters: Mutex::new(Vec::new()),
            path: Some(path),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn reload(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(data) = fs::read_to_string(path) else {
            return;
        };
        if let Ok(mut rosters) = serde_json::from_str::<Vec<HiveRoster>>(&data) {
            for t in &mut rosters {
                if t.id.as_str() == "hrost-legacy" {
                    t.id = HiveRoster::id_from_name(&t.name);
                }
            }
            *self.rosters.lock().expect("rosters lock") = rosters;
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let rosters = self.rosters.lock().expect("rosters lock").clone();
        let data = serde_json::to_string_pretty(&rosters)
            .map_err(|e| format!("serialize rosters: {e}"))?;
        fs::write(path, data).map_err(|e| format!("write rosters: {e}"))?;
        Ok(())
    }

    pub fn create(&self, name: impl Into<String>) -> Result<(), String> {
        let name = name.into();
        {
            let mut rosters = self.rosters.lock().expect("rosters lock");
            if rosters.iter().any(|t| t.name == name) {
                return Err(format!("roster already exists: {name}"));
            }
            rosters.push(HiveRoster {
                name: name.clone(),
                members: Vec::new(),
                created_ms: now_ms(),
                task: None,
                id: vikram_contracts::HiveRosterId::generate(),
            });
        }
        self.save()
    }

    pub fn list(&self) -> Vec<HiveRoster> {
        self.rosters.lock().expect("rosters lock").clone()
    }

    pub fn get(&self, name: impl Into<String>) -> Option<HiveRoster> {
        let name = name.into();
        self.rosters
            .lock()
            .expect("rosters lock")
            .iter()
            .find(|t| t.name == name)
            .cloned()
    }

    pub fn add_member(
        &self,
        roster_name: impl Into<String>,
        agent_id: BotId,
    ) -> Result<(), String> {
        let roster_name = roster_name.into();
        {
            let mut rosters = self.rosters.lock().expect("rosters lock");
            let roster = rosters
                .iter_mut()
                .find(|t| t.name == roster_name)
                .ok_or_else(|| format!("hive not found: {roster_name}"))?;
            if !roster.members.contains(&agent_id) {
                roster.members.push(agent_id);
            }
        }
        self.save()
    }

    pub fn remove_member(
        &self,
        roster_name: impl Into<String>,
        agent_id: BotId,
    ) -> Result<(), String> {
        let roster_name = roster_name.into();
        {
            let mut rosters = self.rosters.lock().expect("rosters lock");
            let roster = rosters
                .iter_mut()
                .find(|t| t.name == roster_name)
                .ok_or_else(|| format!("hive not found: {roster_name}"))?;
            let before = roster.members.len();
            roster.members.retain(|m| *m != agent_id);
            if roster.members.len() == before {
                return Err(format!("member not in roster: {agent_id}"));
            }
        }
        self.save()
    }

    pub fn delete(&self, name: impl Into<String>) -> Result<(), String> {
        let name = name.into();
        {
            let mut rosters = self.rosters.lock().expect("rosters lock");
            let before = rosters.len();
            rosters.retain(|t| t.name != name);
            if rosters.len() == before {
                return Err(format!("roster not found: {name}"));
            }
        }
        self.save()
    }

    pub fn delegate_task(
        &self,
        roster_name: impl Into<String>,
        task: impl Into<String>,
        assigned_to: Option<BotId>,
    ) -> Result<(), String> {
        let roster_name = roster_name.into();
        let task = task.into();
        {
            let mut rosters = self.rosters.lock().expect("rosters lock");
            let roster = rosters
                .iter_mut()
                .find(|t| t.name == roster_name)
                .ok_or_else(|| format!("hive not found: {roster_name}"))?;
            if let Some(agent) = &assigned_to
                && !roster.members.contains(agent)
            {
                return Err(format!("agent is not a member of roster: {agent}"));
            }
            roster.task = Some(task);
        }
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::BotId;

    #[test]
    fn create_and_list() {
        let mgr = HiveRosterManager::new();
        mgr.create("alpha").unwrap();
        let rosters = mgr.list();
        assert_eq!(rosters.len(), 1);
        assert_eq!(rosters[0].name, "alpha");
        assert!(rosters[0].members.is_empty());
        assert!(rosters[0].task.is_none());
    }

    #[test]
    fn duplicate_name_rejected() {
        let mgr = HiveRosterManager::new();
        mgr.create("alpha").unwrap();
        assert!(mgr.create("alpha").is_err());
    }

    #[test]
    fn add_and_remove_member() {
        let mgr = HiveRosterManager::new();
        mgr.create("alpha").unwrap();
        let agent = BotId::new("a1");
        mgr.add_member("alpha", agent.clone()).unwrap();
        assert_eq!(mgr.get("alpha").unwrap().members, vec![agent.clone()]);
        mgr.remove_member("alpha", agent.clone()).unwrap();
        assert!(mgr.get("alpha").unwrap().members.is_empty());
    }

    #[test]
    fn remove_missing_member_errors() {
        let mgr = HiveRosterManager::new();
        mgr.create("alpha").unwrap();
        let agent = BotId::new("a1");
        assert!(mgr.remove_member("alpha", agent).is_err());
    }

    #[test]
    fn delete_hive_roster() {
        let mgr = HiveRosterManager::new();
        mgr.create("alpha").unwrap();
        mgr.delete("alpha").unwrap();
        assert!(mgr.get("alpha").is_none());
        assert!(mgr.delete("alpha").is_err());
    }

    #[test]
    fn delegate_stores_task() {
        let mgr = HiveRosterManager::new();
        mgr.create("alpha").unwrap();
        let agent = BotId::new("a1");
        mgr.add_member("alpha", agent.clone()).unwrap();
        mgr.delegate_task("alpha", "do work", Some(agent)).unwrap();
        assert_eq!(mgr.get("alpha").unwrap().task, Some("do work".to_string()));
    }

    #[test]
    fn delegate_rejects_non_member_assignee() {
        let mgr = HiveRosterManager::new();
        mgr.create("alpha").unwrap();
        let agent = BotId::new("a1");
        assert!(mgr.delegate_task("alpha", "do work", Some(agent)).is_err());
    }

    #[test]
    fn delegate_without_assignee() {
        let mgr = HiveRosterManager::new();
        mgr.create("alpha").unwrap();
        mgr.delegate_task("alpha", "do work", None).unwrap();
        assert_eq!(mgr.get("alpha").unwrap().task, Some("do work".to_string()));
    }

    #[test]
    fn persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hive-roster.json");
        let mgr = HiveRosterManager::load_from(path.clone());
        mgr.create("alpha").unwrap();
        let agent = BotId::new("a1");
        mgr.add_member("alpha", agent.clone()).unwrap();
        mgr.delegate_task("alpha", "task", Some(agent)).unwrap();

        let mgr2 = HiveRosterManager::load_from(path);
        assert_eq!(mgr2.list().len(), 1);
        let t = mgr2.get("alpha").unwrap();
        assert_eq!(t.members.len(), 1);
        assert_eq!(t.task, Some("task".to_string()));
    }
}
