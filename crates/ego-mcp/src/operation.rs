//! Fresh per-operation MCP lifecycles bound to guarded transport authority.

use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ego_core::config::{McpHttpConfig, McpServerConfig, McpServerName, McpStdioConfig};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::mcp::{
    CacheScope as CoreCacheScope, CancellationClassification, ForeignContent, ForeignInputRequired,
    ForeignResult, McpAdmissionFingerprint, McpOperationKind, McpSubject, ToolExecution,
};
use ego_core::permit::{
    Authorized, CanonPath, DuplexEnd, DuplexHandle, EnvPolicy, ForeignTransportTarget, GuardedIo,
    HttpRoute, InvocationDigest, McpHttpClaim, OneHopResponse, PreparedCall, SpawnSpec, ToolError,
    ToolResult,
};
use ego_core::suspension::{
    ContinuationError, ContinuationJournal, OpenedContinuation, PreparedResume,
};
use futures::future;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResult, ReadResourceRequestParams, ReadResourceResult,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::dynamic::{McpOperation, McpOperationFactory};
use crate::http::AdmittedMcpHttpProfile;
use crate::state::{compile_schema, validate_answer};
use crate::{
    McpClient, McpClientOptions, McpDuplexEffects, McpError, McpFailure, McpFuture, McpHttpEffects,
    McpHttpEnd, McpHttpPurpose, McpHttpResponse, McpHttpSettlement, McpManualOutcome,
    McpPreparedHttpParts, McpRequestId, PreparedMcpHttpCall,
};

#[derive(Clone, Copy)]
struct OperationLimits {
    max_sse_event_size: usize,
    timeout: Duration,
}

impl OperationLimits {
    fn new(max_sse_event_size: usize, timeout: Duration) -> Result<Self, McpError> {
        if max_sse_event_size == 0 || timeout.is_zero() {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid MCP operation limits",
            ));
        }
        Ok(Self {
            max_sse_event_size,
            timeout,
        })
    }

    fn options(self) -> Result<McpClientOptions, McpError> {
        let deadline = Instant::now().checked_add(self.timeout).ok_or_else(|| {
            McpError::new(
                McpFailure::InvalidValue,
                "MCP operation deadline overflowed",
            )
        })?;
        McpClientOptions::new(self.max_sse_event_size, deadline)
    }
}

/// A fresh HTTP lifecycle. Management traffic uses its injected effect port;
/// the selected operation alone receives the guarded one-shot claim.
pub struct McpHttpOperationFactory {
    management: Arc<dyn McpHttpEffects>,
    continuations: Arc<dyn McpInputRequiredRecorder>,
    limits: OperationLimits,
}

/// Consumer-owned persistence boundary used before an input-required outcome
/// is allowed to return control to the turn loop.
pub trait McpInputRequiredRecorder: Send + Sync {
    fn record(
        &self,
        envelope: &ego_core::permit::McpPreparedEnvelope,
        required: crate::McpMappedInput,
        resume: PreparedResume,
    ) -> Result<ForeignInputRequired, ContinuationError>;
}

/// Durable adapter from one validated MRTR form to the core journal port.
pub struct DurableMcpInputRequiredRecorder {
    journal: Arc<dyn ContinuationJournal>,
}

impl DurableMcpInputRequiredRecorder {
    pub fn new(journal: Arc<dyn ContinuationJournal>) -> Self {
        Self { journal }
    }
}

impl McpInputRequiredRecorder for DurableMcpInputRequiredRecorder {
    fn record(
        &self,
        envelope: &ego_core::permit::McpPreparedEnvelope,
        required: crate::McpMappedInput,
        resume: PreparedResume,
    ) -> Result<ForeignInputRequired, ContinuationError> {
        let schema =
            serde_json::to_vec(required.schema()).map_err(|_| ContinuationError::InvalidRecord)?;
        let id = continuation_id(envelope, &required, &schema);
        let record = resume.into_record(
            id.clone(),
            envelope.parameters().to_vec(),
            required.request_state().to_vec(),
            required.correlation_key(),
            schema,
            envelope.transport_fingerprint(),
        )?;
        self.journal.record(record)?;
        ForeignInputRequired::new(id).map_err(|_| ContinuationError::InvalidId)
    }
}

