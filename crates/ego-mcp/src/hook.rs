//! Hook dispatch through one immutable captured MCP registry generation.

use ego_core::event::hook::{
    HookDecision, HookFailure, HookFailureKind, HookFeedback, HookOutcome,
};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::hook::{
    EffectScope, HookEventV1, HookFuture, HookHandlerConfig, HookHandlerResultV1,
    HookInvocationPort, HookInvocationRequest,
};
use ego_core::mcp::McpOperationKind;
use ego_core::permit::{Classification, GuardedEffects, PolicyEngine, PreparedCall, ToolError};
use ego_core::store::evidence::EvidenceRef;
use ego_core::turn::ToolEffectPorts;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::McpRegistryCapture;

pub(crate) struct McpHookPort<'a> {
    capture: McpRegistryCapture,
    policy: &'a PolicyEngine,
    effects: ToolEffectPorts<'a>,
}

impl<'a> McpHookPort<'a> {
    pub(crate) fn new(
        capture: McpRegistryCapture,
        policy: &'a PolicyEngine,
        effects: ToolEffectPorts<'a>,
    ) -> Self {
        Self {
            capture,
            policy,
            effects,
        }
    }

    async fn invoke_owned(
        &self,
        request: HookInvocationRequest,
    ) -> Result<HookOutcome, Box<HookFailure>> {
        let HookHandlerConfig::McpTool {
            server,
            tool,
            input,
            ..
        } = request.handler()
        else {
            return Err(self.failure(
                HookFailureKind::HandlerFailed,
                "the MCP hook port received a non-MCP handler".to_owned(),
            ));
        };
        let hook = HookEventV1::from_runtime(&request)
            .map_err(|error| self.failure(HookFailureKind::MalformedResult, error.to_string()))?;
        let wire_input = json!({ "hook": hook, "input": input });
        let prepared = self
            .capture
            .prepare_hook_call(server, tool, wire_input)
            .map_err(|error| self.failure(HookFailureKind::HandlerFailed, error.to_string()))?;
        let (prepared, effects) = match self.policy.classify(prepared) {
            Classification::Settled(Ok(authorized)) => self.effects.clone().bind(authorized),
            Classification::Settled(Err(denial)) => {
                return Err(self.failure(HookFailureKind::EffectDenied, denial.to_string()));
            }
            Classification::Ask(pending) => {
                return Err(
                    self.failure(HookFailureKind::EffectDenied, pending.request().to_string())
                );
            }
        };
        let settlement = McpHookAdapter::new(
            self.capture.clone(),
            effects,
            prepared,
            request.effect_scope().clone(),
        )
        .invoke(request.clone())
        .await
        .map_err(|error| self.failure(HookFailureKind::HandlerFailed, error.to_string()))?;
        if let Some(failure) = settlement.failure() {
            let kind = match failure {
                McpHookFailure::Malformed => HookFailureKind::MalformedResult,
                McpHookFailure::PermissionDenied => HookFailureKind::EffectDenied,
                McpHookFailure::HandlerFailed
                    if settlement.delivery() == DeliveryCertainty::PossiblySent =>
                {
                    HookFailureKind::DeliveryUncertain
                }
                McpHookFailure::HandlerFailed => HookFailureKind::HandlerFailed,
            };
            return Err(Box::new(HookFailure::new(kind, None)));
        }
        let feedback = settlement
            .feedback()
            .map(|text| {
                let reference =
                    self.effects
                        .store_hook_feedback(text.as_bytes())
                        .map_err(|_| {
                            Box::new(HookFailure::new(HookFailureKind::EvidenceFailed, None))
                        })?;
                HookFeedback::bounded(request.hook(), reference, text)
                    .map_err(|_| Box::new(HookFailure::new(HookFailureKind::EvidenceFailed, None)))
            })
            .transpose()?;
        Ok(HookOutcome::new(settlement.decision(), feedback))
    }

    fn failure(&self, kind: HookFailureKind, message: String) -> Box<HookFailure> {
        drop(message);
        Box::new(HookFailure::new(kind, None))
    }
}

impl HookInvocationPort for McpHookPort<'_> {
    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        Box::pin(async move { self.invoke_owned(request).await.map_err(|failure| *failure) })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpHookExecution {
    result: CallToolResult,
    evidence: EvidenceRef,
    delivery: DeliveryCertainty,
}

impl McpHookExecution {
    pub fn new(result: CallToolResult, evidence: EvidenceRef, delivery: DeliveryCertainty) -> Self {
        Self {
            result,
            evidence,
            delivery,
        }
    }

    pub fn result(&self) -> &CallToolResult {
        &self.result
    }

