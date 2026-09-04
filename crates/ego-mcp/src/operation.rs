//! Fresh per-operation MCP lifecycles bound to guarded transport authority.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ego_core::config::{McpHttpConfig, McpServerConfig, McpServerName, McpStdioConfig};
use ego_core::event::hook::{HookLifecycle, HookOccurrenceKey, HookSubject};
use ego_core::event::id::Producer;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::hook::{EffectScope, HookCausality, HookEvent, HookOccurrence, SessionHookRuntime};
use ego_core::mcp::{
    CacheScope as CoreCacheScope, CancellationClassification, ForeignContent, ForeignInputRequired,
    ForeignQuestionPort, ForeignResult, McpAdmissionFingerprint, McpOperationKind, McpSubject,
    ToolExecution,
};
use ego_core::permit::{
    Authorized, CanonPath, DuplexEnd, DuplexHandle, EnvPolicy, ForeignTransportTarget, GuardedIo,
    HttpHeader, HttpRoute, InvocationDigest, McpHttpClaim, OneHopResponse, PreparedCall, SpawnSpec,
    ToolError, ToolResult,
};
use ego_core::suspension::{
    ContinuationError, ContinuationJournal, ContinuationRecord, ContinuationState,
    GuardedMcpResume, McpResumeContext, OpenedContinuation, PreparedResume, ResumeFloor,
    ResumeKeySource,
};
use ego_core::turn::ToolEffectPorts;
use futures::future;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResult, ReadResourceRequestParams, ReadResourceResult,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::dynamic::{McpCollapsedBootstrap, McpOperation, McpOperationFactory};
use crate::http::AdmittedMcpHttpProfile;
use crate::registry::{
    McpRegistryCapture, McpRegistryError, McpToolSchemaAdmission, McpToolSchemaCommand,
};
use crate::state::{compile_schema, validate_answer};
use crate::{
    McpClient, McpClientOptions, McpDuplexEffects, McpError, McpFailure, McpFuture,
    McpHookExecution, McpHttpEffects, McpHttpEnd, McpHttpPurpose, McpHttpResponse,
    McpHttpSettlement, McpManualOutcome, McpPreparedHttpParts, McpRequestId, McpServerAdmission,
    PreparedMcpHttpCall,
};

#[derive(Clone, Copy)]
struct OperationLimits {
    max_sse_event_size: usize,
    timeout: Duration,
}

#[derive(Clone)]
struct CollapsedSchemaAdmission {
    bootstrap: McpCollapsedBootstrap,
    admission: Arc<dyn McpToolSchemaAdmission>,
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

    fn hook_options(self, scope: &EffectScope) -> Result<McpClientOptions, McpError> {
        McpClientOptions::new(self.max_sse_event_size, scope.deadline())
    }
}

/// A fresh HTTP lifecycle. Management traffic uses its injected effect port;
/// the selected operation alone receives the guarded one-shot claim.
pub struct McpHttpOperationFactory {
    management: Arc<dyn McpHttpEffects>,
    continuations: Arc<McpContinuationOwner>,
    limits: OperationLimits,
    collapsed: Option<CollapsedSchemaAdmission>,
}

/// Exact immutable registry capture lookup owned by continuation recovery.
pub trait McpCaptureResolver: Send + Sync {
    fn resolve_capture(
        &self,
        stable_fingerprint: &str,
    ) -> Result<McpRegistryCapture, McpRegistryError>;
}

/// The sole owner of durable MCP input-required and accepted-answer rounds.
pub struct McpContinuationOwner {
    journal: Arc<dyn ContinuationJournal>,
    captures: Arc<dyn McpCaptureResolver>,
    keys: Arc<dyn ResumeKeySource>,
    clock: Arc<dyn Clock>,
    pinned: Mutex<BTreeMap<String, McpRegistryCapture>>,
}