/// Protocol-validated continuation ready to reconstruct one exact MRTR retry.
pub struct McpResumeAdmission {
    opened: OpenedContinuation,
}

impl McpResumeAdmission {
    pub fn validate(opened: OpenedContinuation) -> Result<Self, McpError> {
        let schema: Value = serde_json::from_slice(opened.correlation_schema())?;
        let answer: Value = serde_json::from_slice(opened.accepted_answer())?;
        let validator = compile_schema(&schema)?;
        validate_answer(&validator, &answer)?;
        Ok(Self { opened })
    }

    pub fn into_retry_authorized(self) -> Result<Authorized, McpError> {
        self.opened.into_retry_authorized().map_err(|error| {
            McpError::new(
                McpFailure::Authority,
                format!("invalid MRTR continuation: {error}"),
            )
        })
    }
}

fn continuation_id(
    envelope: &ego_core::permit::McpPreparedEnvelope,
    required: &crate::McpMappedInput,
    schema: &[u8],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ego:mcp:continuation:v1\0");
    digest.update(envelope.subject().server().as_bytes());
    digest.update([0]);
    digest.update(envelope.operation().method().as_bytes());
    digest.update([0]);
    digest.update(envelope.admission().generation().as_bytes());
    digest.update([0]);
    digest.update(envelope.admission().operation().as_bytes());
    digest.update([0]);
    digest.update(envelope.admission().schema().as_bytes());
    digest.update([0]);
    digest.update(envelope.transport_fingerprint().as_bytes());
    digest.update([0]);
    digest.update(envelope.parameters());
    digest.update([0]);
    digest.update(required.correlation_key().as_bytes());
    digest.update([0]);
    digest.update(required.request_state());
    digest.update([0]);
    digest.update(schema);
    format!("mcp-{:x}", digest.finalize())
}

impl McpHttpOperationFactory {
    pub fn new(
        management: Arc<dyn McpHttpEffects>,
        continuations: Arc<dyn McpInputRequiredRecorder>,
        max_sse_event_size: usize,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        Ok(Self {
            management,
            continuations,
            limits: OperationLimits::new(max_sse_event_size, timeout)?,
        })
    }
}

impl McpOperationFactory for McpHttpOperationFactory {
    fn prepare(
        &self,
        config: &McpServerConfig,
        operation: McpOperation,
    ) -> Result<PreparedCall, ToolError> {
        let McpServerConfig::Http(config) = config else {
            return Err(prepare_error("HTTP MCP factory received stdio config"));
        };
        prepare_http(config, operation).map_err(mcp_prepare_error)
    }

    fn execute<'a>(
        &'a self,
        config: &'a McpServerConfig,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
    ) -> futures::future::BoxFuture<'a, Result<ToolExecution, ToolError>> {
        Box::pin(async move {
            let McpServerConfig::Http(config) = config else {
                return Err(execution_error("HTTP MCP factory received stdio config"));
            };
            execute_http(
                config,
                call,
                io,
                Arc::clone(&self.management),
                Arc::clone(&self.continuations),
                self.limits,
            )
            .await
            .map_err(mcp_execution_error)
        })
    }
}

/// A fresh stdio lifecycle consuming one guarded owned duplex.
pub struct McpStdioOperationFactory {
    continuations: Arc<dyn McpInputRequiredRecorder>,
    limits: OperationLimits,
}

impl McpStdioOperationFactory {
    pub fn new(
        continuations: Arc<dyn McpInputRequiredRecorder>,
        max_sse_event_size: usize,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        Ok(Self {
            continuations,
            limits: OperationLimits::new(max_sse_event_size, timeout)?,
        })
    }
}

impl McpOperationFactory for McpStdioOperationFactory {
    fn prepare(
        &self,
        config: &McpServerConfig,
        operation: McpOperation,
    ) -> Result<PreparedCall, ToolError> {
        let McpServerConfig::Stdio(config) = config else {
            return Err(prepare_error("stdio MCP factory received HTTP config"));
        };
        prepare_stdio(config, operation, self.limits.timeout).map_err(mcp_prepare_error)
    }