    pub fn evidence(&self) -> &EvidenceRef {
        &self.evidence
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpHookFailure {
    Malformed,
    PermissionDenied,
    HandlerFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHookSettlement {
    registry_generation: String,
    operation: McpOperationKind,
    wire_input: Value,
    decision: HookDecision,
    feedback: Option<String>,
    failure: Option<McpHookFailure>,
    evidence: Option<EvidenceRef>,
    delivery: DeliveryCertainty,
}

impl McpHookSettlement {
    fn failed(
        registry_generation: &str,
        wire_input: Value,
        failure: McpHookFailure,
        delivery: DeliveryCertainty,
    ) -> Self {
        Self {
            registry_generation: registry_generation.to_owned(),
            operation: McpOperationKind::CallTool,
            wire_input,
            decision: HookDecision::Continue,
            feedback: None,
            failure: Some(failure),
            evidence: None,
            delivery,
        }
    }

    fn malformed_execution(
        registry_generation: &str,
        wire_input: Value,
        execution: &McpHookExecution,
    ) -> Self {
        let mut settlement = Self::failed(
            registry_generation,
            wire_input,
            McpHookFailure::Malformed,
            execution.delivery(),
        );
        settlement.evidence = Some(execution.evidence().clone());
        settlement
    }

    pub fn registry_generation(&self) -> &str {
        &self.registry_generation
    }

    pub fn operation(&self) -> McpOperationKind {
        self.operation
    }

    pub fn wire_input(&self) -> &Value {
        &self.wire_input
    }

    pub fn decision(&self) -> HookDecision {
        self.decision
    }

    pub fn feedback(&self) -> Option<&str> {
        self.feedback.as_deref()
    }

    pub fn failure(&self) -> Option<McpHookFailure> {
        self.failure
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }

    pub fn evidence(&self) -> Option<&EvidenceRef> {
        self.evidence.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHookAdapterError(String);

impl std::fmt::Display for McpHookAdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for McpHookAdapterError {}

pub struct McpHookAdapter<'a> {
    capture: McpRegistryCapture,
    effects: GuardedEffects<'a>,
    prepared: PreparedCall,
    scope: EffectScope,
}

impl<'a> McpHookAdapter<'a> {
    pub fn new(
        capture: McpRegistryCapture,
        effects: GuardedEffects<'a>,
        prepared: PreparedCall,
        scope: EffectScope,
    ) -> Self {
        Self {
            capture,
            effects,
            prepared,
            scope,
        }
    }

    pub async fn invoke(
        self,
        request: HookInvocationRequest,
    ) -> Result<McpHookSettlement, McpHookAdapterError> {
        let HookHandlerConfig::McpTool {
            server,
            tool,
            input,
            ..
        } = request.handler()
        else {
            return Err(McpHookAdapterError("hook handler is not MCP".to_owned()));
        };
        let hook = HookEventV1::from_runtime(&request)
            .map_err(|error| McpHookAdapterError(error.to_string()))?;
        let wire_input = json!({ "hook": hook, "input": input });
        let generation = self.capture.generation();
        let stable_capture = self.capture.stable_fingerprint();
        let Some(captured) = self
            .capture
            .servers()
            .iter()
            .find(|candidate| candidate.identity().as_str() == server.as_str())
            .cloned()
        else {
            return Ok(McpHookSettlement::failed(
                generation,
                wire_input,
                McpHookFailure::PermissionDenied,
                DeliveryCertainty::DefinitelyUnsent,
            ));
        };
        let Some(envelope) = self.prepared.mcp_envelope() else {
            return Ok(McpHookSettlement::failed(
                generation,
                wire_input,
                McpHookFailure::PermissionDenied,
                DeliveryCertainty::DefinitelyUnsent,
            ));
        };
        let identity_matches = envelope.operation() == McpOperationKind::CallTool
            && envelope.subject().server() == server.as_str()
            && envelope.subject().operation_name() == tool.as_str()
            && envelope.admission().capture() == stable_capture
            && self
                .capture
                .tools()
                .iter()
                .any(|captured_tool| captured_tool.descriptor().name == tool.as_str())
            && serde_json::from_slice::<Value>(envelope.parameters())
                .is_ok_and(|parameters| parameters == wire_input);
        if !identity_matches {
            return Ok(McpHookSettlement::failed(
                generation,
                wire_input,
                McpHookFailure::PermissionDenied,
                DeliveryCertainty::DefinitelyUnsent,
            ));
        }
        let factory = captured.operation_factory();
        let admission = captured.into();
        let execution = match factory
            .execute_hook(admission, self.prepared, &self.effects, self.scope)
            .await
        {
            Ok(execution) => execution,
            Err(error) => {
                let (failure, delivery) = map_tool_failure(&error);
                return Ok(McpHookSettlement::failed(
                    generation, wire_input, failure, delivery,
                ));
            }
        };
        let Some(structured) = execution.result().structured_content.as_ref() else {
            return Ok(McpHookSettlement::malformed_execution(
                generation, wire_input, &execution,
            ));
        };
        let raw = serde_json::to_vec(structured)
            .map_err(|error| McpHookAdapterError(error.to_string()))?;
        let result = match HookHandlerResultV1::parse(&raw) {
            Ok(result) => result,
            Err(_) => {
                return Ok(McpHookSettlement::malformed_execution(
                    generation, wire_input, &execution,
                ));
            }
        };
        Ok(McpHookSettlement {
            registry_generation: generation.to_owned(),
            operation: McpOperationKind::CallTool,
            wire_input,
            decision: result.decision(),
            feedback: result.feedback().map(str::to_owned),
            failure: None,
            evidence: Some(execution.evidence().clone()),
            delivery: execution.delivery(),
        })
    }
}

fn map_tool_failure(error: &ToolError) -> (McpHookFailure, DeliveryCertainty) {
    match error {
        ToolError::Denied(_) => (
            McpHookFailure::PermissionDenied,
            DeliveryCertainty::DefinitelyUnsent,
        ),
        ToolError::InvalidParams(_) | ToolError::Fs(_) => (
            McpHookFailure::HandlerFailed,
            DeliveryCertainty::DefinitelyUnsent,
        ),
        _ => (
            McpHookFailure::HandlerFailed,
            DeliveryCertainty::PossiblySent,
        ),
    }
}
