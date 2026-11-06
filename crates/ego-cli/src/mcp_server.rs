//! `ego mcp-server`: production composition for scoped rooted runs over stdio.

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use ego_core::event::id::SessionId;
use ego_core::evidence::EvidenceAdmissionService;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::permit::{CleanupCertainty, DuplexEnd, DuplexSession, DuplexSettlement, ToolResult};
use ego_core::run::{EventSinkFactory, RunController, RunRetention, RunRoot, RunTermination};
use ego_core::sink::{EventSink, NullSink};
use ego_core::store::run_artifact::{RunArtifactId, RunArtifactStore, RunArtifactStoreError};
use ego_core::turn::CancelToken;
use ego_mcp::{
    McpError, McpFailure, McpFuture, McpRunBindingsBuilder, McpRunResourceReader,
    McpRunResourceRequest, McpRunSelectionPolicy, McpRunServerConfig, McpRunServerFactory,
    McpRunServerPorts, McpServerPort, SanitizedMcpRunResource, serialize_run_snapshot,
};
use ego_tools::clock::SystemClock;
use ego_tools::store::JsonlStore;
use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncWrite};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::cli::{McpServerArgs, ModelsArgs};
use crate::host_ports::{CanonicalRoots, TokioExecutionSpawner, UuidIds};
use crate::{models, run, session};

type Controller = RunController<JsonlStore, run::CliRunExecutor, SystemClock, UuidIds>;

#[derive(Debug, Default)]
struct NoOutputBindings;

#[derive(Debug, Default)]
struct NoOutputSinkFactory;

impl EventSinkFactory for NoOutputSinkFactory {
    fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

impl McpRunBindingsBuilder for NoOutputBindings {
    fn build_sink(
        &self,
        _session_id: SessionId,
        _request_state: Option<&str>,
    ) -> Result<Arc<dyn EventSinkFactory>, McpError> {
        Ok(Arc::new(NoOutputSinkFactory))
    }
}

struct RunResources {
    controller: Arc<Controller>,
    artifacts: Arc<JsonlStore>,
}

impl McpRunResourceReader for RunResources {
    fn read<'a>(
        &'a self,
        request: McpRunResourceRequest,
    ) -> McpFuture<'a, Result<SanitizedMcpRunResource, McpError>> {
        Box::pin(async move {
            match request {
                McpRunResourceRequest::Summary { run_id } => {
                    let snapshot = self.controller.status(run_id).await.map_err(|error| {
                        McpError::new(McpFailure::InvalidValue, error.to_string())
                    })?;
                    let text = serialize_run_snapshot(&snapshot)?;
                    Ok(SanitizedMcpRunResource::Text {
                        mime_type: Arc::from("application/json"),
                        text: Arc::from(text),
                    })
                }
                McpRunResourceRequest::Artifact {
                    run_id,
                    artifact_id,
                } => {
                    if artifact_id != RunArtifactId::output().as_str() {
                        return Err(McpError::new(
                            McpFailure::Absent,
                            "MCP run artifact is absent",
                        ));
                    }
                    let artifact = self
                        .artifacts
                        .read(run_id, RunArtifactId::output())
                        .map_err(artifact_error)?;
                    let mime_type = Arc::from(artifact.mime_type());
                    if matches!(artifact.mime_type(), "text/plain" | "application/json") {
                        let text = std::str::from_utf8(artifact.bytes()).map_err(|error| {
                            McpError::new(
                                McpFailure::InvalidResponse,
                                format!("published MCP run artifact is not UTF-8: {error}"),
                            )
                        })?;
                        Ok(SanitizedMcpRunResource::Text {
                            mime_type,
                            text: Arc::from(text),
                        })
                    } else {
                        Ok(SanitizedMcpRunResource::Blob {
                            mime_type,
                            bytes: Arc::from(artifact.bytes()),
                        })
                    }
                }
            }
        })
    }
}

fn artifact_error(error: RunArtifactStoreError) -> McpError {
    let failure = match &error {
        RunArtifactStoreError::Absent { .. } => McpFailure::Absent,
        RunArtifactStoreError::Conflict { .. } => McpFailure::InvalidResponse,
        RunArtifactStoreError::Io { .. } => McpFailure::Transport,
    };
    McpError::new(failure, error.to_string())
}

struct ProcessStdio {
    input: Compat<tokio::io::Stdin>,
    output: Compat<tokio::io::Stdout>,
    delivered: bool,
}

impl ProcessStdio {
    fn new() -> Self {
        Self {
            input: tokio::io::stdin().compat(),
            output: tokio::io::stdout().compat_write(),
            delivered: false,
        }
    }
}

impl AsyncRead for ProcessStdio {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.input).poll_read(context, buffer)
    }
}

impl AsyncWrite for ProcessStdio {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.output).poll_write(context, buffer) {
            Poll::Ready(Ok(written)) => {
                self.delivered |= written != 0;
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.output).poll_flush(context)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.output).poll_close(context)
    }
}

impl DuplexSession for ProcessStdio {
    fn delivery(&self) -> DeliveryCertainty {
        if self.delivered {
            DeliveryCertainty::DefinitelySent
        } else {
            DeliveryCertainty::DefinitelyUnsent
        }
    }