impl McpContinuationOwner {
    pub fn new(
        journal: Arc<dyn ContinuationJournal>,
        captures: Arc<dyn McpCaptureResolver>,
        keys: Arc<dyn ResumeKeySource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            journal,
            captures,
            keys,
            clock,
            pinned: Mutex::new(BTreeMap::new()),
        }
    }

    pub async fn record_input_required(
        &self,
        envelope: ego_core::permit::McpPreparedEnvelope,
        required: crate::McpMappedInput,
        guarded: GuardedMcpResume<'_>,
    ) -> Result<ForeignInputRequired, McpError> {
        let (resume, context) = guarded.into_parts();
        let context =
            context.ok_or_else(|| authority("MCP input required has no session context"))?;
        if context.capture() != envelope.admission().capture() {
            return Err(authority(
                "MCP resume context capture does not match its operation",
            ));
        }
        let schema = serde_json::to_vec(required.schema())
            .map_err(|_| continuation(ContinuationError::InvalidRecord))?;
        let id = continuation_id(&envelope, &required, &schema);
        let record = resume
            .into_record(
                id.clone(),
                required.request_state().to_vec(),
                required.correlation_key(),
                schema,
                envelope.transport_fingerprint(),
            )
            .map_err(continuation)?;
        if record.capture() != context.capture()
            || record.fingerprints().capture() != context.capture()
            || record.subject().server() != envelope.subject().server()
            || record.subject().operation() != envelope.operation().method()
            || record.fingerprints().tool() != envelope.admission().operation()
            || record.fingerprints().schema() != envelope.admission().schema()
        {
            return Err(authority(
                "MCP resume authority does not match its session context",
            ));
        }
        let capture = self
            .captures
            .resolve_capture(context.capture())
            .map_err(|error| authority(error.to_string()))?;
        if capture.stable_fingerprint() != context.capture() {
            return Err(authority(
                "MCP capture resolver returned the wrong identity",
            ));
        }
        self.insert_pin(id.clone(), capture)?;
        if let Err(error) = self.journal.record(record) {
            self.remove_pin(&id)?;
            return Err(continuation(error));
        }
        Self::dispatch(
            &context.runtime(),
            context.causality().clone(),
            HookLifecycle::Elicitation,
            &id,
        )
        .await?;
        ForeignInputRequired::new(id, required.question_request().clone())
            .map_err(|_| continuation(ContinuationError::InvalidId))
    }

    pub async fn accept_answer<'session, 'turn>(
        &self,
        id: &str,
        raw_answer: &[u8],
        runtime: SessionHookRuntime<'session>,
        causality: HookCausality,
        floor: ResumeFloor,
        effects: ToolEffectPorts<'turn>,
    ) -> Result<ToolExecution, McpError> {
        let pending = self.journal.load(id).map_err(continuation)?;
        validate_recorded_answer(&pending, raw_answer)?;
        let capture = self.capture_for(id, pending.capture())?;
        let now = self.clock.now();
        if let Err(error) = self.journal.accept_answer(id, raw_answer, now) {
            if error.is_expired()
                || matches!(
                    error,
                    ContinuationError::DuplicateAnswer | ContinuationError::InvalidTransition
                )
            {
                self.remove_pin(id)?;
            }
            return Err(continuation(error));
        }
        let accepted = self
            .journal
            .load(id)
            .map_err(continuation)?
            .into_accepted()
            .map_err(continuation)?;
        let (server, factory, call, current_fingerprints) = capture.prepare_resume(
            accepted.subject(),
            accepted.fingerprints(),
            accepted.original_parameters(),
        )?;
        let opened =
            match accepted.open(call, &current_fingerprints, &floor, now, self.keys.as_ref()) {
                Ok(opened) => opened,
                Err(error) => {
                    if error.is_expired() {
                        self.remove_pin(id)?;
                    }
                    return Err(continuation(error));
                }
            };
        let expires_at = opened.expires_at();
        let admission = McpResumeAdmission::validate(opened)?;
        Self::dispatch(
            &runtime,
            causality.clone(),
            HookLifecycle::ElicitationResult,
            id,
        )
        .await?;
        let original_causality = original_tool_causality(&runtime, &causality)?;
        let claim_time = self.clock.now();
        let (authorized, successor) = match admission.into_retry_authorized_with_successor(
            floor,
            claim_time,
            self.keys.as_ref(),
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                if matches!(error.failure(), McpFailure::Authority)
                    && self.clock.now() >= expires_at
                {
                    self.remove_pin(id)?;
                }
                return Err(error);
            }
        };
        let claimed = match self.journal.claim(id, claim_time) {
            Ok(claimed) => claimed,
            Err(error) => {
                if error.is_expired()
                    || matches!(
                        error,
                        ContinuationError::DuplicateClaim | ContinuationError::InvalidTransition
                    )
                {
                    self.remove_pin(id)?;
                }
                return Err(continuation(error));
            }
        };
        let successor_context = McpResumeContext::new(runtime, capture.clone(), original_causality);
        let (call, guarded) =
            effects.bind_mcp_resume(authorized, successor, Some(successor_context));
        let generation: Arc<str> = Arc::from(capture.generation());
        match factory.execute(server, call, &guarded, generation).await {
            Ok(execution) => {
                let settlement = self
                    .journal
                    .settle(claimed, ContinuationState::Completed)
                    .map_err(continuation);
                self.remove_pin(id)?;
                settlement?;
                Ok(execution)
            }
            Err(error) => {
                let delivery = error.delivery().unwrap_or(DeliveryCertainty::PossiblySent);
                let state = if delivery == DeliveryCertainty::DefinitelyUnsent {
                    ContinuationState::Failed
                } else {
                    ContinuationState::Uncertain
                };
                let settlement = self.journal.settle(claimed, state).map_err(|settlement| {
                    McpError::with_delivery(
                        McpFailure::Transport,
                        format!("continued MCP settlement failed: {settlement}"),
                        delivery,
                    )
                });
                self.remove_pin(id)?;
                settlement?;
                Err(McpError::with_delivery(
                    McpFailure::Transport,
                    error.to_string(),
                    delivery,
                ))
            }
        }
    }

    fn insert_pin(&self, id: String, capture: McpRegistryCapture) -> Result<(), McpError> {
        let mut pinned = self
            .pinned
            .lock()
            .map_err(|_| authority("MCP continuation capture lock is unavailable"))?;
        if pinned.contains_key(&id) {
            return Err(authority("MCP continuation capture is already pinned"));
        }
        pinned.insert(id, capture);
        Ok(())
    }

    fn remove_pin(&self, id: &str) -> Result<(), McpError> {
        self.pinned
            .lock()
            .map_err(|_| authority("MCP continuation capture lock is unavailable"))?
            .remove(id);
        Ok(())
    }

    fn capture_for(
        &self,
        id: &str,
        stable_fingerprint: &str,
    ) -> Result<McpRegistryCapture, McpError> {
        if let Some(capture) = self
            .pinned
            .lock()
            .map_err(|_| authority("MCP continuation capture lock is unavailable"))?
            .get(id)
            .cloned()
        {
            if capture.stable_fingerprint() != stable_fingerprint {
                return Err(authority(
                    "pinned MCP capture does not match the continuation",
                ));
            }
            return Ok(capture);
        }
        let capture = self
            .captures
            .resolve_capture(stable_fingerprint)
            .map_err(|error| authority(error.to_string()))?;
        if capture.stable_fingerprint() != stable_fingerprint {
            return Err(authority(
                "MCP capture resolver returned the wrong identity",
            ));
        }
        self.insert_pin(id.to_owned(), capture.clone())?;
        Ok(capture)
    }

    async fn dispatch(
        runtime: &SessionHookRuntime<'_>,
        causality: HookCausality,
        lifecycle: HookLifecycle,
        id: &str,
    ) -> Result<(), McpError> {
        let item = causality
            .item_id()
            .ok_or_else(|| hook_failure("MCP continuation has no tool item causality"))?;
        let causality = match causality.owner() {
            ego_core::event::hook::HookOccurrenceOwner::Continuation { id: owner }
                if owner == id =>
            {
                causality
            }
            ego_core::event::hook::HookOccurrenceOwner::Continuation { .. } => {
                return Err(hook_failure(
                    "MCP continuation causality has the wrong owner",
                ));
            }
            _ => {
                let parent = causality.parent().ok_or_else(|| {
                    hook_failure("MCP continuation has no parent event causality")
                })?;
                HookCausality::continuation(id, causality.turn(), item, parent)
                    .map_err(|error| hook_failure(error.to_string()))?
            }
        };
        let key = HookOccurrenceKey::continuation(lifecycle, id)
            .map_err(|error| hook_failure(error.to_string()))?;
        let occurrence = HookOccurrence::new(
            key,
            HookEvent::turn(lifecycle, HookSubject::Item { item }),
            causality,
        )
        .map_err(|error| hook_failure(error.to_string()))?;
        let result = runtime
            .dispatch(Producer::Main, occurrence)
            .await
            .map_err(|error| hook_failure(error.to_string()))?;
        if !result.decision().is_continue() || !result.is_durable() {
            return Err(hook_failure(format!(
                "{} hook blocked or settled uncertain",
                lifecycle.as_str()
            )));
        }
        Ok(())
    }
}

