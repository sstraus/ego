//! Poll and cancel for MCP tasks over HTTP.
//!
//! A task is started by a `tools/call` on one HTTP server. Each poll and each
//! cancel is its own guarded round: it claims the prepared call that the task
//! ticket derived from the original call, connects, declares the extension,
//! sends `tasks/get` or `tasks/cancel` for the ticket's id, and settles. Stdio
//! and ACP servers never reach this code (they never declare the extension).

use super::*;

use ego_core::mcp::{McpTaskPoll, McpTaskPoller, McpTaskPollerHandle, McpTaskStart};

use crate::state::McpTaskState;

/// The longest one poll round may take: far less than a tool call.
const POLL_ROUND_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest one cancel round may take: a hung server must not stall the
/// end of a session (`/new`, exit); cancels of several tasks run at once.
const CANCEL_ROUND_TIMEOUT: Duration = Duration::from_secs(5);

/// The model's immediate answer and the poller for a task the server started.
pub(super) fn task_started(
    task: &rmcp::model::Task,
    envelope: &ego_core::permit::McpPreparedEnvelope,
    config: &McpHttpConfig,
    admission: Arc<McpServerAdmission>,
    limits: OperationLimits,
) -> ToolExecution {
    let server = envelope.subject().server().to_owned();
    let tool = envelope.subject().operation_name().to_owned();
    let text = format!(
        "MCP task {} started on server {server} for tool {tool}. The result arrives in this conversation when the task ends; continue other work meanwhile. A task ends when ego exits or the session changes, so after a restart or resume treat it as lost.",
        task.task_id
    );
    ToolExecution::TaskStarted(McpTaskStart {
        task_id: task.task_id.clone(),
        server,
        tool,
        poll_interval_ms: task.poll_interval_ms,
        ttl_ms: task.ttl_ms,
        result: ToolResult::builtin(text),
        poller: McpTaskPollerHandle(Arc::new(HttpTaskPoller {
            config: config.clone(),
            admission,
            limits,
        })),
    })
}

struct HttpTaskPoller {
    config: McpHttpConfig,
    admission: Arc<McpServerAdmission>,
    limits: OperationLimits,
}

/// What one guarded task round observed.
struct TaskRound {
    state: Option<McpTaskState>,
    envelope: ego_core::permit::McpPreparedEnvelope,
    delivery: DeliveryCertainty,
    cancellation: CancellationClassification,
}

impl HttpTaskPoller {
    async fn round(
        &self,
        call: PreparedCall,
        io: &dyn GuardedIo<'_>,
        cancel: bool,
    ) -> Result<TaskRound, McpError> {
        let envelope = call
            .mcp_envelope()
            .cloned()
            .ok_or_else(|| authority("selected MCP task call has no protocol envelope"))?;
        let expected = if cancel {
            McpOperationKind::CancelTask
        } else {
            McpOperationKind::GetTask
        };
        let McpSubject::Task { task_id, .. } = envelope.subject() else {
            return Err(authority("selected MCP task call has no task subject"));
        };
        if envelope.operation() != expected {
            return Err(authority("selected MCP task operation changed"));
        }
        let task_id = task_id.clone();
        let server = server_name(envelope.subject().server())?;
        let profile = AdmittedMcpHttpProfile::initial(&server, self.config.url.as_url())?;
        let route = profile.route(self.config.url.as_url())?;
        let transport = ForeignTransportTarget::http(route.clone());
        let fingerprint =
            http_transport_fingerprint(profile.fingerprint(), envelope.admission(), &transport);
        if envelope.transport() != &transport || envelope.transport_fingerprint() != fingerprint {
            return Err(authority("selected MCP task transport fingerprint changed"));
        }
        let expected_wire_params = serde_json::json!({ "taskId": task_id });
        let claim = io.claim_mcp_http(call).map_err(McpError::from)?;
        if claim.envelope() != &envelope {
            return Err(authority("guarded MCP task claim changed its envelope"));
        }
        let limits = OperationLimits {
            timeout: self.limits.timeout.min(if cancel {
                CANCEL_ROUND_TIMEOUT
            } else {
                POLL_ROUND_TIMEOUT
            }),
            ..self.limits
        };
        let broker = Arc::new(
            SplitMcpHttpEffects::new(
                claim,
                server.clone(),
                profile.fingerprint(),
                route,
                envelope.clone(),
                expected_wire_params,
                self.admission
                    .authorization_headers(None, Instant::now() + limits.timeout)
                    .await?,
                self.admission.oauth_session().cloned(),
            )
            .with_tasks(true),
        );
        let effects: Arc<dyn McpHttpEffects> = broker.clone();
        let mut client = McpClient::connect_http(
            server,
            self.config.clone(),
            limits.options()?.with_tasks(true),
            effects,
        )?;
        if let Err(error) = client.discover().await {
            let _ = client.settle().await;
            return Err(error);
        }
        let outcome = if cancel {
            client.cancel_task_once(&task_id).await.map(|()| None)
        } else {
            client.get_task_once(&task_id).await.map(Some)
        };
        let settlement = client.settle().await;
        let state = outcome?;
        settlement?;
        let selected = broker
            .selected_settlement()?
            .ok_or_else(|| protocol("selected MCP task operation was not settled"))?;
        Ok(TaskRound {
            state,
            envelope,
            delivery: selected.delivery(),
            cancellation: cancellation_from_http(selected.end()),
        })
    }
}

