//! User-selected filter execution, bound to the same cancellation and deadline as generation.
use ego_ai::protocol_filter::{MAX_ENVELOPE_BYTES, ProtocolFilterCommand, ProtocolFilterResponse};
use ego_core::config::ProtocolFilterConfig;
use ego_core::credentials::SecretMaterial;
use ego_core::turn::CancelToken;
use ego_tools::exec::{SecretChildError, SecretChildRequest, run_protocol_child};
use futures::future::BoxFuture;
use std::time::{Duration, Instant};

pub(crate) struct OsProtocolFilter {
    pub config: ProtocolFilterConfig,
    pub cancel: CancelToken,
    pub deadline: Option<Instant>,
}

impl ProtocolFilterCommand for OsProtocolFilter {
    fn open_response(&self) -> Result<Box<dyn ProtocolFilterResponse>, &'static str> {
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
    fn check_live(&self) -> Result<(), &'static str> {
        if self.cancel.is_cancelled() {
            return Err("protocol_filter cancelled");
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err("protocol_filter run deadline expired");
        }
        Ok(())
    }
    fn run(&self, input: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, &'static str>> {
        Box::pin(async move {
            self.config.validate()?;
            let output = run_protocol_child(
                SecretChildRequest {
                    argv: vec![self.config.command.as_os_str().to_owned()],
                    cwd: None,
                    stdin: Some(SecretMaterial::new(input)),
                    cancel: self.cancel.clone(),
                    run_deadline: self.deadline,
                    command_timeout: Duration::from_millis(self.config.timeout_ms),
                    termination_grace: Duration::from_millis(100),
                },
                MAX_ENVELOPE_BYTES,
            )
            .await
            .map_err(child_error)?;
            // A concurrently cancelled command must never publish a provider request.
            self.check_live()?;
            Ok(output.stdout().expose_secret_bytes().to_vec())
        })
    }
}

fn child_error(error: SecretChildError) -> &'static str {
    match error {
        SecretChildError::Cancelled => "protocol_filter cancelled",
        SecretChildError::RunTimeout => "protocol_filter run deadline expired",
        SecretChildError::CommandTimeout => "protocol_filter timed out",
        SecretChildError::Exit { .. } => "protocol_filter exited unsuccessfully",
        SecretChildError::CleanupUncertain { .. } => {
            "protocol_filter process-tree cleanup uncertain"
        }
        _ => "protocol_filter child exchange failed (check executable and output limits)",
    }
}

struct OsResponseFilter {
    child: ego_tools::exec::ProtocolChild,
}

impl ProtocolFilterResponse for OsResponseFilter {
    fn exchange(&mut self, input: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, &'static str>> {
        Box::pin(async move {
            self.child
                .exchange(SecretMaterial::new(input))
                .await
                .map(|output| output.expose_secret_bytes().to_vec())
                .map_err(child_error)
        })
    }

    fn finish(&mut self) -> BoxFuture<'_, Result<(), &'static str>> {
        Box::pin(async move { self.child.finish().await.map_err(child_error) })
    }
}
