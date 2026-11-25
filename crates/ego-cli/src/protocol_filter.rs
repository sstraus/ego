//! User-selected filter execution, bound to the same cancellation and deadline as generation.
use ego_ai::protocol_filter::{
    ChildFailure, MAX_ENVELOPE_BYTES, ProtocolFilterCommand, ProtocolFilterResponse,
};
use ego_core::config::ProtocolFilterConfig;
use ego_core::credentials::SecretMaterial;
use ego_core::turn::CancelToken;
use ego_tools::exec::{SecretChildError, SecretChildRequest};
use futures::future::BoxFuture;
use std::time::{Duration, Instant};

pub(crate) struct OsProtocolFilter {
    pub config: ProtocolFilterConfig,
    pub cancel: CancelToken,
    pub deadline: Option<Instant>,
}

impl ProtocolFilterCommand for OsProtocolFilter {
    fn open_response(&self) -> Result<Box<dyn ProtocolFilterResponse>, ChildFailure> {
        self.check_live()?;
        let child = ego_tools::exec::start_protocol_session(
            SecretChildRequest {
                argv: vec![self.config.command.as_os_str().to_owned()],
                cwd: None,
                stdin: None,
                cancel: self.cancel.clone(),
                run_deadline: self.deadline,
                command_timeout: Duration::from_millis(self.config.timeout_ms),
                termination_grace: Duration::from_millis(100),
            },
            MAX_ENVELOPE_BYTES,
        )
        .map_err(child_error)?;
        Ok(Box::new(OsResponseFilter { child }))
    }
    fn check_live(&self) -> Result<(), ChildFailure> {
        if self.cancel.is_cancelled() {
            return Err(ChildFailure::Cancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(ChildFailure::RunTimeout);
        }
        Ok(())
    }
}

fn child_error(error: SecretChildError) -> ChildFailure {
    match error {
        SecretChildError::Launch(_) => ChildFailure::Spawn,
        SecretChildError::Io(_) => ChildFailure::Io,
        SecretChildError::ProtocolFraming => ChildFailure::Framing,
        SecretChildError::ProtocolEarlyEof => ChildFailure::EarlyEof,
        SecretChildError::ProtocolOutputLimit => ChildFailure::OutputLimit,
        SecretChildError::InvalidRequest(_) => ChildFailure::InvalidResult,
        SecretChildError::Cancelled => ChildFailure::Cancelled,
        SecretChildError::RunTimeout => ChildFailure::RunTimeout,
        SecretChildError::CommandTimeout => ChildFailure::CommandTimeout,
        SecretChildError::Exit { .. } => ChildFailure::Exit,
        SecretChildError::CleanupUncertain { .. } => ChildFailure::CleanupUncertain,
    }
}

struct OsResponseFilter {
    child: ego_tools::exec::ProtocolChild,
}

impl ProtocolFilterResponse for OsResponseFilter {
    fn exchange(&mut self, input: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, ChildFailure>> {
        Box::pin(async move {
            self.child
                .exchange(SecretMaterial::new(input))
                .await
                .map(|output| output.expose_secret_bytes().to_vec())
                .map_err(child_error)
        })
    }

    fn finish(&mut self) -> BoxFuture<'_, Result<(), ChildFailure>> {
        Box::pin(async move { self.child.finish().await.map_err(child_error) })
    }
}

/// One policy path shared by MCP dispatch and lifecycle hook dispatch.
pub(crate) struct OsEgressFilter<'a> {
    pub journal: ego_core::turn::SharedJournal<'a>,
    pub source: std::sync::Arc<crate::model::CliGenerationModelSource>,
    pub cancel: CancelToken,
    pub deadline: Option<Instant>,
}

impl ego_core::egress::EgressFilter for OsEgressFilter<'_> {
    fn filter<'a>(
        &'a self,
        request: ego_core::egress::EgressRequest<'a>,
    ) -> BoxFuture<'a, Result<(), &'static str>> {
        Box::pin(async move {
            let (provider, model, config) = self.source.egress_filter_selection()?;
            let Some(config) = config else {
                return Ok(());
            };
            config.validate()?;
            let namespace = ego_ai::protocol_filter::namespace(&provider, &config)?;
            let (snapshot, latest_turn) = self.journal.egress_filter_snapshot(&namespace)?;
            let turn = request.turn.or(latest_turn).unwrap_or_default();
            let command = OsProtocolFilter {
                config,
                cancel: request
                    .scope
                    .map_or_else(|| self.cancel.clone(), |scope| scope.cancel_token()),
                deadline: request
                    .scope
                    .map(|scope| scope.deadline())
                    .or(self.deadline),
            };
            let (events, mut notices) = futures::channel::mpsc::unbounded();
            let result = ego_ai::protocol_filter::apply_egress(
                &command,
                (&provider, &model),
                &snapshot,
                request,
                &events,
            )
            .await;
            drop(events);
            use futures::StreamExt as _;
            while let Some(event) = notices.next().await {
                if let ego_core::generator::port::AiEvent::FilterNotice { text } = event {
                    self.journal
                        .publish(&ego_core::public::PublicEvent::FilterNotice { turn, text })
                        .map_err(|_| "protocol_filter local notice sink unavailable")?;
                }
            }
            result
        })
    }
}
