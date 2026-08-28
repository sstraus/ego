//! `ego acp`: production composition for the inbound ACP adapter.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::{ConnectTo, Stdio};
use agent_client_protocol_schema::v1;
use ego_acp::serve::{
    AcpRunBindingsBuilder, ExecutionSpawner, McpSessionAdmissionError, McpSessionAdmitter,
    RootResolveError, RootResolver, SessionMcpAdmission,
};
use ego_core::config::{
    McpConfigProvenance, McpEnvironmentName, McpHttpAuth, McpServerConfig, McpServerName, Source,
};
use ego_core::credentials::SecretMaterial;
use ego_core::event::domain::CancelCause;
use ego_core::run::{IdSource, RunController, RunRetention, RunRoot, RunTermination};
use ego_core::suspension::ContinuationJournal;
use ego_core::turn::{AdmittedToolSource, CancelToken};
use ego_mcp::{
    DurableMcpInputRequiredRecorder, MCP_DEFAULT_MAX_SSE_EVENT_SIZE, MCP_DEFAULT_OPERATION_TIMEOUT,
    McpDuplexEffectsFactory, McpEnvironmentBinding, McpHeaderBinding, McpHttpEffects,
    McpInputRequiredRecorder, McpRegistryCandidate, McpRegistryController, McpRegistryRefresh,
    McpServerAdmission, McpTransportMaterial, RegistryScope, RegistryTrigger,
};
use ego_tools::clock::SystemClock;
use ego_tools::mcp::{RunMcpDuplexEffectsFactory, RunMcpHttpEffects};
use ego_tools::mcp_continuation::JsonlContinuationJournal;
use ego_tools::mcp_registry::JsonlMcpRegistryJournal;
use ego_tools::store::OsIo;
use futures::future::BoxFuture;

use crate::cli::AcpArgs;
use crate::provider_http::CommandHttpScope;
use crate::{run, session};

struct CanonicalRoots {
    base: PathBuf,
}

impl RootResolver for CanonicalRoots {
    fn resolve(&self, cwd: &Path) -> Result<RunRoot, RootResolveError> {
        let candidate = if cwd.is_absolute() {
            cwd.to_path_buf()
        } else {
            self.base.join(cwd)
        };
        let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
            RootResolveError::new(format!(
                "the ACP working directory {} could not be resolved: {error}",
                candidate.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(RootResolveError::new(format!(
                "the ACP working directory {} is not a directory",
                canonical.display()
            )));
        }
        RunRoot::new(canonical).map_err(|error| RootResolveError::new(error.to_string()))
    }
}

#[derive(Debug, Default)]
struct TokioExecutionSpawner;

impl ExecutionSpawner for TokioExecutionSpawner {
    fn spawn(
        &self,
        execution: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) {
        tokio::spawn(execution);
    }
}

#[derive(Debug, Default)]
struct UuidIds;

impl IdSource for UuidIds {
    fn next_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::new_v4()
    }
}

struct AcpSessionMcpAdmission {
    _controller: Arc<McpRegistryController>,
    _storage: tempfile::TempDir,
    admitted_tools: Arc<dyn AdmittedToolSource>,
    mcp_errors: Vec<String>,
}

impl AcpSessionMcpAdmission {
    fn from_controller(controller: Arc<McpRegistryController>, storage: tempfile::TempDir) -> Self {
        let admitted_tools = controller.admitted_tools();
        let mcp_errors = controller.status().mcp_errors();
        Self {
            _controller: controller,
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
            let continuations: Arc<dyn McpInputRequiredRecorder> =
                Arc::new(DurableMcpInputRequiredRecorder::new(continuation_journal));
            let controller = McpRegistryController::new_runtime(
                registry_journal,
                http_management,
                stdio_management,
                continuations,
                max_sse_event_size,
                timeout,
            )
            .map_err(|_| safe_mcp_error("ACP MCP runtime could not be created"))?;
            controller
                .reconcile(refresh)
                .await
                .map_err(|_| safe_mcp_error("the supplied MCP servers could not be admitted"))?;
            if !controller.status().mcp_errors().is_empty() {
                return Err(safe_mcp_error(
                    "the supplied MCP servers could not be admitted",
                ));
            }
            Ok(Some(
                Arc::new(AcpSessionMcpAdmission::from_controller(controller, storage))
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

/// Serve the protocol on stdio until the host closes it.
pub async fn serve(args: &AcpArgs) -> RunTermination {
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
    ));
    let ids = Arc::new(UuidIds);
    let controller = Arc::new(RunController::new(
        store,
        executor,
        Arc::new(SystemClock),
        Arc::clone(&ids),
        RunRetention::new(NonZeroUsize::new(64).expect("non-zero retention"))
            .expect("valid retention"),
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

    let agent = ego_acp::serve::agent(
        controller,
        Arc::new(CanonicalRoots { base: root }),
        Arc::new(TokioExecutionSpawner),
        ids,
        Arc::new(AcpRunBindingsBuilder),
        mcp_admitter,
    );
    let outcome = agent.connect_to(Stdio::new()).await;
    mcp_cancel.cancel(CancelCause::Host);
    drop(mcp_effects);
    match outcome {
        Ok(()) => RunTermination::Done,
        Err(error) => RunTermination::Failed {
            message: format!("the ACP connection ended badly: {error}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn canonical_roots_accepts_a_directory_but_rejects_an_existing_regular_file() {
        let owner = tempdir().expect("ACP root owner");
        let directory = owner.path().join("workspace");
        std::fs::create_dir(&directory).expect("ACP workspace directory");
        let regular_file = owner.path().join("workspace-file");
        std::fs::write(&regular_file, b"not a workspace").expect("ACP workspace file");
        let roots = CanonicalRoots {
            base: owner.path().to_path_buf(),
        };

        assert_eq!(
            roots
                .resolve(Path::new("workspace"))
                .expect("directory is an ACP root")
                .as_path(),
            std::fs::canonicalize(directory)
                .expect("canonical ACP workspace")
                .as_path()
        );
        assert!(
            roots.resolve(Path::new("workspace-file")).is_err(),
            "an existing regular file must be rejected as an ACP root: {}",
            regular_file.display()
        );
    }
}
