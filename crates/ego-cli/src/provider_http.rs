//! Composition of provider HTTP authority with one run's OS effect ports.

use std::sync::Arc;
use std::time::Instant;

use ego_ai::http::{Http, HttpCall, HttpProfileFingerprint, HttpWebSocket, PreparedHttpCall};
use ego_core::event::id::SessionId;
use ego_core::permit::residual::ResidualWriteDesk;
use ego_core::permit::{
    CanonPath, Confinement, ExecObservation, GuardedHttp, RunConfinement, SandboxGrantPort,
    TransportError, TransportFailure,
};
use ego_core::suspension::ContinuationJournal;
use ego_core::turn::CancelToken;
use ego_core::turn::McpRegistryJournal;
use ego_mcp::{
    MCP_DEFAULT_MAX_SSE_EVENT_SIZE, MCP_DEFAULT_OPERATION_TIMEOUT, McpDuplexEffectsFactory,
    McpHttpEffects, McpRegistryController, McpRegistryRuntime, McpStdioSession,
};
use ego_tools::clock::SystemClock;
use ego_tools::credentials::{OsGenerationPolicyCaptureStore, OsResumeKey};
use ego_tools::exec::{ExecLimits, ExecRunConfig, HttpCallTimeout, ProcessError, RunEffectPorts};
use ego_tools::http::{InferenceHttpPorts, SystemResolver};
use ego_tools::mcp::{RunMcpDuplexEffectsFactory, RunMcpHttpEffects};
use ego_tools::mcp_continuation::JsonlContinuationJournal;
use ego_tools::mcp_registry::JsonlMcpRegistryJournal;
use ego_tools::store::{OsIo, SessionEvidenceStore};
use futures::future::BoxFuture;

const COMMAND_EVIDENCE_CAPACITY: u64 = 64 * 1024 * 1024;

/// The CLI-owned adapter joining provider authority to one run's raw ports.
pub struct ProviderHttpAdapter {
    authority: ProviderHttpAuthority,
}

#[derive(Clone)]
enum ProviderHttpAuthority {
    Run(Arc<RunEffectPorts>),
    Inference(Arc<InferenceHttpPorts>),
}

impl ProviderHttpAuthority {
    fn begin_call(&self) -> Instant {
        match self {
            Self::Run(ports) => ports.begin_http_call(),
            Self::Inference(ports) => ports.begin_call(),
        }
    }
    fn guarded(&self) -> GuardedHttp {
        match self {
            Self::Run(ports) => GuardedHttp::new(ports.resolver_handle(), ports.one_hop_handle()),
            Self::Inference(ports) => {
                GuardedHttp::new(ports.resolver_handle(), ports.one_hop_handle())
            }
        }
    }
}

impl ProviderHttpAdapter {
    pub fn new(ports: Arc<RunEffectPorts>) -> Self {
        Self {
            authority: ProviderHttpAuthority::Run(ports),
        }
    }

    pub fn inference(ports: Arc<InferenceHttpPorts>) -> Self {
        Self {
            authority: ProviderHttpAuthority::Inference(ports),
        }
    }
}

impl Http for ProviderHttpAdapter {
    fn open_websocket(
        &self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<Box<dyn HttpWebSocket>, TransportError>> {
        let (profile, route, connect, request) = call.into_parts();
        let authority = self.authority.clone();
        let deadline = authority.begin_call();
        Box::pin(async move {
            let socket = authority
                .guarded()
                .open_websocket_admitted(route.clone(), connect, request, deadline)
                .await?;
            Ok(Box::new(ProviderWebSocket {
                authority,
                profile,
                route,
                socket,
                deadline,
            }) as Box<dyn HttpWebSocket>)
        })
    }

    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(ProviderHttpCall {
            deadline: self.authority.begin_call(),
            authority: self.authority.clone(),
            profile: None,
        })
    }
}

struct ProviderWebSocket {
    authority: ProviderHttpAuthority,
    profile: HttpProfileFingerprint,
    route: ego_core::permit::HttpRoute,
    socket: Box<dyn ego_core::permit::WebSocketSession>,
    deadline: Instant,
}

impl HttpWebSocket for ProviderWebSocket {
    fn send(&mut self, call: PreparedHttpCall) -> BoxFuture<'_, Result<(), TransportError>> {
        let (profile, route, connect, request) = call.into_parts();
        if profile != self.profile || route != self.route {
            return Box::pin(futures::future::ready(Err(
                TransportError::definitely_unsent(TransportFailure::AuthorityMismatch),
            )));
        }
        self.deadline = self.authority.begin_call();
        self.socket.send(connect, request, self.deadline)
    }

    fn receive(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        self.socket.receive(self.deadline)
    }
}

