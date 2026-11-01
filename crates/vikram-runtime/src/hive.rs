//! HIVE teams.
//!
//! A team is a user-named group of saved agent profiles that shares one dedicated conversation.
//! The team owns no execution machinery of its own: it works exactly when its conversation holds
//! an active turn, and an explicit pause request suspends that turn between steps. Everything an
//! interface shows is read from this runtime state — never from a timer, and never shared between
//! teams: each team's activity is independent.

use std::sync::{Arc, Mutex};

use vikram_contracts::{AgentId, SessionId, TeamId};
use vikram_store::TeamRow;

use crate::types::TeamSnapshot;

pub struct ManagedTeam {
    pub id: TeamId,
    pub name: Mutex<String>,
    pub members: Mutex<Vec<AgentId>>,
    pub session: Mutex<Option<SessionId>>,
    /// Set by an explicit pause request and cleared by resume or stop. Transient by design: a
    /// restart never restores a pause, because nothing is actually running after a restart.
    pub paused: Mutex<bool>,
    pub created_ms: u64,
}

impl ManagedTeam {
    pub fn from_row(row: TeamRow) -> Self {
        Self {
            id: row.id,
            name: Mutex::new(row.name),
            members: Mutex::new(Vec::new()),
            session: Mutex::new(row.session_id),
            paused: Mutex::new(false),
            created_ms: row.created_ms,
        }
    }

    pub fn name(&self) -> String {
        self.name.lock().expect("team name").clone()
    }

    pub fn set_name(&self, name: &str) {
        *self.name.lock().expect("team name") = name.to_owned();
    }

    pub fn members(&self) -> Vec<AgentId> {
        self.members.lock().expect("team members").clone()
    }

    /// `false` when the agent already belongs to the team.
    pub fn add_member(&self, agent: AgentId) -> bool {
        let mut members = self.members.lock().expect("team members");
        if members.contains(&agent) {
            return false;
        }
        members.push(agent);
        true
    }

    /// `false` when the agent was not a member.
    pub fn remove_member(&self, agent: &AgentId) -> bool {
        let mut members = self.members.lock().expect("team members");
        let before = members.len();
        members.retain(|member| member != agent);
        members.len() != before
    }

    pub fn session(&self) -> Option<SessionId> {
        self.session.lock().expect("team session").clone()
    }

    pub fn set_session(&self, session: Option<SessionId>) {
        *self.session.lock().expect("team session") = session;
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.lock().expect("team pause")
    }

    pub fn set_paused(&self, paused: bool) {
        *self.paused.lock().expect("team pause") = paused;
    }

    /// `working` is computed by the caller from the live conversation state, so the snapshot can
    /// never claim activity the runtime does not have.
    pub fn snapshot(&self, working: bool) -> TeamSnapshot {
        TeamSnapshot {
            id: self.id.clone(),
            name: self.name(),
            members: self.members(),
            session: self.session(),
            paused: self.is_paused(),
            working,
        }
    }
}

/// The roster of teams. Like `AgentManager`, it owns profiles only — execution lives in the
/// sessions and turn tasks the runtime already runs.
#[derive(Debug, Default)]
pub struct TeamManager {
    teams: Mutex<Vec<Arc<ManagedTeam>>>,
}

impl TeamManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&self, teams: Vec<Arc<ManagedTeam>>) {
        *self.teams.lock().expect("teams lock") = teams;
    }

    pub fn list(&self) -> Vec<Arc<ManagedTeam>> {
        self.teams.lock().expect("teams lock").clone()
    }

    pub fn get(&self, id: &TeamId) -> Option<Arc<ManagedTeam>> {
        self.list().into_iter().find(|t| t.id == *id)
    }

    pub fn add(&self, team: Arc<ManagedTeam>) {
        self.teams.lock().expect("teams lock").push(team);
    }

    pub fn remove(&self, id: &TeamId) -> bool {
        let mut teams = self.teams.lock().expect("teams lock");
        let before = teams.len();
        teams.retain(|t| t.id != *id);
        teams.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(name: &str) -> Arc<ManagedTeam> {
        Arc::new(ManagedTeam::from_row(TeamRow {
            id: TeamId::generate(),
            name: name.into(),
            session_id: None,
            created_ms: 0,
        }))
    }

    #[test]
    fn a_new_team_is_neither_paused_nor_working() {
        let team = team("Forge");
        let snapshot = team.snapshot(false);
        assert_eq!(snapshot.name, "Forge");
        assert!(!snapshot.paused);
        assert!(!snapshot.working);
        assert!(snapshot.members.is_empty());
    }

    #[test]
    fn membership_is_unique_and_removal_reports_change() {
        let team = team("Nexus");
        let agent = AgentId::generate();
        assert!(team.add_member(agent.clone()));
        assert!(!team.add_member(agent.clone()));
        assert_eq!(team.members(), vec![agent.clone()]);
        assert!(team.remove_member(&agent));
        assert!(!team.remove_member(&agent));
        assert!(team.members().is_empty());
    }

    #[test]
    fn pause_is_transient_and_independent_per_team() {
        let a = team("A");
        let b = team("B");
        a.set_paused(true);
        assert!(a.is_paused());
        assert!(!b.is_paused());
        a.set_paused(false);
        assert!(!a.is_paused());
    }

    #[test]
    fn the_manager_removes_teams_by_id() {
        let manager = TeamManager::new();
        let a = team("A");
        let b = team("B");
        manager.add(Arc::clone(&a));
        manager.add(Arc::clone(&b));
        assert_eq!(manager.list().len(), 2);
        assert!(manager.remove(&a.id));
        assert!(!manager.remove(&a.id));
        assert_eq!(manager.list().len(), 1);
        assert_eq!(manager.get(&b.id).unwrap().name(), "B");
    }

    #[test]
    fn a_team_keeps_its_session_link_across_snapshotting() {
        let team = team("Phoenix");
        let session = SessionId::generate();
        team.set_session(Some(session.clone()));
        let snapshot = team.snapshot(true);
        assert_eq!(snapshot.session.as_ref(), Some(&session));
        assert!(snapshot.working);
    }
}
