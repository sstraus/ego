//! `ego acp`: production composition for the inbound ACP adapter.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::{ConnectTo, Stdio};
use agent_client_protocol_schema::v1;
use ego_acp::auth::Credential;
use ego_acp::serve::{
    AcpRunBindingsBuilder, McpSessionAdmissionError, McpSessionAdmitter,
    SessionConfinementAdmission, SessionMcpAdmission,
};
use ego_core::config::{
    McpConfigProvenance, McpEnvironmentName, McpHttpAuth, McpServerConfig, McpServerName, Source,
};
use ego_core::control::SessionControls;
use ego_core::credentials::SecretMaterial;
use ego_core::endpoint::Admitted;
use ego_core::event::domain::CancelCause;
use ego_core::evidence::EvidenceAdmissionService;
use ego_core::mcp::McpRuntimeBinding;
use ego_core::permit::{CanonPath, Confinement, RunConfinement};
use ego_core::run::{RunController, RunRetention, RunTermination};
use ego_core::session_lifecycle::{SessionLifecycle, SessionRunLifecycle};
use ego_core::suspension::ContinuationJournal;
use ego_core::turn::{AdmittedToolSource, CancelToken};
use ego_mcp::{
    MCP_DEFAULT_MAX_SSE_EVENT_SIZE, MCP_DEFAULT_OPERATION_TIMEOUT, McpDuplexEffectsFactory,
    McpEnvironmentBinding, McpHeaderBinding, McpHttpEffects, McpRegistryCandidate,
    McpRegistryRefresh, McpRegistryRuntime, McpServerAdmission, McpTransportMaterial,
    RegistryScope, RegistryTrigger,
};
use ego_tools::clock::SystemClock;
use ego_tools::credentials::{OsGenerationPolicyCaptureStore, OsResumeKey};
use ego_tools::exec::{ExecLimits, RunEffectPorts};
use ego_tools::mcp::{RunMcpDuplexEffectsFactory, RunMcpHttpEffects};
use ego_tools::mcp_continuation::JsonlContinuationJournal;
use ego_tools::mcp_registry::JsonlMcpRegistryJournal;
use ego_tools::store::OsIo;
use futures::future::BoxFuture;

use crate::acp_confinement::CliSessionConfinementAdmission;
use crate::acp_controls::AcpSessionControls;
use crate::cli::AcpArgs;
use crate::confinement_diagnostics::Reporter;
use crate::host_ports::{CanonicalRoots, TokioExecutionSpawner, UuidIds};
use crate::provider_http::{CommandHttpScope, ProviderHttpAdapter};
use crate::{run, session};

struct AcpSessionMcpAdmission {
    runtime: Arc<McpRegistryRuntime>,
    _storage: tempfile::TempDir,
    admitted_tools: Arc<dyn AdmittedToolSource>,
    mcp_errors: Vec<String>,
}

impl AcpSessionMcpAdmission {
    fn from_runtime(runtime: Arc<McpRegistryRuntime>, storage: tempfile::TempDir) -> Self {
        let controller = runtime.controller();
        let admitted_tools = controller.admitted_tools();
        let mcp_errors = controller.status().mcp_errors();
        Self {
            runtime,
            _storage: storage,
            admitted_tools,
            mcp_errors,
        }
    }
}

impl SessionMcpAdmission for AcpSessionMcpAdmission {
    fn admitted_tools(&self) -> Arc<dyn AdmittedToolSource> {
        Arc::clone(&self.admitted_tools)
    }

    fn mcp_errors(&self) -> &[String] {
        &self.mcp_errors
    }

    fn mcp_runtime(&self) -> Arc<dyn McpRuntimeBinding> {
        self.runtime.clone()
    }
}

struct AcpMcpSessionAdmitter {
    http_management: Arc<dyn McpHttpEffects>,
    stdio_management: Arc<dyn McpDuplexEffectsFactory>,
    max_sse_event_size: usize,
    timeout: Duration,
}

impl AcpMcpSessionAdmitter {
    fn new(
        http_management: Arc<dyn McpHttpEffects>,
        stdio_management: Arc<dyn McpDuplexEffectsFactory>,
        max_sse_event_size: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            http_management,
            stdio_management,
            max_sse_event_size,
            timeout,
        }
    }
}