    fn execute<'a>(
        &'a self,
        config: &'a McpServerConfig,
        call: PreparedCall,
        io: &'a dyn GuardedIo,
    ) -> futures::future::BoxFuture<'a, Result<ToolExecution, ToolError>> {
        Box::pin(async move {
            let McpServerConfig::Stdio(config) = config else {
                return Err(execution_error("stdio MCP factory received HTTP config"));
            };
            execute_stdio(
                config,
                call,
                io,
                Arc::clone(&self.continuations),
                self.limits,
            )
            .await
            .map_err(mcp_execution_error)
        })
    }
}

fn prepare_http(config: &McpHttpConfig, operation: McpOperation) -> Result<PreparedCall, McpError> {
    let server = server_name(operation.subject().server())?;
    let profile = AdmittedMcpHttpProfile::initial(&server, config.url.as_url())?;
    let route = profile.route(config.url.as_url())?;
    let transport = ForeignTransportTarget::http(route);
    let fingerprint =
        http_transport_fingerprint(profile.fingerprint(), operation.admission(), &transport);
    PreparedCall::mcp(
        operation.subject().clone(),
        operation.operation(),
        operation.invocations().to_vec(),
        operation.admission().clone(),
        fingerprint,
        transport,
        operation.parameters(),
    )
    .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))
}

fn prepare_stdio(
    config: &McpStdioConfig,
    operation: McpOperation,
    timeout: Duration,
) -> Result<PreparedCall, McpError> {
    let mut argv = Vec::with_capacity(config.args.len() + 1);
    argv.push(OsString::from(&config.command));
    argv.extend(config.args.iter().map(OsString::from));
    let cwd = config.cwd.clone().map(CanonPath::new);
    let env = EnvPolicy::allow_only(config.inherit_env.iter().map(|name| name.as_str()))
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
    let spec = SpawnSpec::new(argv, cwd, Some(timeout), env)
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
    let fingerprint = stdio_transport_fingerprint(config, operation.admission(), &spec);
    PreparedCall::mcp_stdio(
        operation.subject().clone(),
        operation.operation(),
        operation.invocations().to_vec(),
        operation.admission().clone(),
        fingerprint,
        operation.subject().server(),
        spec,
        operation.parameters(),
    )
    .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))
}

async fn execute_http(
    config: &McpHttpConfig,
    call: PreparedCall,
    io: &dyn GuardedIo,
    management: Arc<dyn McpHttpEffects>,
    continuations: Arc<dyn McpInputRequiredRecorder>,
    limits: OperationLimits,
) -> Result<ToolExecution, McpError> {
    let envelope = call
        .mcp_envelope()
        .cloned()
        .ok_or_else(|| authority("selected MCP call has no protocol envelope"))?;
    let server = server_name(envelope.subject().server())?;
    let profile = AdmittedMcpHttpProfile::initial(&server, config.url.as_url())?;
    let route = profile.route(config.url.as_url())?;
    let transport = ForeignTransportTarget::http(route.clone());
    let fingerprint =
        http_transport_fingerprint(profile.fingerprint(), envelope.admission(), &transport);
    if envelope.transport() != &transport || envelope.transport_fingerprint() != fingerprint {
        return Err(authority("selected MCP HTTP transport fingerprint changed"));
    }
    let request = operation_request(&envelope)?;
    let expected_wire_params = request.wire_params()?;
    let claim = io.claim_mcp_http(call).map_err(McpError::from)?;
    if claim.envelope() != &envelope {
        return Err(authority("guarded MCP HTTP claim changed its envelope"));
    }
    let broker = Arc::new(SplitMcpHttpEffects::new(
        management,
        claim,
        server.clone(),
        profile.fingerprint(),
        route,
        envelope.clone(),
        expected_wire_params,
    ));
    let effects: Arc<dyn McpHttpEffects> = broker.clone();
    let mut client = McpClient::connect_http(server, config.clone(), limits.options()?, effects)?;
    if let Err(error) = client.discover().await {
        let _ = client.settle().await;
        return Err(error);
    }
    let outcome = request.invoke(&mut client).await;
    let settlement = client.settle().await;
    let outcome = outcome?;
    settlement?;
    let selected = broker
        .selected_settlement()?
        .ok_or_else(|| protocol("selected MCP HTTP operation was not settled"))?;
    project_outcome(
        outcome,
        &envelope,
        io,
        continuations.as_ref(),
        selected.delivery(),
        cancellation_from_http(selected.end()),
    )
}