/// Session-bound adapter from the core question consumer to the captured MCP
/// continuation owner and its original raw effect authority.
pub struct McpForeignQuestionPort<'session, 'turn> {
    owner: Arc<McpContinuationOwner>,
    runtime: SessionHookRuntime<'session>,
    floor: ResumeFloor,
    effects: ToolEffectPorts<'turn>,
}

impl<'session, 'turn> McpForeignQuestionPort<'session, 'turn> {
    pub fn new(
        owner: Arc<McpContinuationOwner>,
        runtime: SessionHookRuntime<'session>,
        floor: ResumeFloor,
        effects: ToolEffectPorts<'turn>,
    ) -> Self {
        Self {
            owner,
            runtime,
            floor,
            effects,
        }
    }
}

impl ForeignQuestionPort for McpForeignQuestionPort<'_, '_> {
    fn resume<'a>(
        &'a self,
        continuation_id: &'a str,
        canonical_json: &'a str,
        causality: HookCausality,
    ) -> futures::future::BoxFuture<'a, Result<ToolExecution, String>> {
        let owner = Arc::clone(&self.owner);
        let runtime = self.runtime.clone();
        let floor = self.floor.clone();
        let effects = self.effects.clone();
        Box::pin(async move {
            owner
                .accept_answer(
                    continuation_id,
                    canonical_json.as_bytes(),
                    runtime,
                    causality,
                    floor,
                    effects,
                )
                .await
                .map_err(|error| error.to_string())
        })
    }
}

