//! Grants: the authority an agent actually holds at this moment.
//!
//! Operations receive a read-only [`GrantSnapshot`] and there is no path from a snapshot back to
//! the live set. Widening requires an [`ApprovalTicket`], which only [`ApprovalBroker`] can build
//! and which no code outside this crate's host boundary can fabricate. That is the structural
//! version of "an agent must never grant itself permissions by generating model output".

use vikram_contracts::{ActionKind, ApprovalId, PermissionRequest, VikramError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub action: ActionKind,
    /// `*` for any resource, otherwise a path or URL prefix.
    pub scope: String,
}

impl Grant {
    pub fn new(action: ActionKind, scope: impl Into<String>) -> Self {
        Self {
            action,
            scope: scope.into(),
        }
    }

    fn covers(&self, request: &PermissionRequest) -> bool {
        self.action == request.action
            && (self.scope == "*"
                || request.resource == self.scope
                || request.resource.starts_with(&format!("{}/", self.scope)))
    }
}

/// Immutable view handed to operations. Deliberately has no mutators.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantSnapshot {
    grants: Vec<Grant>,
    delegation_depth: u32,
}

impl GrantSnapshot {
    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    pub fn delegation_depth(&self) -> u32 {
        self.delegation_depth
    }

    pub fn permits(&self, request: &PermissionRequest) -> bool {
        self.grants.iter().any(|g| g.covers(request))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantSet {
    grants: Vec<Grant>,
    delegation_depth: u32,
}

impl GrantSet {
    pub fn new(grants: Vec<Grant>, delegation_depth: u32) -> Self {
        Self {
            grants,
            delegation_depth,
        }
    }

    pub fn snapshot(&self) -> GrantSnapshot {
        GrantSnapshot {
            grants: self.grants.clone(),
            delegation_depth: self.delegation_depth,
        }
    }

    /// Removing authority always works; that direction is never dangerous.
    pub fn narrow(&mut self, action: ActionKind, scope: impl Into<String>) {
        let scope = scope.into();
        self.grants
            .retain(|g| !(g.action == action && g.scope == scope));
    }

    /// The only way in. Held by the host, not by operations or plugins. The grant must be inside
    /// the scope the approved ticket carries; anything wider is refused, never truncated.
    pub fn widen(&mut self, ticket: &ApprovalTicket, grant: Grant) -> Result<(), VikramError> {
        if ticket.grant_within(grant.action, grant.scope.clone()).is_none() {
            return Err(VikramError::InvalidState(format!(
                "grant `{:?}` {} exceeds the approved scope `{:?}` {}",
                grant.action,
                grant.scope,
                ticket.request().action,
                ticket.request().resource,
            )));
        }
        if !self.grants.contains(&grant) {
            self.grants.push(grant);
        }
        Ok(())
    }
}

/// Proof a human approved something. Private field means it cannot be constructed outside this
/// crate, so `GrantSet::widen` cannot be called by anything that also writes the model's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalTicket {
    pub id: ApprovalId,
    request: PermissionRequest,
    approved_by: ApprovalSource,
    _seal: (),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalSource {
    User,
    PolicyAuto,
}

/// The host's approval authority. In production this is fed by the terminal's approval prompt or
/// by a pre-agreed unattended ruleset; neither path goes through a model.
pub struct ApprovalBroker {
    issued: Vec<ApprovalTicket>,
}

impl ApprovalBroker {
    pub fn new() -> Self {
        Self { issued: Vec::new() }
    }

    /// A policy-generated approval of model- or tool-authored content is refused outright; a human
    /// reading the prompt and approving is what issues a ticket.
    pub fn issue(
        &mut self,
        request: PermissionRequest,
        source: ApprovalSource,
    ) -> Option<&ApprovalTicket> {
        if source == ApprovalSource::PolicyAuto && request.originating_trust.is_untrusted() {
            return None;
        }
        self.issued.push(ApprovalTicket {
            id: ApprovalId::generate(),
            request,
            approved_by: source,
            _seal: (),
        });
        self.issued.last()
    }

    pub fn issued_count(&self) -> usize {
        self.issued.len()
    }

    /// A referred approval by id, e.g. when a caller wants to know what was approved before
    /// widening with it.
    pub fn get(&self, id: &ApprovalId) -> Option<&ApprovalTicket> {
        self.issued.iter().find(|t| &t.id == id)
    }

    /// Revocation. The ticket stops being evidence: a later widen with it fails.
    pub fn revoke(&mut self, id: &ApprovalId) -> bool {
        let before = self.issued.len();
        self.issued.retain(|t| &t.id != id);
        self.issued.len() != before
    }
}

impl Default for ApprovalBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalTicket {
    pub fn id(&self) -> &ApprovalId {
        &self.id
    }

    pub fn request(&self) -> &PermissionRequest {
        &self.request
    }

    pub fn approved_by(&self) -> ApprovalSource {
        self.approved_by
    }

    /// Grants may never exceed the scope that was actually approved. Containment is on path
    /// boundaries: `/work/etc` may be granted under a `/work` approval; `/etc` may not.
    pub fn grant_within(&self, action: ActionKind, scope: impl Into<String>) -> Option<Grant> {
        let scope = scope.into();
        let approved = &self.request.resource;
        let within = approved == "*"
            || scope == *approved
            || scope
                .get(approved.len()..)
                .is_some_and(|rest| rest.starts_with('/') && scope.starts_with(approved.as_str()));
        if self.request.action == action && within {
            Some(Grant::new(action, scope))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::{SideEffect, Trust};

    fn req(action: ActionKind, resource: &str, trust: Trust) -> PermissionRequest {
        PermissionRequest::new(
            action,
            "fs.write",
            resource,
            SideEffect::LocalMutation,
            trust,
        )
    }

    #[test]
    fn a_snapshot_without_the_grant_denies_and_with_it_allows() {
        let mut set = GrantSet::new(vec![], 0);
        let before = set.snapshot();
        assert!(!before.permits(&req(ActionKind::Write, "/work/a.txt", Trust::User)));

        let mut broker = ApprovalBroker::new();
        let ticket = broker
            .issue(
                req(ActionKind::Write, "/work", Trust::User),
                ApprovalSource::User,
            )
            .expect("user approval issues a ticket");
        let grant = ticket
            .grant_within(ActionKind::Write, "/work")
            .expect("grant within approved scope");
        set.widen(ticket, grant).expect("ticket covers this grant");

        assert!(
            set.snapshot()
                .permits(&req(ActionKind::Write, "/work/a.txt", Trust::User))
        );
        assert!(
            !set.snapshot()
                .permits(&req(ActionKind::Write, "/etc/passwd", Trust::User)),
            "a /work grant must not cover /etc"
        );
    }

    #[test]
    fn narrowing_removes_authority_and_needs_no_ticket() {
        let mut set = GrantSet::new(vec![Grant::new(ActionKind::Write, "/work")], 0);
        assert!(
            set.snapshot()
                .permits(&req(ActionKind::Write, "/work/a", Trust::User))
        );
        set.narrow(ActionKind::Write, "/work");
        assert!(
            !set.snapshot()
                .permits(&req(ActionKind::Write, "/work/a", Trust::User))
        );
    }

    #[test]
    fn an_auto_approval_of_model_authored_content_is_refused() {
        let mut broker = ApprovalBroker::new();
        let ticket = broker.issue(
            req(ActionKind::Write, "/work", Trust::Model),
            ApprovalSource::PolicyAuto,
        );
        assert!(ticket.is_none(), "untrusted content must not self-approve");
        assert_eq!(broker.issued_count(), 0);
    }

    #[test]
    fn a_grant_cannot_exceed_the_scope_that_was_approved() {
        let mut broker = ApprovalBroker::new();
        let ticket = broker
            .issue(
                req(ActionKind::Write, "/work/proj", Trust::User),
                ApprovalSource::User,
            )
            .expect("ticket");
        assert!(
            ticket
                .grant_within(ActionKind::Write, "/work/proj")
                .is_some()
        );
        assert!(ticket.grant_within(ActionKind::Write, "/").is_none());
        assert!(
            ticket
                .grant_within(ActionKind::Admin, "/work/proj")
                .is_none(),
            "a write approval must not authorise admin"
        );
    }

    #[test]
    fn widening_with_a_grant_outside_the_approved_scope_is_refused() {
        let mut set = GrantSet::new(vec![], 0);
        let mut broker = ApprovalBroker::new();
        let ticket = broker
            .issue(req(ActionKind::Write, "/work", Trust::User), ApprovalSource::User)
            .expect("user approval");
        let err = set
            .widen(&ticket, Grant::new(ActionKind::Write, "/etc"))
            .expect_err("out-of-scope grant must be refused");
        assert!(!err.to_string().contains("/work/grant"), "{err}");
        assert!(
            !set.snapshot().permits(&req(ActionKind::Write, "/etc/passwd", Trust::User)),
            "nothing may have been added"
        );
    }

    #[test]
    fn a_revoked_ticket_is_no_longer_evidence() {
        let mut broker = ApprovalBroker::new();
        let request = req(ActionKind::Write, "/work", Trust::User);
        let ticket = broker.issue(request, ApprovalSource::User).expect("issued");
        let id = ticket.id().clone();
        let ticket = ticket.clone();
        assert!(broker.get(&id).is_some());
        assert_eq!(broker.revoke(&id), true);
        assert!(broker.get(&id).is_none(), "revoked tickets must not be returned");
        assert_eq!(broker.revoke(&id), false, "double revoke is a no-op");

        let mut set = GrantSet::new(vec![], 0);
        assert!(
            set.widen(&ticket, Grant::new(ActionKind::Write, "/work")).is_ok(),
            "an in-flight ticket still proves what was approved"
        );
    }

    #[test]
    fn a_ticket_id_is_generated_and_stable() {
        let mut broker = ApprovalBroker::new();
        let t1 = broker
            .issue(req(ActionKind::Read, "/work", Trust::User), ApprovalSource::User)
            .unwrap()
            .id()
            .clone();
        assert!(!t1.as_str().is_empty());
        let t2 = broker
            .issue(req(ActionKind::Read, "/docs", Trust::User), ApprovalSource::User)
            .unwrap()
            .id()
            .clone();
        assert_ne!(t1, t2, "each approval is separately addressable");
    }

    #[test]
    fn wildcard_scope_covers_every_resource_for_that_action() {
        let set = GrantSet::new(vec![Grant::new(ActionKind::Read, "*")], 0);
        assert!(
            set.snapshot()
                .permits(&req(ActionKind::Read, "/anything/here", Trust::External))
        );
        assert!(
            !set.snapshot()
                .permits(&req(ActionKind::Write, "/anything/here", Trust::User))
        );
    }

    #[test]
    fn snapshot_is_detached_from_later_widening() {
        let mut set = GrantSet::new(vec![], 1);
        let stale = set.snapshot();
        let mut broker = ApprovalBroker::new();
        let ticket = broker
            .issue(
                req(ActionKind::Write, "/work", Trust::User),
                ApprovalSource::User,
            )
            .unwrap();
        set
            .widen(
                ticket,
                Grant::new(ActionKind::Write, "/work"),
            )
            .expect("ticket covers this grant");
        assert_eq!(stale.delegation_depth(), 1);
        assert!(
            !stale.permits(&req(ActionKind::Write, "/work/a", Trust::User)),
            "an operation must not gain authority from a snapshot it already holds"
        );
        assert!(
            set.snapshot()
                .permits(&req(ActionKind::Write, "/work/a", Trust::User))
        );
    }
}