async fn execute_stdio(
    config: &McpStdioConfig,
    call: PreparedCall,
    io: &dyn GuardedIo,
    continuations: Arc<dyn McpInputRequiredRecorder>,
    limits: OperationLimits,
) -> Result<ToolExecution, McpError> {
    let envelope = call
        .mcp_envelope()
        .cloned()
        .ok_or_else(|| authority("selected MCP call has no protocol envelope"))?;
    let server = server_name(envelope.subject().server())?;
    let spec = call
        .ops
        .iter()
        .find_map(|op| match op {
            ego_core::permit::Op::Spawn { spec } => Some(spec),
            _ => None,
        })
        .ok_or_else(|| authority("selected MCP stdio call has no spawn authority"))?;
    let expected = stdio_transport_fingerprint(config, envelope.admission(), spec);
    if envelope.transport_fingerprint() != expected
        || envelope.transport().to_string() != format!("stdio:{server}")
    {
        return Err(authority(
            "selected MCP stdio transport fingerprint changed",
        ));
    }
    let request = operation_request(&envelope)?;
    let duplex = io.guarded_duplex(call).await.map_err(McpError::from)?;
    let effects: Box<dyn McpDuplexEffects> = Box::new(ClaimedDuplexEffects {
        server: server.clone(),
        duplex: Some(duplex),
    });
    let mut client = McpClient::connect_stdio(server, limits.options()?, effects)?;
    if let Err(error) = client.discover().await {
        let _ = client.settle().await;
        return Err(error);
    }
    let outcome = request.invoke(&mut client).await;
    let settlement = client.settle().await;
    let outcome = outcome?;
    let settlement = settlement?;
    let duplex = settlement
        .duplex()
        .ok_or_else(|| protocol("selected MCP stdio operation has no settlement"))?;
    project_outcome(
        outcome,
        &envelope,
        io,
        continuations.as_ref(),
        duplex.delivery(),
        cancellation_from_duplex(duplex),
    )
}

enum OperationRequest {
    Tool(CallToolRequestParams),
    Resource(ReadResourceRequestParams),
    Prompt(GetPromptRequestParams),
}

impl OperationRequest {
    fn wire_params(&self) -> Result<Value, McpError> {
        match self {
            Self::Tool(params) => serde_json::to_value(params),
            Self::Resource(params) => serde_json::to_value(params),
            Self::Prompt(params) => serde_json::to_value(params),
        }
        .map_err(McpError::from)
    }

    async fn invoke(self, client: &mut McpClient) -> Result<OperationOutcome, McpError> {
        match self {
            Self::Tool(params) => client
                .call_tool_once(params)
                .await
                .map(|outcome| map_manual(outcome, CompleteResult::Tool)),
            Self::Resource(params) => client
                .read_resource_once(params)
                .await
                .map(|outcome| map_manual(outcome, CompleteResult::Resource)),
            Self::Prompt(params) => client
                .get_prompt_once(params)
                .await
                .map(|outcome| map_manual(outcome, CompleteResult::Prompt)),
        }
    }
}

fn operation_request(
    envelope: &ego_core::permit::McpPreparedEnvelope,
) -> Result<OperationRequest, McpError> {
    let parameters: Value = serde_json::from_slice(envelope.parameters())?;
    let arguments = parameters.as_object().cloned().ok_or_else(|| {
        McpError::new(
            McpFailure::InvalidValue,
            "captured MCP operation parameters are not an object",
        )
    })?;
    match (envelope.operation(), envelope.subject()) {
        (McpOperationKind::CallTool, McpSubject::Tool { name, .. }) => Ok(OperationRequest::Tool(
            CallToolRequestParams::new(name.clone()).with_arguments(arguments),
        )),
        (McpOperationKind::ReadResource, McpSubject::Resource { uri, .. }) => {
            if !arguments.is_empty() {
                return Err(McpError::new(
                    McpFailure::InvalidValue,
                    "resources/read does not accept captured arguments",
                ));
            }
            Ok(OperationRequest::Resource(ReadResourceRequestParams::new(
                uri.clone(),
            )))
        }
        (McpOperationKind::GetPrompt, McpSubject::Prompt { name, .. }) => {
            Ok(OperationRequest::Prompt(
                GetPromptRequestParams::new(name.clone()).with_arguments(arguments),
            ))
        }
        _ => Err(authority(
            "captured MCP operation subject does not match method",
        )),
    }
}

