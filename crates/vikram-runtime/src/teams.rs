//! In-memory team management with JSON persistence.

use crate::config::vikram_home;
use crate::types::now_ms;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use vikram_contracts::AgentId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub name: String,
    pub members: Vec<AgentId>,
    pub created_ms: u64,
    pub task: Option<String>,
}

#[derive(Debug)]
pub struct TeamManager {
    teams: Mutex<Vec<Team>>,
    path: Option<PathBuf>,
}

impl Default for TeamManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TeamManager {
    pub fn new() -> Self {
        Self {
            teams: Mutex::new(Vec::new()),
            path: None,
        }
    }

    pub fn load() -> Self {
        Self::load_from(vikram_home().join("teams.json"))
    }

    pub fn load_from(path: PathBuf) -> Self {
        let mgr = Self {
            teams: Mutex::new(Vec::new()),
            path: Some(path),
        };
        mgr.reload();
        mgr
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self {
            teams: Mutex::new(Vec::new()),
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
        if let Ok(teams) = serde_json::from_str::<Vec<Team>>(&data) {
            *self.teams.lock().expect("teams lock") = teams;
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let teams = self.teams.lock().expect("teams lock").clone();
        let data =
            serde_json::to_string_pretty(&teams).map_err(|e| format!("serialize teams: {e}"))?;
        fs::write(path, data).map_err(|e| format!("write teams: {e}"))?;
        Ok(())
    }

    pub fn create(&self, name: impl Into<String>) -> Result<(), String> {
        let name = name.into();
        {
            let mut teams = self.teams.lock().expect("teams lock");
            if teams.iter().any(|t| t.name == name) {
                return Err(format!("team already exists: {name}"));
            }
            teams.push(Team {
                name: name.clone(),
                members: Vec::new(),
                created_ms: now_ms(),
                task: None,
            });
        }
        self.save()
    }

    pub fn list(&self) -> Vec<Team> {
        self.teams.lock().expect("teams lock").clone()
    }

    pub fn get(&self, name: impl Into<String>) -> Option<Team> {
        let name = name.into();
        self.teams
            .lock()
            .expect("teams lock")
            .iter()
            .find(|t| t.name == name)
            .cloned()
    }

    pub fn add_member(
        &self,
        team_name: impl Into<String>,
        agent_id: AgentId,
    ) -> Result<(), String> {
        let team_name = team_name.into();
        {
            let mut teams = self.teams.lock().expect("teams lock");
            let team = teams
                .iter_mut()
                .find(|t| t.name == team_name)
                .ok_or_else(|| format!("team not found: {team_name}"))?;
            if !team.members.contains(&agent_id) {
                team.members.push(agent_id);
            }
        }
        self.save()
    }

    pub fn remove_member(
        &self,
        team_name: impl Into<String>,
        agent_id: AgentId,
    ) -> Result<(), String> {
        let team_name = team_name.into();
        {
            let mut teams = self.teams.lock().expect("teams lock");
            let team = teams
                .iter_mut()
                .find(|t| t.name == team_name)
                .ok_or_else(|| format!("team not found: {team_name}"))?;
            let before = team.members.len();
            team.members.retain(|m| *m != agent_id);
            if team.members.len() == before {
                return Err(format!("member not in team: {agent_id}"));
            }
        }
        self.save()
    }

    pub fn delete(&self, name: impl Into<String>) -> Result<(), String> {
        let name = name.into();
        {
            let mut teams = self.teams.lock().expect("teams lock");
            let before = teams.len();
            teams.retain(|t| t.name != name);
            if teams.len() == before {
                return Err(format!("team not found: {name}"));
            }
        }
        self.save()
    }

    pub fn delegate_task(
        &self,
        team_name: impl Into<String>,
        task: impl Into<String>,
        assigned_to: Option<AgentId>,
    ) -> Result<(), String> {
        let team_name = team_name.into();
        let task = task.into();
        {
            let mut teams = self.teams.lock().expect("teams lock");
            let team = teams
                .iter_mut()
                .find(|t| t.name == team_name)
                .ok_or_else(|| format!("team not found: {team_name}"))?;
            if let Some(agent) = &assigned_to {
                if !team.members.contains(agent) {
                    return Err(format!("agent is not a member of team: {agent}"));
                }
            }
            team.task = Some(task);
        }
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::AgentId;

    #[test]
    fn create_and_list() {
        let mgr = TeamManager::new();
        mgr.create("alpha").unwrap();
        let teams = mgr.list();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].name, "alpha");
        assert!(teams[0].members.is_empty());
        assert!(teams[0].task.is_none());
    }

    #[test]
    fn duplicate_name_rejected() {
        let mgr = TeamManager::new();
        mgr.create("alpha").unwrap();
        assert!(mgr.create("alpha").is_err());
    }

    #[test]
    fn add_and_remove_member() {
        let mgr = TeamManager::new();
        mgr.create("alpha").unwrap();
        let agent = AgentId::new("a1");
        mgr.add_member("alpha", agent.clone()).unwrap();
        assert_eq!(mgr.get("alpha").unwrap().members, vec![agent.clone()]);
        mgr.remove_member("alpha", agent.clone()).unwrap();
        assert!(mgr.get("alpha").unwrap().members.is_empty());
    }

    #[test]
    fn remove_missing_member_errors() {
        let mgr = TeamManager::new();
        mgr.create("alpha").unwrap();
        let agent = AgentId::new("a1");
        assert!(mgr.remove_member("alpha", agent).is_err());
    }

    #[test]
    fn delete_team() {
        let mgr = TeamManager::new();
        mgr.create("alpha").unwrap();
        mgr.delete("alpha").unwrap();
        assert!(mgr.get("alpha").is_none());
        assert!(mgr.delete("alpha").is_err());
    }

    #[test]
    fn delegate_stores_task() {
        let mgr = TeamManager::new();
        mgr.create("alpha").unwrap();
        let agent = AgentId::new("a1");
        mgr.add_member("alpha", agent.clone()).unwrap();
        mgr.delegate_task("alpha", "do work", Some(agent)).unwrap();
        assert_eq!(mgr.get("alpha").unwrap().task, Some("do work".to_string()));
    }

    #[test]
    fn delegate_rejects_non_member_assignee() {
        let mgr = TeamManager::new();
        mgr.create("alpha").unwrap();
        let agent = AgentId::new("a1");
        assert!(mgr.delegate_task("alpha", "do work", Some(agent)).is_err());
    }

    #[test]
    fn delegate_without_assignee() {
        let mgr = TeamManager::new();
        mgr.create("alpha").unwrap();
        mgr.delegate_task("alpha", "do work", None).unwrap();
        assert_eq!(mgr.get("alpha").unwrap().task, Some("do work".to_string()));
    }

    #[test]
    fn persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("teams.json");
        let mgr = TeamManager::load_from(path.clone());
        mgr.create("alpha").unwrap();
        let agent = AgentId::new("a1");
        mgr.add_member("alpha", agent.clone()).unwrap();
        mgr.delegate_task("alpha", "task", Some(agent)).unwrap();

        let mgr2 = TeamManager::load_from(path);
        assert_eq!(mgr2.list().len(), 1);
        let t = mgr2.get("alpha").unwrap();
        assert_eq!(t.members.len(), 1);
        assert_eq!(t.task, Some("task".to_string()));
    }
}