impl McpSessionAdmitter for AcpMcpSessionAdmitter {
    fn admit(
        &self,
        servers: Vec<v1::McpServer>,
    ) -> BoxFuture<'static, Result<Option<Arc<dyn SessionMcpAdmission>>, McpSessionAdmissionError>>
    {
        if servers.is_empty() {
            return Box::pin(async { Ok(None) });
        }
        let http_management = Arc::clone(&self.http_management);
        let stdio_management = Arc::clone(&self.stdio_management);
        let max_sse_event_size = self.max_sse_event_size;
        let timeout = self.timeout;
        Box::pin(async move {
            let admissions = map_mcp_servers(servers)?;
            let refresh = McpRegistryRefresh::new(
                admissions,
                RegistryTrigger::ConfigChanged,
                RegistryScope::All,
            )
            .map_err(|_| safe_mcp_error("the supplied MCP servers are invalid"))?;
            let storage = tempfile::tempdir()
                .map_err(|_| safe_mcp_error("ACP MCP session storage could not be created"))?;
            let registry_root = storage.path().join("registry");
            let continuation_root = storage.path().join("continuations");
            let registry_journal =
                Arc::new(JsonlMcpRegistryJournal::new(registry_root, Box::new(OsIo)));
            let continuation_journal: Arc<dyn ContinuationJournal> = Arc::new(
                JsonlContinuationJournal::open(continuation_root).map_err(|_| {
                    safe_mcp_error("ACP MCP continuation storage could not be opened")
                })?,
            );
            let runtime = Arc::new(
                McpRegistryRuntime::new(
                    registry_journal,
                    continuation_journal,
                    Arc::new(OsGenerationPolicyCaptureStore::default()),
                    http_management,
                    stdio_management,
                    Arc::new(OsResumeKey::open(storage.path())),
                    Arc::new(SystemClock),
                    max_sse_event_size,
                    timeout,
                )
                .map_err(|_| safe_mcp_error("ACP MCP runtime could not be created"))?,
            );
            runtime
                .controller()
                .reconcile(refresh)
                .await
                .map_err(|_| safe_mcp_error("the supplied MCP servers could not be admitted"))?;
            if !runtime.controller().status().mcp_errors().is_empty() {
                return Err(safe_mcp_error(
                    "the supplied MCP servers could not be admitted",
                ));
            }
            Ok(Some(
                Arc::new(AcpSessionMcpAdmission::from_runtime(runtime, storage))
                    as Arc<dyn SessionMcpAdmission>,
            ))
        })
    }
}