impl McpTaskPoller for HttpTaskPoller {
    fn poll<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> futures::future::BoxFuture<'a, Result<McpTaskPoll, String>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            let round = self
                .round(call, io, false)
                .await
                .map_err(|error| error.to_string())?;
            let state = round
                .state
                .ok_or_else(|| "a poll round returned no task state".to_owned())?;
            Ok(match state {
                McpTaskState::Working => McpTaskPoll::Working,
                McpTaskState::InputRequired => McpTaskPoll::InputRequired,
                McpTaskState::Failed(error) => McpTaskPoll::Failed(error),
                McpTaskState::Cancelled => McpTaskPoll::Cancelled,
                McpTaskState::Completed(result) => {
                    let projected = project_complete(CompleteResult::Tool(*result))
                        .map_err(|error| error.to_string())?;
                    let evidence = io
                        .store_evidence(&projected.evidence)
                        .map_err(|error| error.to_string())?;
                    let envelope = &round.envelope;
                    let foreign = ForeignResult::new(
                        envelope.subject().server(),
                        envelope.subject().operation_name(),
                        InvocationDigest::from_bytes(envelope.parameters()),
                        envelope.admission().clone(),
                        projected.result_type,
                        projected.ttl_ms,
                        projected.cache_scope,
                        projected.content,
                        evidence,
                        round.delivery,
                        round.cancellation,
                    )
                    .map_err(|error| error.to_string())?;
                    McpTaskPoll::Completed(ToolResult::from_foreign(foreign))
                }
            })
        })
    }

    fn cancel<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> futures::future::BoxFuture<'a, Result<(), String>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            self.round(call, io, true)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::client_capabilities;

    fn wire(task_id: &str, declared: bool) -> Value {
        serde_json::json!({
            "taskId": task_id,
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": MCP_PROTOCOL_REVISION,
                "io.modelcontextprotocol/clientInfo": {
                    "name": "ego",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "io.modelcontextprotocol/clientCapabilities": client_capabilities(declared),
                "progressToken": 3
            }
        })
    }

    // Catches: the claim admits a poll for another task id, or one whose declared
    // capabilities differ from the session's, after permission was decided.
    #[test]
    fn a_task_request_is_admitted_only_with_the_prepared_id_and_the_declared_capabilities() {
        let expected = serde_json::json!({"taskId": "task-1"});
        assert!(selected_wire_params_match(
            wire("task-1", true),
            &expected,
            true
        ));
        assert!(!selected_wire_params_match(
            wire("task-2", true),
            &expected,
            true
        ));
        assert!(!selected_wire_params_match(
            wire("task-1", false),
            &expected,
            true
        ));
        assert!(!selected_wire_params_match(
            wire("task-1", true),
            &expected,
            false
        ));
        let mut extra = wire("task-1", true);
        extra["arguments"] = serde_json::json!({"smuggled": true});
        assert!(!selected_wire_params_match(extra, &expected, true));
    }
}