    fn settle(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
        let delivery = self.delivery();
        Box::pin(async move {
            DuplexSettlement::new(
                DuplexEnd::Exited { code: 0 },
                CleanupCertainty::Complete,
                delivery,
                Ok(ToolResult::builtin("")),
            )
        })
    }

    fn interrupt(self: Box<Self>) -> BoxFuture<'static, DuplexSettlement> {
        let delivery = self.delivery();
        Box::pin(async move {
            DuplexSettlement::new(
                DuplexEnd::Interrupted,
                CleanupCertainty::Complete,
                delivery,
                Ok(ToolResult::builtin("")),
            )
        })
    }
}

/// Serve one connection until the host closes stdio.
pub async fn serve(
    args: &McpServerArgs,
    evidence: Arc<EvidenceAdmissionService>,
) -> RunTermination {
    match compose_and_serve(args, evidence).await {
        Ok(settlement) if matches!(settlement.end(), DuplexEnd::Exited { code: 0 }) => {
            RunTermination::Done
        }
        Ok(settlement) => RunTermination::Failed {
            message: format!("the MCP stdio connection ended: {}", settlement.end()),
        },
        Err(error) if error.failure() == McpFailure::InvalidValue => RunTermination::UsageError {
            message: error.to_string(),
        },
        Err(error) => RunTermination::Failed {
            message: format!("the MCP server ended badly: {error}"),
        },
    }
}

async fn compose_and_serve(
    args: &McpServerArgs,
    evidence: Arc<EvidenceAdmissionService>,
) -> Result<DuplexSettlement, McpError> {
    let root = run::workspace(args.directory.as_ref())
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
    let run_root = RunRoot::new(root.clone())
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
    let store_root = session::store_root(args.session_dir.as_ref())
        .map(|store_root| run::rooted_path(&root, store_root))
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
    let store = Arc::new(session::jsonl(store_root, root.clone()));
    let artifact_publisher: Arc<dyn RunArtifactStore> = store.clone();
    let effective = run::protocol_effective(
        root.as_path(),
        args.session_dir.clone(),
        crate::cli::SessionShape::default(),
    )
    .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
    let catalogue = if effective.thinking.is_none() {
        let scope = crate::provider_http::CommandHttpScope::online(CancelToken::new())
            .await
            .map_err(|message| McpError::new(McpFailure::Transport, message))?;
        Some(
            models::build_catalogue_snapshot(
                &ModelsArgs {
                    provider: None,
                    refresh: false,
                    json: false,
                },
                scope.http(),
            )
            .await
            .map_err(catalogue_error)?,
        )
    } else {
        None
    };
    let (executor, deployment) = run::CliRunExecutor::for_mcp(
        run_root,
        args.session_dir.clone(),
        Arc::clone(&store),
        artifact_publisher,
        args.max_concurrent_runs,
        Arc::clone(&evidence),
        catalogue.as_ref(),
    )
    .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;

    let mut roots = BTreeSet::from([deployment.root]);
    for allowed in &args.allow_roots {
        let allowed = run::workspace(Some(&run::rooted_path(&root, allowed.clone())))
            .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
        roots.insert(
            RunRoot::new(allowed)
                .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?,
        );
    }
    let mut models = BTreeSet::from([deployment.model]);
    models.extend(args.allow_models.iter().cloned());
    let mut modes = BTreeSet::from([deployment.mode]);
    modes.extend(args.allow_modes.iter().copied());
    let policy = McpRunSelectionPolicy::new(
        roots,
        models,
        deployment.thinking,
        modes,
        deployment.ceilings,
        deployment.profile.into_iter().collect(),
    )?;
    let config = Arc::new(McpRunServerConfig::new(
        policy,
        args.max_concurrent_runs,
        args.max_resource_bytes,
    )?);
    let ids = Arc::new(UuidIds);
    let controller = Arc::new(RunController::new(
        Arc::clone(&store),
        Arc::new(executor),
        Arc::new(SystemClock),
        ids,
        RunRetention::new(args.retained_requests)
            .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?,
    ));
    let resources = Arc::new(RunResources {
        controller: Arc::clone(&controller),
        artifacts: Arc::clone(&store),
    });
    let compaction: Arc<dyn ego_core::compact::CompactionCommand> =
        Arc::new(run::CliCompactionCommand::for_protocol(
            root.clone(),
            args.session_dir.clone(),
            Arc::clone(&store),
            evidence,
        ));
    let ports = McpRunServerPorts::new(
        Arc::new(CanonicalRoots { base: root }),
        Arc::new(TokioExecutionSpawner),
        Arc::new(NoOutputBindings),
        resources,
    )
    .with_compaction(compaction);
    let factory = McpRunServerFactory::new(controller, config, ports);
    let connection: Arc<dyn McpServerPort> = Arc::new(factory.new_connection());
    ego_mcp::serve_stdio(connection, Box::new(ProcessStdio::new())).await
}

fn catalogue_error(termination: RunTermination) -> McpError {
    let (failure, message) = match termination {
        RunTermination::UsageError { message } => (McpFailure::InvalidValue, message),
        RunTermination::Provider { message } => (McpFailure::Transport, message),
        RunTermination::ContextOverflow { message } | RunTermination::Failed { message } => {
            (McpFailure::Protocol, message)
        }
        other => (
            McpFailure::Protocol,
            format!("the model catalogue stopped with {}", other.exit_reason()),
        ),
    };
    McpError::new(failure, message)
}