enum CompleteResult {
    Tool(CallToolResult),
    Resource(ReadResourceResult),
    Prompt(GetPromptResult),
}

enum OperationOutcome {
    Complete(CompleteResult),
    InputRequired(crate::McpMappedInput),
}

fn map_manual<T>(
    outcome: McpManualOutcome<T>,
    complete: impl FnOnce(T) -> CompleteResult,
) -> OperationOutcome {
    match outcome {
        McpManualOutcome::Complete(result) => OperationOutcome::Complete(complete(result)),
        McpManualOutcome::InputRequired(required) => OperationOutcome::InputRequired(required),
    }
}

fn project_outcome(
    outcome: OperationOutcome,
    envelope: &ego_core::permit::McpPreparedEnvelope,
    io: &dyn GuardedIo,
    continuations: &dyn McpInputRequiredRecorder,
    delivery: DeliveryCertainty,
    cancellation: CancellationClassification,
) -> Result<ToolExecution, McpError> {
    match outcome {
        OperationOutcome::InputRequired(required) => {
            let resume = io
                .take_mcp_resume()
                .map_err(|error| McpError::new(McpFailure::Transport, error.to_string()))?;
            continuations
                .record(envelope, required, resume)
                .map(ToolExecution::InputRequired)
                .map_err(|error| McpError::new(McpFailure::Transport, error.to_string()))
        }
        OperationOutcome::Complete(result) => {
            let projected = project_complete(result)?;
            let evidence = io
                .store_evidence(&projected.evidence)
                .map_err(|error| McpError::new(McpFailure::Transport, error.to_string()))?;
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
                delivery,
                cancellation,
            )
            .map_err(|error| McpError::new(McpFailure::Protocol, error.to_string()))?;
            Ok(ToolExecution::Complete(ToolResult::from_foreign(foreign)))
        }
    }
}

struct CompleteProjection {
    evidence: Vec<u8>,
    result_type: String,
    ttl_ms: Option<u64>,
    cache_scope: Option<CoreCacheScope>,
    content: Vec<ForeignContent>,
}

fn project_complete(result: CompleteResult) -> Result<CompleteProjection, McpError> {
    match result {
        CompleteResult::Tool(result) => {
            let result_type = project_result_type(result.result_type.as_ref())?;
            let evidence = serde_json::to_vec(&result)?;
            let mut content = result
                .content
                .iter()
                .map(project_content_block)
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(structured) = &result.structured_content {
                let bytes = serde_json::to_vec(structured)?;
                content.push(ForeignContent::unsupported("application/json", &bytes));
            }
            Ok(CompleteProjection {
                evidence,
                result_type,
                ttl_ms: None,
                cache_scope: None,
                content,
            })
        }
        CompleteResult::Resource(result) => {
            let result_type = project_result_type(result.result_type.as_ref())?;
            let evidence = serde_json::to_vec(&result)?;
            let content = result
                .contents
                .iter()
                .map(project_serialized_content)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CompleteProjection {
                evidence,
                result_type,
                ttl_ms: result.ttl_ms,
                cache_scope: map_cache_scope(result.cache_scope)?,
                content,
            })
        }
        CompleteResult::Prompt(result) => {
            let result_type = project_result_type(result.result_type.as_ref())?;
            let evidence = serde_json::to_vec(&result)?;
            let content = result
                .messages
                .iter()
                .map(|message| project_content_block(&message.content))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CompleteProjection {
                evidence,
                result_type,
                ttl_ms: None,
                cache_scope: None,
                content,
            })
        }
    }
}

fn project_result_type(value: Option<&rmcp::model::ResultType>) -> Result<String, McpError> {
    let value = value.ok_or_else(|| protocol("selected MCP result has no resultType"))?;
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| protocol("selected MCP resultType is not a string"))
}