fn original_tool_causality(
    runtime: &SessionHookRuntime<'_>,
    result: &HookCausality,
) -> Result<HookCausality, McpError> {
    let item = result
        .item_id()
        .ok_or_else(|| hook_failure("MCP continuation has no tool item causality"))?;
    runtime
        .hook_state()
        .inspect(|state| {
            state.items.get(&item).and_then(|tool| {
                tool.tool_call_started
                    .map(|parent| HookCausality::item(item, tool.turn, parent))
            })
        })
        .map_err(|error| hook_failure(error.to_string()))?
        .ok_or_else(|| hook_failure("MCP continuation lost its original tool causality"))
}

fn validate_recorded_answer(
    record: &ContinuationRecord,
    raw_answer: &[u8],
) -> Result<(), McpError> {
    let schema: Value = serde_json::from_slice(record.correlation_schema())?;
    let answer: Value = serde_json::from_slice(raw_answer)?;
    let validator = compile_schema(&schema)?;
    validate_answer(&validator, &answer)
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

    pub fn into_retry_authorized_with_successor(
        self,
        floor: ResumeFloor,
        now: Timestamp,
        keys: &dyn ResumeKeySource,
    ) -> Result<(Authorized, PreparedResume), McpError> {
        self.opened
            .into_retry_authorized_with_successor(floor, now, keys)
            .map_err(|error| {
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
    digest.update(envelope.admission().capture().as_bytes());
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
        continuations: Arc<McpContinuationOwner>,
        max_sse_event_size: usize,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        Ok(Self {
            management,
            continuations,
            limits: OperationLimits::new(max_sse_event_size, timeout)?,
            collapsed: None,
        })
    }

    pub(crate) fn runtime(
        management: Arc<dyn McpHttpEffects>,
        continuations: Arc<McpContinuationOwner>,
        max_sse_event_size: usize,
        timeout: Duration,
        bootstrap: Option<McpCollapsedBootstrap>,
        admission: Arc<dyn McpToolSchemaAdmission>,
    ) -> Result<Self, McpError> {
        Ok(Self {
            management,
            continuations,
            limits: OperationLimits::new(max_sse_event_size, timeout)?,
            collapsed: bootstrap.map(|bootstrap| CollapsedSchemaAdmission {
                bootstrap,
                admission,
            }),
        })
    }
}

impl McpOperationFactory for McpHttpOperationFactory {
    fn prepare(
        &self,
        admission: Arc<McpServerAdmission>,
        operation: McpOperation,
    ) -> Result<PreparedCall, ToolError> {
        let McpServerConfig::Http(config) = admission.config() else {
            return Err(prepare_error("HTTP MCP factory received stdio config"));
        };
        prepare_http(config, operation).map_err(mcp_prepare_error)
    }

    fn execute<'a, 'io>(
        &'a self,
        admission: Arc<McpServerAdmission>,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
        generation: Arc<str>,
    ) -> futures::future::BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            let McpServerConfig::Http(config) = admission.config() else {
                return Err(execution_error("HTTP MCP factory received stdio config"));
            };
            let config = config.clone();
            execute_http(&config, call, io, self, admission, generation)
                .await
                .map_err(mcp_execution_error)
        })
    }

    fn execute_hook<'a, 'io>(
        &'a self,
        admission: Arc<McpServerAdmission>,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
        scope: EffectScope,
    ) -> futures::future::BoxFuture<'a, Result<McpHookExecution, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            let McpServerConfig::Http(config) = admission.config() else {
                return Err(execution_error("HTTP MCP factory received stdio config"));
            };
            let config = config.clone();
            execute_http_hook(&config, call, io, self, admission, scope)
                .await
                .map_err(mcp_execution_error)
        })
    }
}

