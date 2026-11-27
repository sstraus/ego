//! Configuration projection retains measured refusals beside configured defaults.

use std::sync::Arc;

use agent_client_protocol_schema::v1;
use ego_core::control::SessionControlState;
use ego_core::event::id::SessionId;

use super::SessionConfinementAdmission;

pub(super) fn options(
    state: &SessionControlState,
    admission: Option<&Arc<dyn SessionConfinementAdmission>>,
    session: SessionId,
) -> Vec<v1::SessionConfigOption> {
    let mut options = crate::controls::options(state);
    if let Some(current) = state.sandbox.current
        && !state.sandbox.available.contains(&current)
        && let Some(admission) = admission
        && let Err(refusal) = admission.observation(session, current)
        && let Some(sandbox) = options
            .iter_mut()
            .find(|option| option.id.0.as_ref() == crate::controls::SANDBOX)
    {
        let reason = refusal.to_string();
        let description = sandbox.description.get_or_insert_with(String::new);
        description.push_str(" The configured current value is unavailable: ");
        description.push_str(&reason);
        sandbox.meta = Some(serde_json::Map::from_iter([(
            "ego".to_owned(),
            serde_json::json!({"unavailableReason": reason}),
        )]));
    }
    options
}

pub(super) fn readiness(state: &str) -> v1::Meta {
    serde_json::Map::from_iter([(
        "ego".to_owned(),
        serde_json::json!({"configOptions": {"state": state}}),
    )])
}