fn project_content_block(block: &ContentBlock) -> Result<ForeignContent, McpError> {
    if let Some(text) = block.as_text() {
        return Ok(ForeignContent::text(text.text.clone()));
    }
    project_serialized_content(block)
}

fn project_serialized_content(value: &impl Serialize) -> Result<ForeignContent, McpError> {
    let value = serde_json::to_value(value)?;
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return Ok(ForeignContent::text(text.to_owned()));
    }
    let content_type = value
        .get("mimeType")
        .and_then(Value::as_str)
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or("application/octet-stream");
    let bytes = serde_json::to_vec(&value)?;
    Ok(ForeignContent::unsupported(content_type, &bytes))
}

fn map_cache_scope(scope: Option<CacheScope>) -> Result<Option<CoreCacheScope>, McpError> {
    match scope {
        None => Ok(None),
        Some(CacheScope::Public) => Ok(Some(CoreCacheScope::Public)),
        Some(CacheScope::Private) => Ok(Some(CoreCacheScope::Private)),
        Some(_) => Err(protocol("unknown MCP cache scope")),
    }
}

struct ClaimedDuplexEffects {
    server: McpServerName,
    duplex: Option<DuplexHandle>,
}

impl McpDuplexEffects for ClaimedDuplexEffects {
    fn open(
        mut self: Box<Self>,
        server: McpServerName,
    ) -> McpFuture<'static, Result<DuplexHandle, McpError>> {
        if server != self.server {
            return Box::pin(future::ready(Err(authority(
                "guarded MCP duplex server changed",
            ))));
        }
        Box::pin(future::ready(self.duplex.take().ok_or_else(|| {
            authority("guarded MCP duplex authority was already spent")
        })))
    }
}

struct SelectedHttpState {
    claim: Option<McpHttpClaim>,
    request_id: Option<McpRequestId>,
    delivery: DeliveryCertainty,
    settlement: Option<McpHttpSettlement>,
}

struct SplitMcpHttpEffects {
    management: Arc<dyn McpHttpEffects>,
    state: Arc<Mutex<SelectedHttpState>>,
    server: McpServerName,
    profile_fingerprint: [u8; 32],
    route: HttpRoute,
    envelope: ego_core::permit::McpPreparedEnvelope,
    expected_wire_params: Value,
}

enum HttpLane {
    Management,
    Selected,
}

impl SplitMcpHttpEffects {
    #[allow(clippy::too_many_arguments)]
    fn new(
        management: Arc<dyn McpHttpEffects>,
        claim: McpHttpClaim,
        server: McpServerName,
        profile_fingerprint: [u8; 32],
        route: HttpRoute,
        envelope: ego_core::permit::McpPreparedEnvelope,
        expected_wire_params: Value,
    ) -> Self {
        Self {
            management,
            state: Arc::new(Mutex::new(SelectedHttpState {
                claim: Some(claim),
                request_id: None,
                delivery: DeliveryCertainty::DefinitelyUnsent,
                settlement: None,
            })),
            server,
            profile_fingerprint,
            route,
            envelope,
            expected_wire_params,
        }
    }

    fn selected_settlement(&self) -> Result<Option<McpHttpSettlement>, McpError> {
        self.state
            .lock()
            .map(|state| state.settlement.clone())
            .map_err(|_| protocol("selected MCP HTTP settlement lock poisoned"))
    }