/// A fresh stdio lifecycle consuming one guarded owned duplex.
pub struct McpStdioOperationFactory {
    continuations: Arc<McpContinuationOwner>,
    limits: OperationLimits,
    collapsed: Option<CollapsedSchemaAdmission>,
}

impl McpStdioOperationFactory {
    pub fn new(
        continuations: Arc<McpContinuationOwner>,
        max_sse_event_size: usize,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        Ok(Self {
            continuations,
            limits: OperationLimits::new(max_sse_event_size, timeout)?,
            collapsed: None,
        })
    }

    pub(crate) fn runtime(
        continuations: Arc<McpContinuationOwner>,
        max_sse_event_size: usize,
        timeout: Duration,
        bootstrap: Option<McpCollapsedBootstrap>,
        admission: Arc<dyn McpToolSchemaAdmission>,
    ) -> Result<Self, McpError> {
        Ok(Self {
            continuations,
            limits: OperationLimits::new(max_sse_event_size, timeout)?,
            collapsed: bootstrap.map(|bootstrap| CollapsedSchemaAdmission {
                bootstrap,
                admission,
            }),
        })
    }
}

impl McpOperationFactory for McpStdioOperationFactory {
    fn prepare(
        &self,
        admission: Arc<McpServerAdmission>,
        operation: McpOperation,
    ) -> Result<PreparedCall, ToolError> {
        let McpServerConfig::Stdio(config) = admission.config() else {
            return Err(prepare_error("stdio MCP factory received HTTP config"));
        };
        prepare_stdio(admission.as_ref(), config, operation, self.limits.timeout)
            .map_err(mcp_prepare_error)
    }

    fn execute<'a, 'io>(
        &'a self,
        admission: Arc<McpServerAdmission>,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
        generation: Arc<str>,
    ) -> futures::future::BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            let McpServerConfig::Stdio(config) = admission.config() else {
                return Err(execution_error("stdio MCP factory received HTTP config"));
            };
            let config = config.clone();
            execute_stdio(&config, call, io, self, admission, generation)
                .await
                .map_err(mcp_execution_error)
        })
    }

    fn execute_hook<'a, 'io>(
        &'a self,
        admission: Arc<McpServerAdmission>,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
        scope: EffectScope,
    ) -> futures::future::BoxFuture<'a, Result<McpHookExecution, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async move {
            let McpServerConfig::Stdio(config) = admission.config() else {
                return Err(execution_error("stdio MCP factory received HTTP config"));
            };
            let config = config.clone();
            execute_stdio_hook(&config, call, io, self.limits, scope)
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
    admission: &McpServerAdmission,
    config: &McpStdioConfig,
    operation: McpOperation,
    timeout: Duration,
) -> Result<PreparedCall, McpError> {
    let spec = stdio_spawn_spec(admission, config, timeout)?;
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

pub(crate) fn stdio_spawn_spec(
    admission: &McpServerAdmission,
    config: &McpStdioConfig,
    timeout: Duration,
) -> Result<SpawnSpec, McpError> {
    let mut argv = Vec::with_capacity(config.args.len() + 1);
    argv.push(OsString::from(&config.command));
    argv.extend(config.args.iter().map(OsString::from));
    let cwd = config.cwd.clone().map(CanonPath::new);
    let environment = admission
        .stdio_environment()
        .ok_or_else(|| {
            McpError::new(
                McpFailure::Authority,
                "stdio MCP admission has no environment material",
            )
        })?
        .iter()
        .map(|binding| binding.explicit_binding())
        .collect::<Result<Vec<_>, McpError>>()?;
    let env = EnvPolicy::exact(environment)
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
    SpawnSpec::new(argv, cwd, Some(timeout), env)
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))
}

