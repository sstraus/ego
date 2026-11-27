//! Human-owned root requests after a prepared workspace-boundary refusal.

use futures::future::BoxFuture;

use crate::event::id::{ItemId, SessionId, TurnId};
use crate::permit::{CanonPath, OpName, RootAccess};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootGrantRequest {
    /// Only a boundary refusal which can yield this pending turn has a continuation.
    pub continuation: Option<TurnId>,
    pub tool: String,
    pub operation: OpName,
    pub path: CanonPath,
    pub access: RootAccess,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootGrantChoice {
    Deny,
    Session,
    Permanent(crate::permit::RootConsent),
}

/// An interactive host owns the answer and the permanent config transaction.
/// Core journals pending intent before publication and commits only after durability.
pub trait RootGrantPort: Send + Sync {
    /// The host can rebuild all effect owners and continue this same turn.
    /// Saving authority alone is not permission to widen a live executor.
    fn supports_readmission(&self) -> bool {
        false
    }

    fn ask<'a>(
        &'a self,
        session: SessionId,
        item: ItemId,
        request: RootGrantRequest,
    ) -> BoxFuture<'a, Result<RootGrantChoice, String>>;

    /// Publish the approved config revision only after core persists its pending receipt.
    fn publish<'a>(
        &'a self,
        consent: &'a crate::permit::PermanentRootConsent,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// Process-local human approval of two disclosed measurements for one root call.
/// Deliberately neither Clone nor serializable: durable root consent is not
/// reusable trust in a different backend. Taking this value consumes authority,
/// including when re-admission rejects a stale or mismatched continuation.
#[derive(Debug)]
pub struct RootTransitionApproval {
    session: SessionId,
    turn: TurnId,
    item: ItemId,
    root: crate::permit::ProfileRoot,
    previous: crate::permit::ExecObservation,
    proposed: crate::permit::ExecObservation,
}

impl RootTransitionApproval {
    /// Prepare a validated candidate; the host must publish it only after the
    /// human approves these exact measurements.
    pub fn approved(
        session: SessionId,
        item: ItemId,
        request: &RootGrantRequest,
        previous: crate::permit::ExecObservation,
        proposed: crate::permit::ExecObservation,
    ) -> Result<Self, String> {
        let turn = request
            .continuation
            .ok_or("root question has no same-turn continuation")?;
        let root = crate::permit::ProfileRoot::new(request.path.clone(), request.access);
        if !proposed.root_transition_is_sufficient(&previous, std::slice::from_ref(&root)) {
            return Err(
                "root transition has missing policy or insufficient measured guarantees".into(),
            );
        }
        Ok(Self {
            session,
            turn,
            item,
            root,
            previous,
            proposed,
        })
    }

    pub fn item(&self) -> ItemId {
        self.item
    }

    pub fn root(&self) -> &crate::permit::ProfileRoot {
        &self.root
    }

    pub fn consume(
        self,
        session: SessionId,
        continuation: Option<TurnId>,
        action: Option<(TurnId, ItemId)>,
        previous: &crate::permit::ExecObservation,
        observed: &crate::permit::ExecObservation,
        approved_roots: &[crate::permit::ProfileRoot],
    ) -> Result<(), String> {
        // Report only static categories, never the bound authority values.
        // These predicates are pure; retain every co-failure in its fixed order.
        let mismatches = [
            (self.session != session, "session"),
            (continuation != Some(self.turn), "continuation"),
            (action != Some((self.turn, self.item)), "action"),
            (!approved_roots.contains(&self.root), "approved-root"),
            (self.previous != *previous, "previous-observation"),
            (self.proposed != *observed, "proposed-observation"),
            (
                !observed.root_transition_is_sufficient(previous, std::slice::from_ref(&self.root)),
                "insufficient-transition",
            ),
        ]
        .into_iter()
        .filter_map(|(mismatch, label)| mismatch.then_some(label))
        .collect::<Vec<_>>();
        if !mismatches.is_empty() {
            return Err(format!(
                "approved root/backend transition is stale, foreign, or changed; authority consumed ({})",
                mismatches.join(",")
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permit::{
        BackendIdentity, Confinement, ExecCaps, ExecObservation, ExecutionPolicy, PolicyDigest,
        ProfileRoot, RunConfinement,
    };

    fn workspace() -> CanonPath {
        CanonPath::new(if cfg!(windows) {
            r"C:\workspace"
        } else {
            "/workspace"
        })
    }

    fn observations(request: &RootGrantRequest) -> (ExecObservation, ExecObservation) {
        let base = RunConfinement::new(Confinement::Workspace, false, workspace()).unwrap();
        let caps = Confinement::Workspace.required_caps();
        let observation = |policy: &RunConfinement, backend, digit: &str| {
            ExecObservation::new(
                BackendIdentity::new(backend, "1"),
                caps,
                PolicyDigest::try_from(digit.repeat(64)).unwrap(),
            )
            .with_policy(ExecutionPolicy::capture(policy, Vec::new()))
        };
        let previous = observation(&base, "bwrap", "1");
        let proposed = observation(
            &base.with_roots(vec![ProfileRoot::new(request.path.clone(), request.access)]),
            "landlock",
            "2",
        );
        (previous, proposed)
    }

    fn request() -> RootGrantRequest {
        RootGrantRequest {
            continuation: Some(TurnId::new()),
            tool: "read".into(),
            operation: OpName::Read,
            path: CanonPath::new("/outside/exact"),
            access: RootAccess::Read,
        }
    }

    // Catches: a one-action approval becoming backend equivalence, crossing
    // session/action/turn boundaries, or excusing a fresh measurement's drift.
    #[test]
    fn disclosed_root_transition_rejects_stale_foreign_or_drifted_measurements() {
        let request = request();
        let session = SessionId::new();
        let item = ItemId::new();
        let (previous, proposed) = observations(&request);
        let roots = vec![ProfileRoot::new(request.path.clone(), request.access)];
        let approval = || {
            RootTransitionApproval::approved(
                session,
                item,
                &request,
                previous.clone(),
                proposed.clone(),
            )
            .unwrap()
        };
        assert!(
            !proposed.not_more_permissive_than_with_roots(&previous, &roots),
            "ordinary root consent must still reject a different backend"
        );
        assert!(
            approval()
                .consume(
                    session,
                    Some(request.continuation.unwrap()),
                    Some((request.continuation.unwrap(), item)),
                    &previous,
                    &proposed,
                    &roots
                )
                .is_ok()
        );
        for (name, owner, continuation, action, approved) in [
            (
                "foreign session",
                SessionId::new(),
                Some(request.continuation.unwrap()),
                Some((request.continuation.unwrap(), item)),
                roots.clone(),
            ),
            (
                "stale turn",
                session,
                Some(TurnId::new()),
                Some((request.continuation.unwrap(), item)),
                roots.clone(),
            ),
            (
                "missing continuation",
                session,
                None,
                Some((request.continuation.unwrap(), item)),
                roots.clone(),
            ),
            (
                "foreign action",
                session,
                Some(request.continuation.unwrap()),
                Some((request.continuation.unwrap(), ItemId::new())),
                roots.clone(),
            ),
            (
                "missing action",
                session,
                Some(request.continuation.unwrap()),
                None,
                roots.clone(),
            ),
            (
                "revoked root",
                session,
                Some(request.continuation.unwrap()),
                Some((request.continuation.unwrap(), item)),
                Vec::new(),
            ),
        ] {
            let label = match name {
                "foreign session" => "session",
                "stale turn" | "missing continuation" => "continuation",
                "foreign action" | "missing action" => "action",
                "revoked root" => "approved-root",
                _ => unreachable!(),
            };
            assert_eq!(
                approval()
                    .consume(owner, continuation, action, &previous, &proposed, &approved)
                    .unwrap_err(),
                format!(
                    "approved root/backend transition is stale, foreign, or changed; authority consumed ({label})"
                ),
                "{name}"
            );
        }
        let mutate = |observation: &ExecObservation, field: &str, value: serde_json::Value| {
            let mut json = serde_json::to_value(observation).unwrap();
            if let Some((outer, inner)) = field.split_once('.') {
                json[outer][inner] = value;
            } else {
                json[field] = value;
            }
            serde_json::from_value::<ExecObservation>(json).unwrap()
        };
        for (name, changed) in [
            (
                "backend",
                mutate(&proposed, "backend.name", serde_json::json!("other")),
            ),
            (
                "revision",
                mutate(&proposed, "backend.revision", serde_json::json!("2")),
            ),
            (
                "capabilities",
                mutate(
                    &proposed,
                    "capabilities",
                    serde_json::to_value(proposed.capabilities().union(ExecCaps::NO_IP_NETWORK))
                        .unwrap(),
                ),
            ),
            (
                "digest",
                mutate(
                    &proposed,
                    "policy_digest",
                    serde_json::json!("3".repeat(64)),
                ),
            ),
            (
                "policy",
                mutate(
                    &proposed,
                    "policy.workspace",
                    serde_json::json!("/different"),
                ),
            ),
            (
                "missing policy",
                mutate(&proposed, "policy", serde_json::Value::Null),
            ),
        ] {
            let labels = match name {
                "policy" | "missing policy" => "proposed-observation,insufficient-transition",
                _ => "proposed-observation",
            };
            assert_eq!(
                approval()
                    .consume(
                        session,
                        Some(request.continuation.unwrap()),
                        Some((request.continuation.unwrap(), item)),
                        &previous,
                        &changed,
                        &roots
                    )
                    .unwrap_err(),
                format!(
                    "approved root/backend transition is stale, foreign, or changed; authority consumed ({labels})"
                ),
                "{name}"
            );
        }
        let old_drift = mutate(&previous, "backend.revision", serde_json::json!("2"));
        assert_eq!(
            approval()
                .consume(
                    session,
                    Some(request.continuation.unwrap()),
                    Some((request.continuation.unwrap(), item)),
                    &old_drift,
                    &proposed,
                    &roots
                )
                .unwrap_err(),
            "approved root/backend transition is stale, foreign, or changed; authority consumed (previous-observation)"
        );
        // Catches a first-failure diagnostic hiding simultaneous foreign bindings.
        assert_eq!(
            approval()
                .consume(SessionId::new(), None, None, &old_drift, &proposed, &[])
                .unwrap_err(),
            "approved root/backend transition is stale, foreign, or changed; authority consumed (session,continuation,action,approved-root,previous-observation)"
        );
    }

    // Catches: a question promising a transition with unknown policy, lost
    // confinement guarantees, or an undisclosed sibling/execute/write widening.
    #[test]
    fn disclosed_root_transition_refuses_missing_or_insufficient_measurement() {
        let request = request();
        let (previous, proposed) = observations(&request);
        let mut unbound = request.clone();
        unbound.continuation = None;
        assert!(
            RootTransitionApproval::approved(
                SessionId::new(),
                ItemId::new(),
                &unbound,
                previous.clone(),
                proposed.clone()
            )
            .is_err()
        );
        let unknown = ExecObservation::new(
            previous.backend().clone(),
            previous.capabilities(),
            previous.policy_digest().clone(),
        );
        let insufficient = ExecObservation::new(
            proposed.backend().clone(),
            ExecCaps::empty(),
            proposed.policy_digest().clone(),
        )
        .with_policy(ExecutionPolicy::capture(
            &RunConfinement::new(Confinement::Workspace, false, workspace()).unwrap(),
            Vec::new(),
        ));
        // Catches loss of the final sufficiency check without weakening approved().
        // Only this module can construct an approval with invalid bound measurements.
        let session = SessionId::new();
        let item = ItemId::new();
        let root = ProfileRoot::new(request.path.clone(), request.access);
        let invalid = RootTransitionApproval {
            session,
            turn: request.continuation.unwrap(),
            item,
            root: root.clone(),
            previous: previous.clone(),
            proposed: insufficient.clone(),
        };
        assert_eq!(
            invalid
                .consume(
                    session,
                    request.continuation,
                    Some((request.continuation.unwrap(), item)),
                    &previous,
                    &insufficient,
                    &[root]
                )
                .unwrap_err(),
            "approved root/backend transition is stale, foreign, or changed; authority consumed (insufficient-transition)"
        );
        for (old, new) in [
            (unknown.clone(), proposed.clone()),
            (previous.clone(), unknown),
            (previous.clone(), insufficient),
        ] {
            assert!(
                RootTransitionApproval::approved(
                    SessionId::new(),
                    ItemId::new(),
                    &request,
                    old,
                    new
                )
                .is_err()
            );
        }
        let base = RunConfinement::new(Confinement::Workspace, false, workspace()).unwrap();
        for policy in [
            base.clone().with_roots(vec![ProfileRoot::new(
                CanonPath::new("/outside/sibling"),
                RootAccess::Read,
            )]),
            base.clone().with_roots(vec![ProfileRoot::new(
                request.path.clone(),
                RootAccess::ReadWrite,
            )]),
            base.with_exec_paths(vec![CanonPath::new("/outside/exec")]),
        ] {
            let new = ExecObservation::new(
                proposed.backend().clone(),
                proposed.capabilities(),
                proposed.policy_digest().clone(),
            )
            .with_policy(ExecutionPolicy::capture(&policy, Vec::new()));
            assert!(
                RootTransitionApproval::approved(
                    SessionId::new(),
                    ItemId::new(),
                    &request,
                    previous.clone(),
                    new
                )
                .is_err()
            );
        }
    }
}
