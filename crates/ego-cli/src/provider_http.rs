//! Composition of provider HTTP authority with one run's OS effect ports.

use std::sync::Arc;
use std::time::Instant;

use ego_ai::http::{Http, HttpCall, HttpProfileFingerprint, PreparedHttpCall};
use ego_core::event::id::SessionId;
use ego_core::permit::{
    CanonPath, Confinement, ExecObservation, GuardedHttp, RunConfinement, TransportError,
    TransportFailure,
};
use ego_core::turn::CancelToken;
use ego_tools::exec::{ExecLimits, ExecRunConfig, HttpCallTimeout, ProcessError, RunEffectPorts};
use ego_tools::http::SystemResolver;
use ego_tools::store::SessionEvidenceStore;
use futures::future::BoxFuture;

const COMMAND_EVIDENCE_CAPACITY: u64 = 64 * 1024 * 1024;

/// The CLI-owned adapter joining provider authority to one run's raw ports.
pub struct ProviderHttpAdapter {
    ports: Arc<RunEffectPorts>,
}

impl ProviderHttpAdapter {
    pub fn new(ports: Arc<RunEffectPorts>) -> Self {
        Self { ports }
    }
}

impl Http for ProviderHttpAdapter {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(ProviderHttpCall {
            deadline: self.ports.begin_http_call(),
            ports: Arc::clone(&self.ports),
            profile: None,
        })
    }
}

struct ProviderHttpCall {
    ports: Arc<RunEffectPorts>,
    deadline: Instant,
    profile: Option<HttpProfileFingerprint>,
}

impl HttpCall for ProviderHttpCall {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<ego_core::permit::OneHopResponse, TransportError>> {
        let (profile, route, connect, request) = call.into_parts();
        if self
            .profile
            .as_ref()
            .is_some_and(|expected| expected != &profile)
        {
            return Box::pin(futures::future::ready(Err(
                TransportError::definitely_unsent(TransportFailure::AuthorityMismatch),
            )));
        }
        self.profile.get_or_insert(profile);

        let ports = Arc::clone(&self.ports);
        let deadline = self.deadline;
        Box::pin(async move {
            GuardedHttp::new(ports.resolver_handle(), ports.one_hop_handle())
                .execute_admitted(route, connect, request, Some(deadline))
                .await
        })
    }
}

/// Build the complete effect-port owner for one cancellation scope.
pub(crate) async fn run_effect_ports(
    confinement: RunConfinement,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    run_deadline: Option<Instant>,
) -> Result<(Arc<RunEffectPorts>, ExecObservation), ProcessError> {
    let config = ExecRunConfig::new(
        confinement,
        evidence,
        cancel,
        ExecLimits::default(),
        HttpCallTimeout::default(),
        run_deadline,
        Arc::new(SystemResolver::new()),
    );
    let (ports, observation) = RunEffectPorts::for_run(config).await?;
    Ok((Arc::new(ports), observation))
}

/// Complete lifetime owner for an online command that has no durable session.
pub(crate) struct CommandHttpScope {
    _evidence_directory: tempfile::TempDir,
    http: Arc<dyn Http>,
}

#[cfg(test)]
mod tests;

impl CommandHttpScope {
    pub(crate) async fn online(cancel: CancelToken) -> Result<Self, String> {
        let workspace = std::env::current_dir()
            .map_err(|error| format!("could not resolve the command workspace: {error}"))?;
        let confinement = RunConfinement::new(Confinement::Off, false, CanonPath::new(&workspace))
            .map_err(|error| error.to_string())?;
        let evidence_directory = tempfile::tempdir()
            .map_err(|error| format!("could not create the command evidence store: {error}"))?;
        let evidence = SessionEvidenceStore::open(
            evidence_directory.path(),
            SessionId::new(),
            COMMAND_EVIDENCE_CAPACITY,
        )
        .map_err(|error| format!("could not open the command evidence store: {error}"))?;
        let (ports, _) = run_effect_ports(confinement, evidence, cancel, None)
            .await
            .map_err(|error| error.to_string())?;
        let http = Arc::new(ProviderHttpAdapter::new(ports)) as Arc<dyn Http>;
        Ok(Self {
            _evidence_directory: evidence_directory,
            http,
        })
    }

    pub(crate) fn http(&self) -> &Arc<dyn Http> {
        &self.http
    }
}