fn map_mcp_servers(
    servers: Vec<v1::McpServer>,
) -> Result<Vec<McpRegistryCandidate>, McpSessionAdmissionError> {
    servers
        .into_iter()
        .map(|server| {
            let (name, config, material) = match server {
                v1::McpServer::Stdio(server) => {
                    let name = server_name(&server.name)?;
                    if !server.command.is_absolute() {
                        return Err(safe_mcp_error(
                            "an ACP stdio MCP command must be an absolute path",
                        ));
                    }
                    let command = server.command.into_os_string().into_string().map_err(|_| {
                        safe_mcp_error("an ACP stdio MCP command is not valid Unicode")
                    })?;
                    let mut environment_names = Vec::with_capacity(server.env.len());
                    let mut environment = Vec::with_capacity(server.env.len());
                    for binding in server.env {
                        let name = binding
                            .name
                            .parse::<McpEnvironmentName>()
                            .map_err(|error| safe_mcp_error(error.to_string()))?;
                        environment_names.push(name.clone());
                        environment.push(McpEnvironmentBinding::new(
                            name,
                            SecretMaterial::new(binding.value.into_bytes()),
                        ));
                    }
                    let config =
                        McpServerConfig::stdio(command, server.args, None, environment_names)
                            .map_err(|error| safe_mcp_error(error.to_string()))?;
                    (name, config, McpTransportMaterial::stdio(environment))
                }
                v1::McpServer::Http(server) => {
                    let name = server_name(&server.name)?;
                    let url = server.url.parse::<ego_core::config::McpHttpUrl>().map_err(
                        |error: ego_core::config::McpValueError| safe_mcp_error(error.to_string()),
                    )?;
                    let config =
                        McpServerConfig::http(url.as_url().clone(), McpHttpAuth::None, false)
                            .map_err(|error| safe_mcp_error(error.to_string()))?;
                    let headers = server
                        .headers
                        .into_iter()
                        .map(|header| {
                            McpHeaderBinding::new(
                                header.name,
                                SecretMaterial::new(header.value.into_bytes()),
                            )
                            .map_err(|_| safe_mcp_error("an ACP MCP HTTP header is invalid"))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    (name, config, McpTransportMaterial::http(headers))
                }
                v1::McpServer::Sse(_) => {
                    return Err(safe_mcp_error("this build does not support MCP over SSE"));
                }
                _ => {
                    return Err(safe_mcp_error(
                        "this build does not support the supplied MCP transport",
                    ));
                }
            };
            let admission = McpServerAdmission::new(name, config, material)
                .map_err(|_| safe_mcp_error("the supplied MCP server is invalid"))?;
            Ok(McpRegistryCandidate::ready(
                Arc::new(admission),
                McpConfigProvenance::Layer {
                    source: Source::Flags,
                },
            ))
        })
        .collect()
}

fn server_name(name: &str) -> Result<McpServerName, McpSessionAdmissionError> {
    name.parse()
        .map_err(|error: ego_core::config::McpValueError| safe_mcp_error(error.to_string()))
}

fn safe_mcp_error(message: impl Into<String>) -> McpSessionAdmissionError {
    McpSessionAdmissionError::new(message)
}

struct AcpListener {
    admitted: Admitted,
    bound: ego_listen::Bound,
    credential: Option<Credential>,
    gate: ego_listen::ConnectionGate,
}

async fn prepare_listener(args: &AcpArgs) -> Result<Option<AcpListener>, RunTermination> {
    let Some(listen) = args.listen.as_deref() else {
        return Ok(None);
    };
    let admitted = crate::cli::admit_endpoint(listen, args.allow_public_bind)
        .map_err(|error| failed(error.to_string()))?;
    let credential = if ego_listen::credential_required(&admitted) {
        Some(
            Credential::from_env(|name| std::env::var(name).ok())
                .map_err(|error| failed(error.to_string()))?,
        )
    } else {
        None
    };
    let limits = ego_listen::ConnectionLimits::new(
        args.max_connections,
        args.connection_lifetime,
        args.max_ingress_bytes,
    )
    .map_err(|error| failed(format!("the ACP listener limits are invalid: {error}")))?;
    let gate = ego_listen::ConnectionGate::new(limits);
    let bound = ego_listen::bind(&admitted)
        .await
        .map_err(|error| failed(format!("the ACP endpoint could not be opened: {error}")))?;
    Ok(Some(AcpListener {
        admitted,
        bound,
        credential,
        gate,
    }))
}

fn failed(message: String) -> RunTermination {
    RunTermination::Failed { message }
}

fn confinement_ceiling(
    args: &AcpArgs,
    configured: Option<Confinement>,
    endpoint: Option<&Admitted>,
) -> Confinement {
    let selected = args.sandbox.or(configured).unwrap_or_default();
    endpoint.map_or(selected, |endpoint| endpoint.cap_confinement(selected))
}

/// Serve the protocol on stdio or an explicitly selected endpoint.
pub async fn serve(args: &AcpArgs, evidence: Arc<EvidenceAdmissionService>) -> RunTermination {
    let listener = match prepare_listener(args).await {
        Ok(listener) => listener,
        Err(termination) => return termination,
    };
    let admitted = listener.as_ref().map(|listener| &listener.admitted);
    let root = match run::workspace(args.directory.as_ref()) {
        Ok(root) => root,
        Err(error) => {
            return RunTermination::UsageError {
                message: error.to_string(),
            };
        }
    };
    let store_root = match session::store_root(args.session_dir.as_ref()) {
        Ok(store_root) => run::rooted_path(&root, store_root),
        Err(error) => {
            return RunTermination::UsageError {
                message: error.to_string(),
            };
        }
    };
    let store = Arc::new(session::jsonl(store_root, root.clone()));
    let executor = Arc::new(run::CliRunExecutor::for_acp(
        args.session_dir.clone(),
        Arc::clone(&store),
        Arc::clone(&evidence),
    ));
    let compaction: Arc<dyn ego_core::compact::CompactionCommand> =
        Arc::new(run::CliCompactionCommand::for_protocol(
            root.clone(),
            args.session_dir.clone(),
            Arc::clone(&store),
            evidence,
        ));
    let ids = Arc::new(UuidIds);
    let clock = Arc::new(SystemClock);
    let controller = Arc::new(RunController::new(
        Arc::clone(&store),
        executor,
        Arc::clone(&clock),
        Arc::clone(&ids),
        RunRetention::new(NonZeroUsize::new(64).expect("non-zero retention"))
            .expect("valid retention"),
    ));
    let run_lifecycle: Arc<dyn SessionRunLifecycle> = controller.clone();
    let lifecycle = Arc::new(SessionLifecycle::new(
        Arc::clone(&store),
        run_lifecycle,
        clock,
    ));

    let mcp_cancel = CancelToken::new();
    let mcp_effects = match CommandHttpScope::online(mcp_cancel.clone()).await {
        Ok(effects) => effects,
        Err(error) => {
            return RunTermination::Failed {
                message: format!("the ACP MCP effect scope could not be created: {error}"),
            };
        }
    };
    let http_management: Arc<dyn McpHttpEffects> =
        Arc::new(RunMcpHttpEffects::new(Arc::clone(mcp_effects.ports())));
    let stdio_management: Arc<dyn McpDuplexEffectsFactory> = Arc::new(
        RunMcpDuplexEffectsFactory::new(mcp_effects.ports().duplex_handle()),
    );
    let mcp_admitter: Arc<dyn McpSessionAdmitter> = Arc::new(AcpMcpSessionAdmitter::new(
        http_management,
        stdio_management,
        MCP_DEFAULT_MAX_SSE_EVENT_SIZE,
        MCP_DEFAULT_OPERATION_TIMEOUT,
    ));

    // The connected host is this connection's operator seat, so it gets the
    // picker a terminal seat gets. `run.rs` builds the catalogue only where
    // there is a human, and the ACP client is one. Discovery reuses the HTTP
    // scope this server already opened rather than a second egress: an `ego
    // acp` invocation is online by construction, so this adds no reach it did
    // not already have.
    let (controls, ceiling) =
        match run::protocol_effective(root.as_path(), args.session_dir.clone()) {
            Ok(effective) => {
                crate::config::report_discarded(root.as_path(), &effective, &crate::output::Stderr);
                let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(mcp_effects.ports())))
                    as Arc<dyn ego_ai::http::Http>;
                let catalogue = match crate::models::build_catalogue_snapshot(
                    &crate::cli::ModelsArgs {
                        provider: None,
                        refresh: false,
                        json: false,
                    },
                    &http,
                )
                .await
                {
                    Ok(catalogue) => Some(catalogue),
                    // A picker that lists only the configured model is worse
                    // than one that lists every model; a connection that
                    // refuses to start is worse than both.
                    Err(error) => {
                        tracing::warn!(?error, "the ACP model catalogue is unavailable");
                        None
                    }
                };
                // The flag wins over the configuration, and the configuration
                // over the default. A host that was started at `--sandbox ro`
                // is capped there whatever any layer says, which is the whole
                // reason the flag exists: the layer an operator would have
                // reached for is the one an untrusted headless resolution
                // discards.
                let ceiling = confinement_ceiling(args, effective.sandbox, admitted);
                (
                    Arc::new(AcpSessionControls::new(&effective, ceiling, catalogue)),
                    ceiling,
                )
            }
            Err(error) => {
                return RunTermination::UsageError {
                    message: format!(
                        "the ACP confinement configuration could not be resolved: {error}"
                    ),
                };
            }
        };

    let reporter = Arc::new(Reporter::new(Arc::new(crate::output::Stderr)));
    if listener.is_some() {
        let policy = RunConfinement::new(ceiling, false, CanonPath::new(root.as_path()))
            .expect("the resolved ACP process root is absolute");
        reporter.report(
            RunEffectPorts::diagnostic_capabilities(
                &policy,
                mcp_cancel.clone(),
                ExecLimits::default(),
            )
            .await,
        );
    }

    let control_port: Arc<dyn SessionControls> = controls.clone();
    let confinement_port: Arc<dyn SessionConfinementAdmission> =
        Arc::new(CliSessionConfinementAdmission::with_reporter(
            Arc::clone(&store),
            controls,
            ceiling,
            mcp_cancel.clone(),
            Arc::clone(&reporter),
        ));

    let roots = Arc::new(CanonicalRoots { base: root });
    let spawner = Arc::new(TokioExecutionSpawner);
    let factory = move || {
        ego_acp::serve::durable_agent_with_compaction(
            Arc::clone(&controller),
            Arc::clone(&lifecycle),
            roots.clone(),
            spawner.clone(),
            ids.clone(),
            Arc::new(AcpRunBindingsBuilder::with_controls(Some(Arc::clone(
                &control_port,
            )))),
            Arc::clone(&mcp_admitter),
            Arc::clone(&compaction),
            Some(Arc::clone(&control_port)),
            Some(Arc::clone(&confinement_port)),
        )
    };
    let outcome = match listener {
        Some(listener) => ego_acp::serve::serve_on(
            listener.bound,
            &listener.admitted,
            listener.credential,
            listener.gate,
            factory,
            std::future::pending(),
        )
        .await
        .map_err(|error| format!("the ACP listener ended badly: {error}")),
        None => factory()
            .connect_to(Stdio::new())
            .await
            .map_err(|error| format!("the ACP connection ended badly: {error}")),
    };
    mcp_cancel.cancel(CancelCause::Host);
    drop(mcp_effects);
    match outcome {
        Ok(()) => RunTermination::Done,
        Err(message) => RunTermination::Failed { message },
    }
}