struct ProviderHttpCall {
    authority: ProviderHttpAuthority,
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

        let authority = self.authority.clone();
        let deadline = self.deadline;
        Box::pin(async move {
            authority
                .guarded()
                .execute_admitted(route, connect, request, deadline)
                .await
        })
    }
}

/// Build the complete effect-port owner for one cancellation scope.
///
/// A supplied observation is admission truth: `for_run` compares its single
/// fresh measurement before it constructs any of the returned ports.
pub(crate) async fn run_effect_ports(
    confinement: RunConfinement,
    evidence: SessionEvidenceStore,
    cancel: CancelToken,
    run_deadline: Option<Instant>,
    admitted_observation: Option<&ExecObservation>,
    sandbox_grants: Option<Arc<dyn SandboxGrantPort>>,
    residual: Option<Arc<ResidualWriteDesk>>,
) -> Result<
    (
        Arc<RunEffectPorts>,
        ExecObservation,
        Option<ego_core::store::evidence::EvidenceRef>,
    ),
    ProcessError,
> {
    let config = ExecRunConfig::new(
        confinement,
        evidence,
        cancel,
        ExecLimits::default(),
        HttpCallTimeout::default(),
        run_deadline,
        Arc::new(SystemResolver::new()),
    );
    let config = match admitted_observation {
        Some(observation) => config.with_admitted_observation(observation.clone()),
        None => config,
    };
    let config = match sandbox_grants {
        Some(grants) => config.with_sandbox_grants(grants),
        None => config,
    };
    let config = match residual {
        Some(desk) => config.with_residual_desk(desk),
        None => config,
    };
    let (ports, observation, probe_evidence) = RunEffectPorts::for_run(config).await?;
    Ok((Arc::new(ports), observation, probe_evidence))
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
        Self::confined(confinement, cancel, None, None)
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn confined(
        confinement: RunConfinement,
        cancel: CancelToken,
        run_deadline: Option<Instant>,
        admitted_observation: Option<&ExecObservation>,
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
        let (ports, _, _) = run_effect_ports(
            confinement,
            evidence,
            cancel,
            run_deadline,
            admitted_observation,
            None,
            None,
        )
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
    /// Present for a run: its stdio servers keep one child across tool calls.
    stdio: Option<Arc<McpStdioSession>>,
}

impl McpProcessScope {
    pub async fn online(cancel: CancelToken) -> Result<Self, String> {
        let effects = CommandHttpScope::online(cancel).await?;
        Self::from_effects(effects, false)
    }

    pub(crate) async fn for_run(
        confinement: RunConfinement,
        cancel: CancelToken,
        run_deadline: Option<Instant>,
    ) -> Result<Self, McpProcessScopeError> {
        let effects = CommandHttpScope::confined(confinement, cancel, run_deadline, None).await?;
        Self::from_effects(effects, true).map_err(McpProcessScopeError::Setup)
    }

    fn from_effects(effects: CommandHttpScope, retain_stdio: bool) -> Result<Self, String> {
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
        let one_shot = Arc::new(RunMcpDuplexEffectsFactory::new(
            effects.ports().duplex_handle(),
        ));
        // A stdio server may bind state to its process (tuic-bridge holds the
        // peer registration), so a run keeps one child per server like an ACP session.
        let process = effects.ports().process_handle();
        let session = process
            .confinement()
            .filter(|_| retain_stdio)
            .map(|confinement| {
                McpStdioSession::new(
                    SessionId::new(),
                    one_shot.clone(),
                    process.clone(),
                    confinement,
                )
            });
        let stdio = match &session {
            Some(session) => session.clone() as Arc<dyn McpDuplexEffectsFactory>,
            None => one_shot as Arc<dyn McpDuplexEffectsFactory>,
        };
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
            .map_err(|error| format!("the MCP server registry could not be opened: {error}"))?,
        );
        Ok(Self {
            _effects: effects,
            runtime,
            stdio: session,
        })
    }

    /// Reap every retained stdio child before the run's effect scope goes away.
    pub async fn close(&self) {
        if let Some(stdio) = &self.stdio {
            stdio.close().await;
        }
    }

    pub fn runtime(&self) -> &Arc<McpRegistryRuntime> {
        &self.runtime
    }

    pub fn controller(&self) -> &Arc<McpRegistryController> {
        self.runtime.controller()
    }
}
