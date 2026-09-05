//! Composition of provider HTTP authority with one run's OS effect ports.

use std::sync::Arc;
use std::time::Instant;

use ego_ai::http::{Http, HttpCall, HttpProfileFingerprint, PreparedHttpCall};
use ego_core::event::id::SessionId;
use ego_core::permit::{
    CanonPath, Confinement, ExecObservation, GuardedHttp, RunConfinement, TransportError,
    TransportFailure,
};
use ego_core::suspension::ContinuationJournal;
use ego_core::turn::CancelToken;
use ego_core::turn::McpRegistryJournal;
use ego_mcp::{
    MCP_DEFAULT_MAX_SSE_EVENT_SIZE, MCP_DEFAULT_OPERATION_TIMEOUT, McpDuplexEffectsFactory,
    McpHttpEffects, McpRegistryController, McpRegistryRuntime,
};
use ego_tools::clock::SystemClock;
use ego_tools::credentials::{OsGenerationPolicyCaptureStore, OsResumeKey};
use ego_tools::exec::{ExecLimits, ExecRunConfig, HttpCallTimeout, ProcessError, RunEffectPorts};
use ego_tools::http::SystemResolver;
use ego_tools::mcp::{RunMcpDuplexEffectsFactory, RunMcpHttpEffects};
use ego_tools::mcp_continuation::JsonlContinuationJournal;
use ego_tools::mcp_registry::JsonlMcpRegistryJournal;
use ego_tools::store::{OsIo, SessionEvidenceStore};
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
                .execute_admitted(route, connect, request, deadline)
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
    ports: Arc<RunEffectPorts>,
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
        Self::confined(confinement, cancel, None)
            .await
            .map_err(|error| error.to_string())
    }

    async fn confined(
        confinement: RunConfinement,
        cancel: CancelToken,
        run_deadline: Option<Instant>,
    ) -> Result<Self, McpProcessScopeError> {
        let evidence_directory = tempfile::tempdir().map_err(|error| {
            McpProcessScopeError::Setup(format!("could not create the MCP evidence store: {error}"))
        })?;
        let evidence = SessionEvidenceStore::open(
            evidence_directory.path(),
            SessionId::new(),
            COMMAND_EVIDENCE_CAPACITY,
        )
        .map_err(|error| {
            McpProcessScopeError::Setup(format!("could not open the MCP evidence store: {error}"))
        })?;
        let (ports, _) = run_effect_ports(confinement, evidence, cancel, run_deadline)
            .await
            .map_err(McpProcessScopeError::Effects)?;
        let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(&ports))) as Arc<dyn Http>;
        Ok(Self {
            _evidence_directory: evidence_directory,
            ports,
            http,
        })
    }

    pub(crate) fn http(&self) -> &Arc<dyn Http> {
        &self.http
    }

    /// The same complete command-lifetime effect authority, exposed for
    /// foreign protocol adapters that implement their own consumer ports.
    pub(crate) fn ports(&self) -> &Arc<RunEffectPorts> {
        &self.ports
    }
}

pub(crate) enum McpProcessScopeError {
    Effects(ProcessError),
    Setup(String),
}

impl std::fmt::Display for McpProcessScopeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Effects(error) => std::fmt::Display::fmt(error, formatter),
            Self::Setup(message) => formatter.write_str(message),
        }
    }
}

/// One command- or invocation-lifetime MCP authority and its guarded effects.
/// The effect scope is retained beside the runtime so neither its evidence
/// directory nor guarded process/network handles can disappear first.
pub struct McpProcessScope {
    _effects: CommandHttpScope,
    runtime: Arc<McpRegistryRuntime>,
}

impl McpProcessScope {
    pub async fn online(cancel: CancelToken) -> Result<Self, String> {
        let effects = CommandHttpScope::online(cancel).await?;
        Self::from_effects(effects)
    }

    pub(crate) async fn for_run(
        confinement: RunConfinement,
        cancel: CancelToken,
        run_deadline: Option<Instant>,
    ) -> Result<Self, McpProcessScopeError> {
        let effects = CommandHttpScope::confined(confinement, cancel, run_deadline).await?;
        Self::from_effects(effects).map_err(McpProcessScopeError::Setup)
    }

    fn from_effects(effects: CommandHttpScope) -> Result<Self, String> {
        let root = crate::config::ego_home()
            .ok_or_else(|| "MCP needs EGO_HOME or HOME for its machine journals".to_owned())?
            .join("mcp");
        let journal = Arc::new(JsonlMcpRegistryJournal::new(&root, Box::new(OsIo)))
            as Arc<dyn McpRegistryJournal>;
        let continuation_journal =
            Arc::new(JsonlContinuationJournal::open(&root).map_err(|error| {
                format!("MCP continuation journal could not be opened: {error}")
            })?) as Arc<dyn ContinuationJournal>;
        let http = Arc::new(RunMcpHttpEffects::new(Arc::clone(effects.ports())))
            as Arc<dyn McpHttpEffects>;
        let stdio = Arc::new(RunMcpDuplexEffectsFactory::new(
            effects.ports().duplex_handle(),
        )) as Arc<dyn McpDuplexEffectsFactory>;
        let runtime = Arc::new(
            McpRegistryRuntime::new(
                journal,
                continuation_journal,
                Arc::new(OsGenerationPolicyCaptureStore::default()),
                http,
                stdio,
                Arc::new(OsResumeKey::open(&root)),
                Arc::new(SystemClock),
                MCP_DEFAULT_MAX_SSE_EVENT_SIZE,
                MCP_DEFAULT_OPERATION_TIMEOUT,
            )
            .map_err(|error| format!("MCP registry authority could not be opened: {error}"))?,
        );
        Ok(Self {
            _effects: effects,
            runtime,
        })
    }

    pub fn runtime(&self) -> &Arc<McpRegistryRuntime> {
        &self.runtime
    }

    pub fn controller(&self) -> &Arc<McpRegistryController> {
        self.runtime.controller()
    }
}