#[cfg(test)]
mod confinement_ceiling_tests {
    use super::confinement_ceiling;
    use crate::cli::{self, AcpArgs, Invocation};
    use ego_core::endpoint::Admitted;
    use ego_core::permit::Confinement;

    fn args(sandbox: Option<Confinement>) -> AcpArgs {
        let mut argv = vec!["ego".to_owned(), "acp".to_owned()];
        if let Some(sandbox) = sandbox {
            argv.extend(["--sandbox".to_owned(), sandbox.as_str().to_owned()]);
        }
        match cli::parse(argv).expect("the ACP command line parses") {
            Invocation::Acp(args) => *args,
            _ => panic!("the test command is not `ego acp`"),
        }
    }

    #[test]
    fn resolves_flag_then_config_then_default_and_caps_only_admitted_public() {
        let local = Admitted::Local {
            name: "regression case.sock".to_owned(),
        };
        let loopback = Admitted::Loopback("127.0.0.1:8080".parse().unwrap());
        let public = Admitted::Public("0.0.0.0:8080".parse().unwrap());

        assert_eq!(
            confinement_ceiling(&args(Some(Confinement::Ro)), Some(Confinement::Off), None),
            Confinement::Ro,
            "the CLI flag wins over configuration"
        );
        assert_eq!(
            confinement_ceiling(&args(None), Some(Confinement::Ro), None),
            Confinement::Ro,
            "configuration wins when the CLI does not name a ceiling"
        );
        assert_eq!(
            confinement_ceiling(&args(None), None, None),
            Confinement::Workspace,
            "the default applies when neither flag nor configuration names a ceiling"
        );
        assert_eq!(
            confinement_ceiling(&args(None), Some(Confinement::Off), Some(&public)),
            Confinement::Workspace,
            "an admitted public endpoint caps unconfined ACP at workspace"
        );
        assert_eq!(
            confinement_ceiling(&args(None), Some(Confinement::Off), None),
            Confinement::Off,
            "stdio has no endpoint cap"
        );
        assert_eq!(
            confinement_ceiling(&args(None), Some(Confinement::Off), Some(&local)),
            Confinement::Off,
            "a local endpoint adds no confinement cap"
        );
        assert_eq!(
            confinement_ceiling(&args(None), Some(Confinement::Off), Some(&loopback)),
            Confinement::Off,
            "a loopback endpoint adds no confinement cap"
        );
    }
}
