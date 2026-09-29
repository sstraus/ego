//! Fresh per-operation MCP lifecycles bound to guarded transport authority.

use std::collections::{BTreeMap, HashMap};
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
use ego_core::turn::{
    GenerationPolicyCaptureStore, GenerationPolicyDigest, GenerationPolicySnapshot, ToolEffectPorts,
};
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

/// An effect-free recipe for a fresh HTTP lifecycle.
pub struct McpHttpOperationFactory {
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
    policy_captures: Arc<dyn GenerationPolicyCaptureStore>,
    captures: Arc<dyn McpCaptureResolver>,
    keys: Arc<dyn ResumeKeySource>,
    clock: Arc<dyn Clock>,
    pinned: Mutex<BTreeMap<String, PinnedContinuation>>,
}

#[derive(Clone)]
struct PinnedContinuation {
    capture: McpRegistryCapture,
    policy_snapshot: Arc<GenerationPolicySnapshot>,
}

impl McpContinuationOwner {
    pub fn new(
        journal: Arc<dyn ContinuationJournal>,
        policy_captures: Arc<dyn GenerationPolicyCaptureStore>,
        captures: Arc<dyn McpCaptureResolver>,
        keys: Arc<dyn ResumeKeySource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            journal,
            policy_captures,
            captures,
            keys,
            clock,
            pinned: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn load_pending_foreign_policy(
        &self,
        id: &str,
    ) -> Result<Arc<GenerationPolicySnapshot>, McpError> {
        let pending = self.load_pending_record(id)?;
        let digest = pending.generation_policy_digest().clone();
        let capture = pending.capture().to_owned();
        let snapshot = self
            .policy_captures
            .load(id, &digest)
            .map_err(|error| authority(error.diagnostic()))?;
        if snapshot.digest() != digest {
            return Err(authority(
                "MCP generation policy capture does not match its durable record",
            ));
        }
        self.revalidate_pending_authority(id, &digest, &capture)?;
        Ok(snapshot)
    }

    pub fn bind_pending_foreign_continuation(
        &self,
        id: &str,
        policy_snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<(), McpError> {
        let pending = self.load_pending_record(id)?;
        let digest = pending.generation_policy_digest().clone();
        let capture_identity = pending.capture().to_owned();
        if policy_snapshot.digest() != digest {
            return Err(authority(
                "MCP generation policy capture does not match its durable record",
            ));
        }
        let capture = self
            .captures
            .resolve_capture(&capture_identity)
            .map_err(|_| authority("MCP registry capture is unavailable"))?;
        if capture.stable_fingerprint() != capture_identity {
            return Err(authority(
                "MCP capture resolver returned the wrong identity",
            ));
        }
        self.revalidate_pending_authority(id, &digest, &capture_identity)?;
        self.insert_pin(id.to_owned(), capture, policy_snapshot)
    }

    pub async fn record_input_required(
        &self,
        envelope: ego_core::permit::McpPreparedEnvelope,
        required: crate::McpMappedInput,
        guarded: GuardedMcpResume<'_>,
    ) -> Result<ForeignInputRequired, McpError> {
        let (resume, context) = guarded.into_parts().map_err(continuation)?;
        let context =
            context.ok_or_else(|| authority("MCP input required has no session context"))?;
        if context.capture() != envelope.admission().capture() {
            return Err(authority(
                "MCP resume context capture does not match its operation",
            ));
        }
        let policy_snapshot = context.policy_snapshot();
        let policy_digest = policy_snapshot.digest();
        let schema = serde_json::to_vec(required.schema())
            .map_err(|_| continuation(ContinuationError::InvalidRecord))?;
        let id = continuation_id(&envelope, &required, &schema);
        let record = resume
            .into_record(
                id.clone(),
                policy_digest.clone(),
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
        self.insert_pin(id.clone(), capture, Arc::clone(&policy_snapshot))?;
        if let Err(error) = self
            .policy_captures
            .store(&id, Arc::clone(&policy_snapshot))
        {
            let _ = self.remove_pin(&id);
            return Err(authority(error.diagnostic()));
        }
        if let Err(error) = self.journal.record(record) {
            let _ = self.remove_pin(&id);
            return Err(continuation(error));
        }
        if let Err(primary) = Self::dispatch(
            &context.runtime(),
            context.causality().clone(),
            HookLifecycle::Elicitation,
            &id,
        )
        .await
        {
            return Err(self.terminalize_pending_failure(&id, &policy_digest, primary));
        }
        ForeignInputRequired::new(id.clone(), required.question_request().clone()).map_err(|_| {
            self.terminalize_pending_failure(
                &id,
                &policy_digest,
                continuation(ContinuationError::InvalidId),
            )
        })
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
        let policy_digest = pending.generation_policy_digest().clone();
        let pinned = self.capture_for(id, pending.capture())?;
        if pinned.policy_snapshot.digest() != policy_digest {
            return Err(authority(
                "MCP continuation generation policy does not match its durable record",
            ));
        }
        let capture = &pinned.capture;
        let now = self.clock.now();
        self.journal
            .accept_answer(id, raw_answer, now)
            .map_err(continuation)?;
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
        let opened = accepted
            .open(call, &current_fingerprints, &floor, now, self.keys.as_ref())
            .map_err(continuation)?;
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
        let (authorized, successor) = admission.into_retry_authorized_with_successor(
            floor,
            claim_time,
            self.keys.as_ref(),
        )?;
        let claimed = self.journal.claim(id, claim_time).map_err(continuation)?;
        let successor_context = McpResumeContext::new(
            runtime,
            capture.clone(),
            original_causality,
            Arc::clone(&pinned.policy_snapshot),
        );
        let (call, guarded) =
            effects.bind_mcp_resume(authorized, successor, Some(successor_context));
        let generation: Arc<str> = Arc::from(capture.generation());
        match factory.execute(server, call, &guarded, generation).await {
            Ok(execution) => {
                self.journal
                    .settle(claimed, ContinuationState::Completed)
                    .map_err(|_| settlement_failure(DeliveryCertainty::DefinitelySent))?;
                self.cleanup_terminal(id, &policy_digest);
                Ok(execution)
            }
            Err(error) => {
                let message = error.to_string();
                let delivery = error.delivery().unwrap_or(DeliveryCertainty::PossiblySent);
                let state = if delivery == DeliveryCertainty::DefinitelyUnsent {
                    ContinuationState::Failed
                } else {
                    ContinuationState::Uncertain
                };
                self.journal
                    .settle(claimed, state)
                    .map_err(|_| settlement_failure(delivery))?;
                self.cleanup_terminal(id, &policy_digest);
                Err(McpError::with_delivery(
                    McpFailure::Transport,
                    message,
                    delivery,
                ))
            }
        }
    }

    fn terminalize_pending_failure(
        &self,
        id: &str,
        digest: &GenerationPolicyDigest,
        primary: McpError,
    ) -> McpError {
        if self.journal.fail_pending(id).is_err() {
            return authority("MCP continuation pending failure could not be durably recorded");
        }
        self.cleanup_terminal(id, digest);
        primary
    }

    fn cleanup_terminal(&self, id: &str, digest: &GenerationPolicyDigest) {
        let _ = self.remove_pin(id);
        let _ = self.policy_captures.delete(id, digest);
    }

    fn load_pending_record(&self, id: &str) -> Result<ContinuationRecord, McpError> {
        let record = self
            .journal
            .load(id)
            .map_err(|_| authority("MCP pending continuation record is unavailable"))?;
        if record.state() != ContinuationState::Pending {
            return Err(authority("MCP continuation is not pending"));
        }
        Ok(record)
    }

    fn revalidate_pending_authority(
        &self,
        id: &str,
        digest: &GenerationPolicyDigest,
        capture: &str,
    ) -> Result<(), McpError> {
        let current = self.load_pending_record(id)?;
        if current.generation_policy_digest() != digest || current.capture() != capture {
            return Err(authority(
                "MCP pending continuation authority changed during recovery",
            ));
        }
        Ok(())
    }

    fn insert_pin(
        &self,
        id: String,
        capture: McpRegistryCapture,
        policy_snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<(), McpError> {
        let mut pinned = self
            .pinned
            .lock()
            .map_err(|_| authority("MCP continuation capture lock is unavailable"))?;
        if pinned.contains_key(&id) {
            return Err(authority("MCP continuation capture is already pinned"));
        }
        pinned.insert(
            id,
            PinnedContinuation {
                capture,
                policy_snapshot,
            },
        );
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
    ) -> Result<PinnedContinuation, McpError> {
        let pinned = self
            .pinned
            .lock()
            .map_err(|_| authority("MCP continuation capture lock is unavailable"))?
            .get(id)
            .cloned()
            .ok_or_else(|| authority("MCP continuation generation policy is unavailable"))?;
        if pinned.capture.stable_fingerprint() != stable_fingerprint {
            return Err(authority(
                "pinned MCP capture does not match the continuation",
            ));
        }
        Ok(pinned)
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
    let cwd = match &config.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().map_err(|_| {
            McpError::new(
                McpFailure::InvalidValue,
                "MCP stdio working directory cannot be resolved",
            )
        })?,
    };
    let cwd = std::fs::canonicalize(cwd).map_err(|_| {
        McpError::new(
            McpFailure::InvalidValue,
            "MCP stdio working directory cannot be resolved",
        )
    })?;
    if !cwd.is_dir() {
        return Err(McpError::new(
            McpFailure::InvalidValue,
            "MCP stdio working directory is not a directory",
        ));
    }
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
    SpawnSpec::new(argv, Some(CanonPath::new(cwd)), Some(timeout), env)
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
    request_id: Option<McpRequestId>,
    delivery: DeliveryCertainty,
    settlement: Option<McpHttpSettlement>,
    management: HashMap<McpRequestId, ManagedHttpRequest>,
}

struct ManagedHttpRequest {
    delivery: DeliveryCertainty,
    settlement: Option<McpHttpSettlement>,
}

struct SplitMcpHttpEffects {
    claim: McpHttpClaim,
    state: Arc<Mutex<SelectedHttpState>>,
    server: McpServerName,
    profile_fingerprint: [u8; 32],
    route: HttpRoute,
    envelope: ego_core::permit::McpPreparedEnvelope,
    expected_wire_params: Value,
    headers: Arc<[HttpHeader]>,
}

#[derive(Clone, Copy)]
enum HttpLane {
    Management,
    Selected,
}

impl SplitMcpHttpEffects {
    #[allow(clippy::too_many_arguments)]
    fn new(
        claim: McpHttpClaim,
        server: McpServerName,
        profile_fingerprint: [u8; 32],
        route: HttpRoute,
        envelope: ego_core::permit::McpPreparedEnvelope,
        expected_wire_params: Value,
        headers: Vec<HttpHeader>,
    ) -> Self {
        Self {
            claim,
            state: Arc::new(Mutex::new(SelectedHttpState {
                request_id: None,
                delivery: DeliveryCertainty::DefinitelyUnsent,
                settlement: None,
                management: HashMap::new(),
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
        self.validate_lifecycle(parts)?;
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

    fn validate_lifecycle(&self, parts: &McpPreparedHttpParts) -> Result<(), McpError> {
        if parts.server != self.server
            || parts.purpose != McpHttpPurpose::Protocol
            || parts.profile_fingerprint != self.profile_fingerprint
            || parts.route != self.route
        {
            return Err(authority("MCP HTTP lifecycle profile changed"));
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

    fn settle_management(
        &self,
        request_id: McpRequestId,
        end: McpHttpEnd,
    ) -> Result<McpHttpSettlement, McpError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| protocol("MCP HTTP lifecycle settlement lock poisoned"))?;
        let request = state
            .management
            .remove(&request_id)
            .ok_or_else(|| authority("MCP HTTP lifecycle request identity changed"))?;
        match request.settlement {
            Some(settlement) => Ok(settlement),
            None => McpHttpSettlement::new(request_id, end, request.delivery),
        }
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
        let parts = call.into_parts();
        let validation = match lane {
            HttpLane::Management => self.validate_lifecycle(&parts),
            HttpLane::Selected => self.validate_selected(&parts),
        };
        if let Err(error) = validation {
            return Box::pin(future::ready(Err(error)));
        }
        let request_id = parts.request_id.clone();
        {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => {
                    return Box::pin(future::ready(Err(protocol(
                        "selected MCP HTTP authority lock poisoned",
                    ))));
                }
            };
            match lane {
                HttpLane::Selected => {
                    if state.request_id.is_some() {
                        return Box::pin(future::ready(Err(authority(
                            "selected MCP HTTP authority was already spent",
                        ))));
                    }
                    state.request_id = Some(request_id.clone());
                    state.delivery = DeliveryCertainty::PossiblySent;
                }
                HttpLane::Management => {
                    if state
                        .management
                        .insert(
                            request_id.clone(),
                            ManagedHttpRequest {
                                delivery: DeliveryCertainty::PossiblySent,
                                settlement: None,
                            },
                        )
                        .is_some()
                    {
                        state.management.remove(&request_id);
                        return Box::pin(future::ready(Err(authority(
                            "MCP HTTP lifecycle request identity was reused",
                        ))));
                    }
                }
            }
        }
        let exchange = self.claim.exchange(parts.request, parts.deadline);
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let response = exchange.await;
            let delivery = match &response {
                Ok(response) => response.delivery(),
                Err(error) => error.delivery(),
            };
            if let Ok(mut state) = state.lock() {
                match lane {
                    HttpLane::Selected => state.delivery = delivery,
                    HttpLane::Management => {
                        if let Some(request) = state.management.get_mut(&request_id) {
                            request.delivery = delivery;
                            if response.is_err() {
                                request.settlement = McpHttpSettlement::new(
                                    request_id.clone(),
                                    McpHttpEnd::Failed,
                                    delivery,
                                )
                                .ok();
                            }
                        }
                    }
                }
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
        Box::pin(future::ready(if selected {
            self.settle_selected(request_id, McpHttpEnd::Cancelled)
        } else {
            self.settle_management(request_id, McpHttpEnd::Cancelled)
        }))
    }

    fn timeout(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let selected = self
            .state
            .lock()
            .is_ok_and(|state| state.request_id.as_ref() == Some(&request_id));
        Box::pin(future::ready(if selected {
            self.settle_selected(request_id, McpHttpEnd::TimedOut)
        } else {
            self.settle_management(request_id, McpHttpEnd::TimedOut)
        }))
    }

    fn settle(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        let selected = self
            .state
            .lock()
            .is_ok_and(|state| state.request_id.as_ref() == Some(&request_id));
        Box::pin(future::ready(if selected {
            self.settle_selected(request_id, McpHttpEnd::Completed)
        } else {
            self.settle_management(request_id, McpHttpEnd::Completed)
        }))
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

#[cfg(test)]
mod stdio_fingerprint_tests {
    use super::*;
    use ego_core::credentials::SecretMaterial;
    use ego_core::permit::{TransportError, TransportFailure};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DuplexProbe(AtomicBool);

    impl<'io> GuardedIo<'io> for DuplexProbe {
        fn write(&self, _: &CanonPath, _: &[u8]) -> Result<(), ego_core::permit::IoDenied> {
            unreachable!("stdio selection does not write a file")
        }

        fn read(&self, _: &CanonPath) -> Result<Vec<u8>, ego_core::permit::IoDenied> {
            unreachable!("stdio selection does not read a file")
        }

        fn list(
            &self,
            _: &ego_core::permit::ScanRequest,
        ) -> Result<ego_core::permit::Scan, ego_core::permit::IoDenied> {
            unreachable!("stdio selection does not list files")
        }

        fn search(
            &self,
            _: &ego_core::permit::ScanRequest,
            _: &dyn ego_core::permit::LineMatcher,
        ) -> Result<ego_core::permit::Found, ego_core::permit::IoDenied> {
            unreachable!("stdio selection does not search files")
        }

        fn exec<'a>(
            &'a self,
            _: &'a SpawnSpec,
        ) -> futures::future::BoxFuture<'a, Result<ToolResult, ego_core::permit::ProcessError>>
        {
            unreachable!("stdio selection uses guarded duplex")
        }

        fn read_evidence<'a>(
            &'a self,
            _: &'a ego_core::store::evidence::EvidenceRead,
        ) -> futures::future::BoxFuture<
            'a,
            Result<ego_core::store::evidence::EvidencePage, ToolError>,
        > {
            unreachable!("stdio selection does not read evidence")
        }

        fn guarded_http(
            &self,
            _: PreparedCall,
            _: ego_core::permit::HttpRequest,
        ) -> futures::future::BoxFuture<'static, Result<OneHopResponse, TransportError>> {
            unreachable!("stdio selection does not use HTTP")
        }

        fn claim_mcp_http(&self, _: PreparedCall) -> Result<McpHttpClaim, TransportError> {
            unreachable!("stdio selection does not use HTTP")
        }

        fn guarded_duplex(
            &self,
            _: PreparedCall,
        ) -> futures::future::BoxFuture<'static, Result<DuplexHandle, TransportError>> {
            self.0.store(true, Ordering::SeqCst);
            Box::pin(std::future::ready(Err(TransportError::definitely_unsent(
                TransportFailure::Exchange("duplex probe".into()),
            ))))
        }

        fn store_evidence(
            &self,
            _: &[u8],
        ) -> Result<ego_core::store::evidence::EvidenceRef, ToolError> {
            unreachable!("stdio selection does not store evidence")
        }
    }

    fn tuic_admission() -> McpServerAdmission {
        let environment = [
            ("TUIC_SOCKET", "/repo/mcp.sock"),
            ("TUIC_SESSION", "550e8400-e29b-41d4-a716-446655440a01"),
        ];
        let config = McpServerConfig::stdio(
            "/opt/tuic/tuic-bridge".into(),
            Vec::new(),
            None,
            environment
                .iter()
                .map(|(name, _)| name.parse().expect("TUIC variable name"))
                .collect(),
        )
        .expect("TUIC stdio config");
        let material = crate::McpTransportMaterial::stdio(
            environment
                .into_iter()
                .map(|(name, value)| {
                    crate::McpEnvironmentBinding::new(
                        name.parse().expect("TUIC variable name"),
                        SecretMaterial::new(value.as_bytes().to_vec()),
                    )
                })
                .collect(),
        );
        McpServerAdmission::new(
            "tuicommander".parse().expect("TUIC server name"),
            config,
            material,
        )
        .expect("TUIC admission")
    }

    #[test]
    fn tuic_stdio_selection_survives_process_cwd_observation() {
        let server_admission = tuic_admission();
        let McpServerConfig::Stdio(config) = server_admission.config() else {
            panic!("TUIC admission must be stdio")
        };
        let fingerprint =
            McpAdmissionFingerprint::new("capture-v1", "tuicommander/search_tools", "schema-v1")
                .expect("admission");
        let planned = stdio_spawn_spec(&server_admission, config, Duration::from_secs(30))
            .expect("planned spawn");
        let observed = SpawnSpec::new(
            planned.argv().to_vec(),
            Some(CanonPath::new(
                std::fs::canonicalize(std::env::current_dir().expect("current directory"))
                    .expect("canonical current directory"),
            )),
            planned.timeout(),
            planned.env_policy().clone(),
        )
        .expect("observed spawn");

        assert_eq!(
            stdio_transport_fingerprint(config, &fingerprint, &planned),
            stdio_transport_fingerprint(config, &fingerprint, &observed),
            "process observation must not invalidate the selected TUIC transport"
        );
    }

    #[test]
    fn selected_tuic_search_reaches_guarded_duplex_after_observation() {
        let server_admission = tuic_admission();
        let McpServerConfig::Stdio(config) = server_admission.config() else {
            panic!("TUIC admission must be stdio")
        };
        let subject = McpSubject::Tool {
            server: "tuicommander".into(),
            name: "search_tools".into(),
        };
        let input = serde_json::json!({"query": "terminal"});
        let invocation = (
            ego_core::permit::ForeignToolId::new("tuicommander", "search_tools")
                .expect("selected tool"),
            InvocationDigest::from_bytes(&serde_json::to_vec(&input).expect("selected input")),
        );
        let operation = McpOperation::captured(
            subject.clone(),
            McpOperationKind::CallTool,
            McpAdmissionFingerprint::new("capture-v1", "tuicommander/search_tools", "schema-v1")
                .expect("admission fingerprint"),
            input.clone(),
            vec![invocation.clone()],
        );
        let planned = prepare_stdio(
            &server_admission,
            config,
            operation,
            Duration::from_secs(30),
        )
        .expect("planned selected call");
        let envelope = planned.mcp_envelope().expect("selected envelope");
        let observed = SpawnSpec::new(
            vec![OsString::from(&config.command)],
            Some(CanonPath::new(
                std::fs::canonicalize(std::env::current_dir().expect("current directory"))
                    .expect("canonical current directory"),
            )),
            Some(Duration::from_secs(30)),
            planned
                .ops()
                .iter()
                .find_map(|op| match op {
                    ego_core::permit::Op::Spawn { spec } => Some(spec.env_policy().clone()),
                    _ => None,
                })
                .expect("planned environment"),
        )
        .expect("observed spawn");
        let selected = PreparedCall::mcp_stdio(
            subject,
            McpOperationKind::CallTool,
            vec![invocation],
            envelope.admission().clone(),
            envelope.transport_fingerprint(),
            "tuicommander",
            observed,
            &input,
        )
        .expect("observed selected call");
        let probe = DuplexProbe(AtomicBool::new(false));
        let result = futures::executor::block_on(execute_stdio_hook(
            config,
            selected,
            &probe,
            OperationLimits::new(64 * 1024, Duration::from_secs(30)).expect("limits"),
            EffectScope::new(Duration::from_secs(30)),
        ));

        assert!(result.is_err(), "probe intentionally has no bridge process");
        assert!(
            probe.0.load(Ordering::SeqCst),
            "selected search must reach the guarded duplex"
        );
    }

    #[test]
    fn explicit_stdio_cwd_is_canonical_before_selection() {
        let current = std::env::current_dir().expect("current directory");
        let config = McpStdioConfig::new(
            "/opt/tuic/tuic-bridge".into(),
            Vec::new(),
            Some(current.join("src").join("..")),
            Vec::new(),
        )
        .expect("stdio config");
        let admission = McpServerAdmission::new(
            "tuicommander".parse().expect("server name"),
            McpServerConfig::Stdio(config.clone()),
            crate::McpTransportMaterial::stdio(Vec::new()),
        )
        .expect("admission");

        let planned =
            stdio_spawn_spec(&admission, &config, Duration::from_secs(30)).expect("planned spawn");
        assert_eq!(
            planned.cwd().expect("explicit cwd").as_path(),
            std::fs::canonicalize(current).expect("canonical cwd")
        );
    }

    #[test]
    fn stdio_selection_rejects_cwd_that_is_not_a_directory() {
        let config = McpStdioConfig::new(
            "/opt/tuic/tuic-bridge".into(),
            Vec::new(),
            Some(
                std::env::current_dir()
                    .expect("current directory")
                    .join("Cargo.toml"),
            ),
            Vec::new(),
        )
        .expect("stdio config");
        let admission = McpServerAdmission::new(
            "tuicommander".parse().expect("server name"),
            McpServerConfig::Stdio(config.clone()),
            crate::McpTransportMaterial::stdio(Vec::new()),
        )
        .expect("admission");

        assert!(stdio_spawn_spec(&admission, &config, Duration::from_secs(30)).is_err());
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

fn settlement_failure(delivery: DeliveryCertainty) -> McpError {
    McpError::with_delivery(
        McpFailure::Authority,
        "MCP continuation terminal state could not be durably recorded",
        delivery,
    )
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
