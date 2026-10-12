//! Re-admission events share the existing session writer lease.
use super::*;

impl CliRunExecutor {
    pub(super) async fn journal_readmission(
        &self,
        prepared: &mut CliPrepared,
        (observation, probe_evidence): ego_core::run::RunExecutionObservation,
        question: Option<ego_core::permit::PermissionRequest>,
        bindings: &RunExecutionBindings,
    ) -> Result<bool, RunExecutorError> {
        let clock = SystemClock;
        let opened = session::open(prepared.store.as_ref(), &prepared.session_id, &clock)
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        let mut journal = Journal::new(
            prepared.session_id,
            &opened.writer,
            &clock,
            &ego_core::sink::NullSink,
            ego_core::event::id::Producer::Main,
            opened.state,
        );
        let turn = ego_core::event::id::TurnId::new();
        journal
            .append(
                turn,
                None,
                None,
                ego_core::event::domain::EventKind::ExecutionObserved {
                    observation,
                    probe_evidence,
                },
            )
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        let Some(question) = question else {
            return Ok(true);
        };
        let item = ego_core::event::id::ItemId::new();
        let requested = journal
            .append(
                turn,
                Some(item),
                None,
                ego_core::event::domain::EventKind::PermissionRequested {
                    tool: question.tool().to_owned(),
                    escalation: question.why().clone(),
                },
            )
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        let decision = match bindings.human() {
            Some(human) => Some(human.decide(item, &question).await),
            None => None,
        };
        let allowed = matches!(decision, Some(ego_core::permit::PermissionDecision::Allow));
        journal
            .append(
                turn,
                Some(item),
                Some(requested),
                ego_core::event::domain::EventKind::PermissionDecided {
                    allowed: Some(allowed),
                    by: decision.map(|_| ego_core::permit::Authority::Party),
                    reason: Some(question.to_string()),
                },
            )
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        Ok(allowed)
    }
}