async fn execute_http(
    config: &McpHttpConfig,
    call: PreparedCall,
    io: &dyn GuardedIo<'_>,
    factory: &McpHttpOperationFactory,
    admission: Arc<McpServerAdmission>,
    generation: Arc<str>,
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
        Arc::clone(&factory.management),
        claim,
        server.clone(),
        profile.fingerprint(),
        route,
        envelope.clone(),
        expected_wire_params,
        admission_headers(admission.as_ref())?,
    ));
    let effects: Arc<dyn McpHttpEffects> = broker.clone();
    let mut client =
        McpClient::connect_http(server, config.clone(), factory.limits.options()?, effects)?;
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
    let schema = tool_schema_command(
        &outcome,
        &envelope,
        admission,
        &generation,
        factory.collapsed.as_ref(),
    )?;
    let execution = project_outcome(
        outcome,
        &envelope,
        io,
        factory.continuations.as_ref(),
        selected.delivery(),
        cancellation_from_http(selected.end()),
    )
    .await?;
    if let (Some(collapsed), Some(command)) = (factory.collapsed.as_ref(), schema) {
        collapsed.admission.reconcile(command).await?;
    }
    Ok(execution)
}

async fn execute_stdio(
    config: &McpStdioConfig,
    call: PreparedCall,
    io: &dyn GuardedIo<'_>,
    factory: &McpStdioOperationFactory,
    admission: Arc<McpServerAdmission>,
    generation: Arc<str>,
) -> Result<ToolExecution, McpError> {
    let envelope = call
        .mcp_envelope()
        .cloned()
        .ok_or_else(|| authority("selected MCP call has no protocol envelope"))?;
    let server = server_name(envelope.subject().server())?;
    let spec = call
        .ops()
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
    let mut client = McpClient::connect_stdio(server, factory.limits.options()?, effects)?;
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
    let schema = tool_schema_command(
        &outcome,
        &envelope,
        admission,
        &generation,
        factory.collapsed.as_ref(),
    )?;
    let execution = project_outcome(
        outcome,
        &envelope,
        io,
        factory.continuations.as_ref(),
        duplex.delivery(),
        cancellation_from_duplex(duplex),
    )
    .await?;
    if let (Some(collapsed), Some(command)) = (factory.collapsed.as_ref(), schema) {
        collapsed.admission.reconcile(command).await?;
    }
    Ok(execution)
}

async fn execute_http_hook(
    config: &McpHttpConfig,
    call: PreparedCall,
    io: &dyn GuardedIo<'_>,
    factory: &McpHttpOperationFactory,
    admission: Arc<McpServerAdmission>,
    scope: EffectScope,
) -> Result<McpHookExecution, McpError> {
    ensure_hook_scope(&scope)?;
    let envelope = call
        .mcp_envelope()
        .cloned()
        .ok_or_else(|| authority("selected MCP hook has no protocol envelope"))?;
    let server = server_name(envelope.subject().server())?;
    let profile = AdmittedMcpHttpProfile::initial(&server, config.url.as_url())?;
    let route = profile.route(config.url.as_url())?;
    let transport = ForeignTransportTarget::http(route.clone());
    let fingerprint =
        http_transport_fingerprint(profile.fingerprint(), envelope.admission(), &transport);
    if envelope.transport() != &transport || envelope.transport_fingerprint() != fingerprint {
        return Err(authority("selected MCP hook transport fingerprint changed"));
    }
    let request = operation_request(&envelope)?;
    if !matches!(&request, OperationRequest::Tool(_)) {
        return Err(authority("MCP hook operation is not a tool call"));
    }
    let expected_wire_params = request.wire_params()?;
    let claim = io.claim_mcp_http(call).map_err(McpError::from)?;
    if claim.envelope() != &envelope {
        return Err(authority("guarded MCP hook claim changed its envelope"));
    }
    let broker = Arc::new(SplitMcpHttpEffects::new(
        Arc::clone(&factory.management),
        claim,
        server.clone(),
        profile.fingerprint(),
        route,
        envelope,
        expected_wire_params,
        admission_headers(admission.as_ref())?,
    ));
    let effects: Arc<dyn McpHttpEffects> = broker.clone();
    let mut client = McpClient::connect_http(
        server,
        config.clone(),
        factory.limits.hook_options(&scope)?,
        effects,
    )?;
    if let Err(error) = client.discover().await {
        let _ = client.settle().await;
        return Err(error);
    }
    let outcome = invoke_in_scope(request, &mut client, &scope).await;
    let settlement = client.settle().await;
    let outcome = outcome?;
    settlement?;
    let selected = broker
        .selected_settlement()?
        .ok_or_else(|| protocol("selected MCP hook HTTP operation was not settled"))?;
    project_hook_outcome(outcome, io, selected.delivery())
}

