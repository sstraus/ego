//! Continuation re-admission shares the run transaction and human permission seat.

use super::*;
use crate::permit::{CanonPath, PermissionRequest};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PolicyReadmission {
    previous: ExecObservation,
    approved_by_human: bool,
}

pub(super) fn valid_transition(
    old: &RunSelections,
    current: &RunSelections,
    command: &RunCommand,
) -> bool {
    if old == current {
        return true;
    }
    if !matches!(command, RunCommand::CommandAdmitted { start: false, .. }) {
        return false;
    }
    let Some(readmission) = current.sandbox_readmission.as_ref() else {
        return false;
    };
    if current.sandbox_observation.is_none() {
        return false;
    }
    let mut unchanged = current.clone();
    unchanged.sandbox_observation = old.sandbox_observation.clone();
    unchanged.sandbox_readmission = old.sandbox_readmission.clone();
    unchanged == *old && old.sandbox_observation.as_ref() == Some(&readmission.previous)
}

impl<S: SessionStore, E: RunExecutor, C: Clock, I: IdSource> RunController<S, E, C, I> {
    pub(super) async fn readmit_policy(
        &self,
        snapshot: &RunSnapshot,
        prepared: &mut E::Prepared,
        bindings: &RunExecutionBindings,
    ) -> Result<RunSelections, RunError> {
        let selections = &snapshot.selections;
        let Some(old) = selections.sandbox_observation.as_ref() else {
            return Ok(selections.clone());
        };
        let Some((observed, probe_evidence)) = self
            .executor
            .execution_observation(prepared, bindings.cancel())
            .await?
        else {
            return Err(RunExecutorError::new("execution authority is not measurable").into());
        };
        if bindings.cancel().is_cancelled() {
            return Err(RunExecutorError::new("execution observation cancelled").into());
        }
        if observed.same_measurement(old) {
            self.executor
                .request_execution_policy_permission(
                    prepared,
                    (observed, probe_evidence),
                    None,
                    bindings,
                )
                .await?;
            return Ok(selections.clone());
        }
        let automatic = observed.not_more_permissive_than(old);
        let message = format!(
            "{} {}",
            observed.changes_from(old),
            if automatic {
                "Re-admitting under equal or narrower authority."
            } else {
                "Approval is required: current authority may be wider."
            }
        );
        bindings
            .sink_factory
            .execution_policy_notice(&message)
            .map_err(RunExecutorError::new)?;
        let question = (!automatic).then(|| {
            PermissionRequest::execution_policy_readmission(
                message,
                CanonPath::new(snapshot.root.as_path()),
            )
        });
        let allowed = self
            .executor
            .request_execution_policy_permission(
                prepared,
                (observed.clone(), probe_evidence),
                question,
                bindings,
            )
            .await?;
        if !allowed {
            return Err(RunExecutorError::new(
                "execution policy re-admission was denied; no effects started",
            )
            .into());
        }
        let mut updated = selections.clone();
        updated.sandbox_readmission = Some(Box::new(PolicyReadmission {
            previous: old.clone(),
            approved_by_human: !automatic,
        }));
        updated.sandbox_observation = Some(observed);
        Ok(updated)
    }
}