    fn lane(&self, call: &PreparedMcpHttpCall) -> Result<HttpLane, McpError> {
        let body: Value = serde_json::from_slice(call.request().body())?;
        let method = body
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| authority("MCP HTTP POST has no request method"))?;
        if method == self.envelope.operation().method() {
            return Ok(HttpLane::Selected);
        }
        if matches!(
            method,
            "initialize"
                | "notifications/initialized"
                | "notifications/cancelled"
                | "ping"
                | "tools/list"
                | "resources/list"
                | "resources/templates/list"
                | "prompts/list"
        ) {
            return Ok(HttpLane::Management);
        }
        Err(authority(
            "MCP HTTP method is neither admitted management nor selected",
        ))
    }

    fn validate_selected(&self, parts: &McpPreparedHttpParts) -> Result<(), McpError> {
        if parts.server != self.server
            || parts.purpose != McpHttpPurpose::Protocol
            || parts.profile_fingerprint != self.profile_fingerprint
            || parts.route != self.route
        {
            return Err(authority("selected MCP HTTP profile changed"));
        }
        let body: Value = serde_json::from_slice(parts.request.body())?;
        let params = body.get("params").cloned().unwrap_or(Value::Null);
        if body.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || body.get("id").is_none()
            || body.get("method").and_then(Value::as_str)
                != Some(self.envelope.operation().method())
            || params != self.expected_wire_params
        {
            return Err(authority("selected MCP HTTP method or parameters changed"));
        }
        let transport = ForeignTransportTarget::http(parts.route.clone());
        let fingerprint = http_transport_fingerprint(
            parts.profile_fingerprint,
            self.envelope.admission(),
            &transport,
        );
        if self.envelope.subject().server() != parts.server.as_str()
            || self.envelope.transport() != &transport
            || self.envelope.transport_fingerprint() != fingerprint
        {
            return Err(authority("selected MCP HTTP authority changed"));
        }
        Ok(())
    }

    fn settle_selected(
        &self,
        request_id: McpRequestId,
        end: McpHttpEnd,
    ) -> Result<McpHttpSettlement, McpError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| protocol("selected MCP HTTP settlement lock poisoned"))?;
        if state.request_id.as_ref() != Some(&request_id) {
            return Err(authority("selected MCP HTTP request identity changed"));
        }
        if let Some(settlement) = &state.settlement {
            return Ok(settlement.clone());
        }
        let settlement = McpHttpSettlement::new(request_id, end, state.delivery)?;
        state.settlement = Some(settlement.clone());
        Ok(settlement)
    }
}

impl McpHttpEffects for SplitMcpHttpEffects {
    fn execute(
        &self,
        call: PreparedMcpHttpCall,
    ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
        let lane = match self.lane(&call) {
            Ok(lane) => lane,
            Err(error) => return Box::pin(future::ready(Err(error))),
        };
        if matches!(lane, HttpLane::Management) {
            return self.management.execute(call);
        }
        let parts = call.into_parts();
        if let Err(error) = self.validate_selected(&parts) {
            return Box::pin(future::ready(Err(error)));
        }
        let claim = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => {
                    return Box::pin(future::ready(Err(protocol(
                        "selected MCP HTTP authority lock poisoned",
                    ))));
                }
            };
            if state.request_id.is_some() {
                return Box::pin(future::ready(Err(authority(
                    "selected MCP HTTP authority was already spent",
                ))));
            }
            state.request_id = Some(parts.request_id.clone());
            state.delivery = DeliveryCertainty::PossiblySent;
            match state.claim.take() {
                Some(claim) => claim,
                None => {
                    return Box::pin(future::ready(Err(authority(
                        "selected MCP HTTP authority was already spent",
                    ))));
                }
            }
        };
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let response = claim.exchange(parts.request, Some(parts.deadline)).await;
            let delivery = match &response {
                Ok(response) => response.delivery(),
                Err(error) => error.delivery(),
            };
            if let Ok(mut selected) = state.lock() {
                selected.delivery = delivery;
            }
            one_hop_response(response.map_err(McpError::from)?)
        })
    }

    fn cancel(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let selected = self
            .state
            .lock()
            .is_ok_and(|state| state.request_id.as_ref() == Some(&request_id));
        if selected {
            Box::pin(future::ready(
                self.settle_selected(request_id, McpHttpEnd::Cancelled),
            ))
        } else {
            self.management.cancel(request_id)
        }
    }

    fn timeout(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let selected = self
            .state
            .lock()
            .is_ok_and(|state| state.request_id.as_ref() == Some(&request_id));
        if selected {
            Box::pin(future::ready(
                self.settle_selected(request_id, McpHttpEnd::TimedOut),
            ))
        } else {
            self.management.timeout(request_id)
        }
    }

    fn settle(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let selected = self
            .state
            .lock()
            .is_ok_and(|state| state.request_id.as_ref() == Some(&request_id));
        if selected {
            Box::pin(future::ready(
                self.settle_selected(request_id, McpHttpEnd::Completed),
            ))
        } else {
            self.management.settle(request_id)
        }
    }
}