async fn execute_stdio_hook(
    config: &McpStdioConfig,
    call: PreparedCall,
    io: &dyn GuardedIo<'_>,
    limits: OperationLimits,
    scope: EffectScope,
) -> Result<McpHookExecution, McpError> {
    ensure_hook_scope(&scope)?;
    let envelope = call
        .mcp_envelope()
        .cloned()
        .ok_or_else(|| authority("selected MCP hook has no protocol envelope"))?;
    let server = server_name(envelope.subject().server())?;
    let spec = call
        .ops()
        .iter()
        .find_map(|op| match op {
            ego_core::permit::Op::Spawn { spec } => Some(spec),
            _ => None,
        })
        .ok_or_else(|| authority("selected MCP hook has no spawn authority"))?;
    let expected = stdio_transport_fingerprint(config, envelope.admission(), spec);
    if envelope.transport_fingerprint() != expected
        || envelope.transport().to_string() != format!("stdio:{server}")
    {
        return Err(authority("selected MCP hook transport fingerprint changed"));
    }
    let request = operation_request(&envelope)?;
    if !matches!(&request, OperationRequest::Tool(_)) {
        return Err(authority("MCP hook operation is not a tool call"));
    }
    let duplex = io.guarded_duplex(call).await.map_err(McpError::from)?;
    let effects: Box<dyn McpDuplexEffects> = Box::new(ClaimedDuplexEffects {
        server: server.clone(),
        duplex: Some(duplex),
    });
    let mut client = McpClient::connect_stdio(server, limits.hook_options(&scope)?, effects)?;
    if let Err(error) = client.discover().await {
        let _ = client.settle().await;
        return Err(error);
    }
    let outcome = invoke_in_scope(request, &mut client, &scope).await;
    let settlement = client.settle().await;
    let outcome = outcome?;
    let settlement = settlement?;
    let duplex = settlement
        .duplex()
        .ok_or_else(|| protocol("selected MCP hook stdio operation has no settlement"))?;
    project_hook_outcome(outcome, io, duplex.delivery())
}

async fn invoke_in_scope(
    request: OperationRequest,
    client: &mut McpClient,
    scope: &EffectScope,
) -> Result<OperationOutcome, McpError> {
    let cancellation = client.cancellation();
    let mut invoke = Box::pin(request.invoke(client));
    loop {
        tokio::select! {
            outcome = &mut invoke => return outcome,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(scope.deadline())) => {
                cancellation.cancel();
                return Err(McpError::new(
                    McpFailure::Transport,
                    "MCP hook deadline elapsed",
                ));
            }
            _ = tokio::time::sleep(Duration::from_millis(2)) => {
                if scope.is_cancelled() {
                    cancellation.cancel();
                    return Err(McpError::new(
                        McpFailure::Cancelled,
                        "MCP hook was cancelled",
                    ));
                }
            }
        }
    }
}

fn project_hook_outcome(
    outcome: OperationOutcome,
    io: &dyn GuardedIo<'_>,
    delivery: DeliveryCertainty,
) -> Result<McpHookExecution, McpError> {
    let OperationOutcome::Complete(CompleteResult::Tool(result)) = outcome else {
        return Err(protocol("MCP hook did not return a complete tool result"));
    };
    if result.is_error == Some(true)
        || !result
            .result_type
            .as_ref()
            .is_some_and(rmcp::model::ResultType::is_complete)
    {
        return Err(protocol(
            "MCP hook tool result was not successful and complete",
        ));
    }
    let evidence_bytes = serde_json::to_vec(&result)?;
    let evidence = io
        .store_evidence(&evidence_bytes)
        .map_err(|error| McpError::new(McpFailure::Transport, error.to_string()))?;
    if result.structured_content.is_none() {
        return Err(protocol("MCP hook result has no structuredContent"));
    }
    Ok(McpHookExecution::new(result, evidence, delivery))
}