fn one_hop_response(response: OneHopResponse) -> Result<McpHttpResponse, McpError> {
    match response {
        OneHopResponse::Response {
            status,
            headers,
            body,
            delivery,
            ..
        } => McpHttpResponse::response(status, headers, body, delivery),
        OneHopResponse::Redirect {
            status,
            headers,
            location,
            delivery,
            ..
        } => Ok(McpHttpResponse::Redirect {
            status,
            headers,
            location,
            delivery,
        }),
    }
}

fn http_transport_fingerprint(
    profile: [u8; 32],
    admission: &McpAdmissionFingerprint,
    transport: &ForeignTransportTarget,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"ego:mcp:selected-http:v1");
    hash.update(profile);
    hash.update(admission.generation().as_bytes());
    hash.update([0]);
    hash.update(admission.operation().as_bytes());
    hash.update([0]);
    hash.update(admission.schema().as_bytes());
    hash.update([0]);
    hash.update(transport.to_string().as_bytes());
    format!("sha256:{:x}", hash.finalize())
}

fn stdio_transport_fingerprint(
    config: &McpStdioConfig,
    admission: &McpAdmissionFingerprint,
    spec: &SpawnSpec,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"ego:mcp:selected-stdio:v1");
    hash.update(admission.generation().as_bytes());
    hash.update([0]);
    hash.update(admission.operation().as_bytes());
    hash.update([0]);
    hash.update(admission.schema().as_bytes());
    hash.update([0]);
    hash.update(config.command.as_bytes());
    for argument in &config.args {
        hash.update([0]);
        hash.update(argument.as_bytes());
    }
    if let Some(cwd) = spec.cwd() {
        hash.update([0]);
        hash.update(cwd.to_string().as_bytes());
    }
    for name in spec.env_policy().names() {
        hash.update([0]);
        hash.update(name.to_string().as_bytes());
    }
    if let Some(timeout) = spec.timeout() {
        hash.update(timeout.as_secs().to_be_bytes());
        hash.update(timeout.subsec_nanos().to_be_bytes());
    }
    format!("sha256:{:x}", hash.finalize())
}

fn cancellation_from_http(end: McpHttpEnd) -> CancellationClassification {
    match end {
        McpHttpEnd::Completed => CancellationClassification::Completed,
        McpHttpEnd::Cancelled => CancellationClassification::Cancelled,
        McpHttpEnd::TimedOut => CancellationClassification::TimedOut,
        McpHttpEnd::Failed => CancellationClassification::Failed,
    }
}

fn cancellation_from_duplex(
    settlement: &ego_core::permit::DuplexSettlement,
) -> CancellationClassification {
    if settlement.cleanup() != &ego_core::permit::CleanupCertainty::Complete {
        return CancellationClassification::Uncertain;
    }
    match settlement.end() {
        DuplexEnd::Exited { code: 0 } => CancellationClassification::Completed,
        DuplexEnd::Cancelled => CancellationClassification::Cancelled,
        DuplexEnd::TimedOut { .. } => CancellationClassification::TimedOut,
        DuplexEnd::Failed | DuplexEnd::Exited { .. } | DuplexEnd::Signalled { .. } => {
            CancellationClassification::Failed
        }
        DuplexEnd::Interrupted => CancellationClassification::Uncertain,
    }
}

fn server_name(value: &str) -> Result<McpServerName, McpError> {
    value
        .parse()
        .map_err(|error: ego_core::config::McpValueError| {
            McpError::new(McpFailure::InvalidValue, error.to_string())
        })
}

fn prepare_error(message: impl Into<String>) -> ToolError {
    ToolError::InvalidParams(message.into())
}

fn execution_error(message: impl Into<String>) -> ToolError {
    ToolError::InvalidParams(format!("MCP execution failed: {}", message.into()))
}

fn mcp_prepare_error(error: McpError) -> ToolError {
    prepare_error(error.to_string())
}

fn mcp_execution_error(error: McpError) -> ToolError {
    execution_error(error.to_string())
}

fn authority(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::Authority, message)
}

fn protocol(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::Protocol, message)
}