fn ensure_hook_scope(scope: &EffectScope) -> Result<(), McpError> {
    if scope.is_cancelled() {
        return Err(McpError::new(
            McpFailure::Cancelled,
            "MCP hook was cancelled",
        ));
    }
    if Instant::now() >= scope.deadline() {
        return Err(McpError::new(
            McpFailure::Transport,
            "MCP hook deadline elapsed",
        ));
    }
    Ok(())
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
        McpManualOutcome::InputRequired(required) => OperationOutcome::InputRequired(*required),
    }
}

fn tool_schema_command(
    outcome: &OperationOutcome,
    envelope: &ego_core::permit::McpPreparedEnvelope,
    server_admission: Arc<McpServerAdmission>,
    generation: &str,
    collapsed: Option<&CollapsedSchemaAdmission>,
) -> Result<Option<McpToolSchemaCommand>, McpError> {
    let Some(collapsed) = collapsed else {
        return Ok(None);
    };
    let McpSubject::Tool { name, .. } = envelope.subject() else {
        return Ok(None);
    };
    if name != collapsed.bootstrap.lookup_name() {
        return Ok(None);
    }
    let OperationOutcome::Complete(CompleteResult::Tool(result)) = outcome else {
        return Ok(None);
    };
    if result.is_error == Some(true)
        || !result
            .result_type
            .as_ref()
            .is_some_and(rmcp::model::ResultType::is_complete)
    {
        return Err(protocol(
            "collapsed schema lookup did not return a successful complete result",
        ));
    }
    let structured_content = result
        .structured_content
        .clone()
        .ok_or_else(|| protocol("collapsed schema lookup result has no structuredContent"))?;
    let parameters: Value = serde_json::from_slice(envelope.parameters())?;
    let requested_name = collapsed.bootstrap.requested_name(&parameters)?;
    Ok(Some(McpToolSchemaCommand::new(
        server_admission,
        generation.to_string(),
        envelope.admission().clone(),
        requested_name,
        structured_content,
    )))
}

async fn project_outcome(
    outcome: OperationOutcome,
    envelope: &ego_core::permit::McpPreparedEnvelope,
    io: &dyn GuardedIo<'_>,
    continuations: &McpContinuationOwner,
    delivery: DeliveryCertainty,
    cancellation: CancellationClassification,
) -> Result<ToolExecution, McpError> {
    match outcome {
        OperationOutcome::InputRequired(required) => {
            let resume = io
                .take_mcp_resume()
                .map_err(|error| McpError::new(McpFailure::Transport, error.to_string()))?;
            continuations
                .record_input_required(envelope.clone(), required, resume)
                .await
                .map(ToolExecution::InputRequired)
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
    headers: Arc<[HttpHeader]>,
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
        headers: Vec<HttpHeader>,
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
            headers: headers.into(),
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
        let call = match call.with_headers(&self.headers) {
            Ok(call) => call,
            Err(error) => return Box::pin(future::ready(Err(error))),
        };
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
    hash.update(admission.capture().as_bytes());
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
    hash.update(admission.capture().as_bytes());
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

fn admission_headers(admission: &McpServerAdmission) -> Result<Vec<HttpHeader>, McpError> {
    admission
        .http_headers()
        .ok_or_else(|| authority("HTTP MCP admission has no header material"))?
        .iter()
        .map(|binding| {
            HttpHeader::secret(
                binding.name(),
                ego_core::credentials::SecretMaterial::new(
                    binding.value().expose_secret_bytes().to_vec(),
                ),
            )
            .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))
        })
        .collect()
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
    ToolError::foreign(
        format!("MCP execution failed: {}", message.into()),
        DeliveryCertainty::DefinitelyUnsent,
    )
}

fn mcp_prepare_error(error: McpError) -> ToolError {
    prepare_error(error.to_string())
}

fn mcp_execution_error(error: McpError) -> ToolError {
    let delivery = error.delivery().unwrap_or(DeliveryCertainty::PossiblySent);
    ToolError::foreign(format!("MCP execution failed: {error}"), delivery)
}

fn continuation(error: ContinuationError) -> McpError {
    McpError::new(McpFailure::Authority, error.to_string())
}

fn hook_failure(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::Authority, message)
}

fn authority(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::Authority, message)
}

fn protocol(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::Protocol, message)
}
