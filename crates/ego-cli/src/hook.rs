//! Prompt hooks over the shared tool-less generation lifecycle.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ego_core::event::domain::GenerationRole;
use ego_core::event::hook::{
    HookDecision, HookFailure, HookFailureKind, HookFeedback, HookOutcome,
};
use ego_core::event::id::TurnId;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::hook::{
    EffectScope, HookFuture, HookHandlerConfig, HookHandlerRequestV1, HookHandlerResultV1,
    HookInvocationBinding, HookInvocationPort, HookInvocationRequest,
};
use ego_core::mcp::McpRuntimeBinding;
use ego_core::permit::{
    Classification, HttpRoute, HttpScheme, NetworkTarget, PolicyEngine, PreparedCall,
};
use ego_core::store::evidence::EvidenceRef;
use ego_core::turn::{ToolEffectPorts, ToollessGenerationOwner};
use ego_tools::exec::RunEffectPorts;
use ego_tools::hook::{CommandHookAdapter, HookAdapterResult, HttpHookAdapter};

use crate::model::CliGenerationModelSource;
use crate::provider_http::ProviderHttpAdapter;

pub type GenerationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<GeneratedHookResult, ToollessGenerationError>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct ToollessGenerationRequest {
    role: GenerationRole,
    prompt: String,
    input: HookHandlerRequestV1,
    model_slug: Option<String>,
    tools: Vec<String>,
    effect_scope: EffectScope,
    turn: TurnId,
}

impl ToollessGenerationRequest {
    pub fn role(&self) -> GenerationRole {
        self.role
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn input(&self) -> &HookHandlerRequestV1 {
        &self.input
    }

    pub fn model_slug(&self) -> Option<&str> {
        self.model_slug.as_deref()
    }

    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    pub fn effect_scope(&self) -> &EffectScope {
        &self.effect_scope
    }

    pub fn turn(&self) -> TurnId {
        self.turn
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedHookResult {
    pub raw: Vec<u8>,
    pub evidence: EvidenceRef,
    pub delivery: DeliveryCertainty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToollessGenerationError {
    message: String,
    delivery: DeliveryCertainty,
}

impl ToollessGenerationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            delivery: DeliveryCertainty::DefinitelyUnsent,
        }
    }

    pub fn with_delivery(message: impl Into<String>, delivery: DeliveryCertainty) -> Self {
        Self {
            message: message.into(),
            delivery,
        }
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
}

impl std::fmt::Display for ToollessGenerationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToollessGenerationError {}

pub trait ToollessGenerationService: Send + Sync {
    fn generate(&self, request: ToollessGenerationRequest) -> GenerationFuture<'_>;
}

impl<S: ToollessGenerationService + ?Sized> ToollessGenerationService for Arc<S> {
    fn generate(&self, request: ToollessGenerationRequest) -> GenerationFuture<'_> {
        self.as_ref().generate(request)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptHookFailure {
    Generation,
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHookSettlement {
    raw: Vec<u8>,
    decision: HookDecision,
    feedback: Option<String>,
    failure: Option<PromptHookFailure>,
    evidence: Option<EvidenceRef>,
    delivery: DeliveryCertainty,
}

impl PromptHookSettlement {
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn decision(&self) -> HookDecision {
        self.decision
    }

    pub fn feedback(&self) -> Option<&str> {
        self.feedback.as_deref()
    }

    pub fn failure(&self) -> Option<PromptHookFailure> {
        self.failure
    }

    pub fn evidence(&self) -> Option<&EvidenceRef> {
        self.evidence.as_ref()
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
}

pub struct HookPromptAdapter<S> {
    service: S,
}

impl<S: ToollessGenerationService> HookPromptAdapter<S> {
    pub fn new(service: S) -> Self {
        Self { service }
    }

    pub async fn invoke(
        self,
        request: HookInvocationRequest,
    ) -> Result<PromptHookSettlement, ToollessGenerationError> {
        let HookHandlerConfig::Prompt { prompt, model, .. } = request.handler() else {
            return Err(ToollessGenerationError::new("hook handler is not a prompt"));
        };
        let generated = self
            .service
            .generate(ToollessGenerationRequest {
                role: GenerationRole::Hook,
                prompt: prompt.clone(),
                input: HookHandlerRequestV1::from_runtime(&request)
                    .map_err(|error| ToollessGenerationError::new(error.to_string()))?,
                model_slug: model.clone(),
                tools: Vec::new(),
                effect_scope: request.effect_scope().clone(),
                turn: request.causality().turn(),
            })
            .await;
        let generated = match generated {
            Ok(generated) => generated,
            Err(error) => {
                return Ok(PromptHookSettlement {
                    raw: Vec::new(),
                    decision: HookDecision::Continue,
                    feedback: None,
                    failure: Some(PromptHookFailure::Generation),
                    evidence: None,
                    delivery: error.delivery(),
                });
            }
        };
        let parsed = HookHandlerResultV1::parse(&generated.raw);
        match parsed {
            Ok(result) => Ok(PromptHookSettlement {
                raw: generated.raw,
                decision: result.decision(),
                feedback: result.feedback().map(str::to_owned),
                failure: None,
                evidence: Some(generated.evidence),
                delivery: generated.delivery,
            }),
            Err(_) => Ok(PromptHookSettlement {
                raw: generated.raw,
                decision: HookDecision::Continue,
                feedback: None,
                failure: Some(PromptHookFailure::Malformed),
                evidence: Some(generated.evidence),
                delivery: generated.delivery,
            }),
        }
    }
}

/// Production prompt-hook generation over the session's shared tool-less
/// attempt owner and the invocation's exact cancel/deadline scope.
pub struct RunToollessGenerationService<'session, 'effects> {
    owner: ToollessGenerationOwner<'session, 'effects>,
    source: Arc<CliGenerationModelSource>,
    ports: Arc<RunEffectPorts>,
}

impl<'session, 'effects> RunToollessGenerationService<'session, 'effects> {
    pub fn new(
        owner: ToollessGenerationOwner<'session, 'effects>,
        source: Arc<CliGenerationModelSource>,
        ports: Arc<RunEffectPorts>,
    ) -> Self {
        Self {
            owner,
            source,
            ports,
        }
    }
}

impl ToollessGenerationService for RunToollessGenerationService<'_, '_> {
    fn generate(&self, request: ToollessGenerationRequest) -> GenerationFuture<'_> {
        Box::pin(async move {
            let input = serde_json::to_string(request.input())
                .map_err(|error| ToollessGenerationError::new(error.to_string()))?;
            let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(&self.ports)))
                as Arc<dyn ego_ai::http::Http>;
            let source = self.source.bind(
                http,
                request.effect_scope().cancel_token(),
                Some(request.effect_scope().deadline()),
            );
            let generated = self
                .owner
                .generate(
                    source,
                    request.role(),
                    request.turn(),
                    request.prompt().to_owned(),
                    input,
                    request
                        .model_slug()
                        .map(|model| ego_core::config::ModelRef(model.to_owned())),
                )
                .await
                .map_err(|error| {
                    ToollessGenerationError::with_delivery(error.to_string(), error.delivery())
                })?;
            Ok(GeneratedHookResult {
                raw: generated.raw().to_vec(),
                evidence: generated.evidence().clone(),
                delivery: generated.delivery(),
            })
        })
    }
}

/// The sole production-facing hook invocation adapter for one policy binding.
///
/// Handler-specific parsing and execution stays in the typed lower adapters;
/// this owner binds each prepared effect to the run policy and converts the
/// settlement into the core hook result vocabulary.
pub struct HookPortAdapter<'a> {
    policy: Arc<PolicyEngine>,
    effects: ToolEffectPorts<'a>,
    mcp: Option<Arc<dyn HookInvocationPort + 'a>>,
    prompt: Arc<dyn ToollessGenerationService + 'a>,
}

impl<'a> HookPortAdapter<'a> {
    pub fn new(
        policy: Arc<PolicyEngine>,
        effects: ToolEffectPorts<'a>,
        mcp: Option<Arc<dyn HookInvocationPort + 'a>>,
        prompt: Arc<dyn ToollessGenerationService + 'a>,
    ) -> Self {
        Self {
            policy,
            effects,
            mcp,
            prompt,
        }
    }

    async fn invoke_owned(
        &self,
        request: HookInvocationRequest,
    ) -> Result<HookOutcome, Box<HookFailure>> {
        match request.handler() {
            HookHandlerConfig::Command { .. } => self.invoke_command(request).await,
            HookHandlerConfig::Http { .. } => self.invoke_http(request).await,
            HookHandlerConfig::McpTool { .. } => self.invoke_mcp(request).await,
            HookHandlerConfig::Prompt { .. } => self.invoke_prompt(request).await,
        }
    }

    async fn invoke_command(
        &self,
        request: HookInvocationRequest,
    ) -> Result<HookOutcome, Box<HookFailure>> {
        let prepared = CommandHookAdapter::prepare(request.clone())
            .map_err(|error| self.failure(HookFailureKind::HandlerFailed, error.to_string()))?;
        let (prepared, effects) = self.authorize(prepared)?;
        let result = CommandHookAdapter::new(effects, prepared)
            .invoke(request.clone())
            .await
            .map_err(|error| self.failure(HookFailureKind::HandlerFailed, error.to_string()))?;
        self.settle_adapter(&request, result)
    }

    async fn invoke_http(
        &self,
        request: HookInvocationRequest,
    ) -> Result<HookOutcome, Box<HookFailure>> {
        let http = HttpHookAdapter::prepare(request.clone())
            .map_err(|error| self.failure(HookFailureKind::HandlerFailed, error.to_string()))?;
        let prepared = self.prepare_http_authority(&request, &http)?;
        let (prepared, effects) = self.authorize(prepared)?;
        let result = HttpHookAdapter::new(effects, prepared, http)
            .invoke(request.clone())
            .await
            .map_err(|error| self.failure(HookFailureKind::HandlerFailed, error.to_string()))?;
        self.settle_adapter(&request, result)
    }

    async fn invoke_mcp(
        &self,
        request: HookInvocationRequest,
    ) -> Result<HookOutcome, Box<HookFailure>> {
        let mcp = self.mcp.as_ref().ok_or_else(|| {
            self.failure(
                HookFailureKind::BackendUnavailable,
                "the session has no MCP hook runtime".to_owned(),
            )
        })?;
        mcp.invoke(request).await.map_err(Box::new)
    }

    async fn invoke_prompt(
        &self,
        request: HookInvocationRequest,
    ) -> Result<HookOutcome, Box<HookFailure>> {
        let settlement = HookPromptAdapter::new(Arc::clone(&self.prompt))
            .invoke(request.clone())
            .await
            .map_err(|error| self.failure(HookFailureKind::HandlerFailed, error.to_string()))?;
        if let Some(failure) = settlement.failure() {
            let kind = match failure {
                PromptHookFailure::Malformed => HookFailureKind::MalformedResult,
                PromptHookFailure::Generation
                    if settlement.delivery() == DeliveryCertainty::PossiblySent =>
                {
                    HookFailureKind::DeliveryUncertain
                }
                PromptHookFailure::Generation => HookFailureKind::BackendUnavailable,
            };
            return Err(Box::new(HookFailure::new(kind, None)));
        }
        self.outcome(&request, settlement.decision(), settlement.feedback())
    }

    fn authorize(
        &self,
        prepared: PreparedCall,
    ) -> Result<(PreparedCall, ego_core::permit::GuardedEffects<'a>), Box<HookFailure>> {
        match self.policy.classify(prepared) {
            Classification::Settled(Ok(authorized)) => Ok(self.effects.clone().bind(authorized)),
            Classification::Settled(Err(denial)) => {
                Err(self.failure(HookFailureKind::EffectDenied, denial.to_string()))
            }
            Classification::Ask(pending) => {
                Err(self.failure(HookFailureKind::EffectDenied, pending.request().to_string()))
            }
        }
    }

    fn prepare_http_authority(
        &self,
        request: &HookInvocationRequest,
        http: &ego_core::permit::HttpRequest,
    ) -> Result<PreparedCall, Box<HookFailure>> {
        let HookHandlerConfig::Http {
            url,
            headers,
            allowed_env_vars,
            ..
        } = request.handler()
        else {
            unreachable!("the HTTP branch matches the handler kind")
        };
        let parsed = url::Url::parse(url)
            .map_err(|error| self.failure(HookFailureKind::HandlerFailed, error.to_string()))?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(self.failure(
                HookFailureKind::HandlerFailed,
                "HTTP hook URL contains unsupported authority or fragment".to_owned(),
            ));
        }
        let scheme = match parsed.scheme() {
            "http" => HttpScheme::Http,
            "https" => HttpScheme::Https,
            _ => {
                return Err(self.failure(
                    HookFailureKind::HandlerFailed,
                    "HTTP hook URL has an unsupported scheme".to_owned(),
                ));
            }
        };
        let host = parsed.host_str().ok_or_else(|| {
            self.failure(
                HookFailureKind::HandlerFailed,
                "HTTP hook URL has no host".to_owned(),
            )
        })?;
        let port = parsed.port_or_known_default().ok_or_else(|| {
            self.failure(
                HookFailureKind::HandlerFailed,
                "HTTP hook URL has no port".to_owned(),
            )
        })?;
        let target = NetworkTarget::new(host, port)
            .map_err(|error| self.failure(HookFailureKind::HandlerFailed, error.to_string()))?;
        let route = HttpRoute::new(scheme, target)
            .map_err(|error| self.failure(HookFailureKind::HandlerFailed, error.to_string()))?;
        let allowed = allowed_env_vars.as_deref().unwrap_or_default();
        let secret_sources = headers
            .iter()
            .filter_map(|(_, value)| exact_secret_name(value))
            .filter(|name| allowed.iter().any(|allowed| allowed == name))
            .map(str::to_owned)
            .collect();
        Ok(PreparedCall::hook_http(
            request.hook(),
            route,
            http,
            secret_sources,
        ))
    }

    fn settle_adapter(
        &self,
        request: &HookInvocationRequest,
        result: HookAdapterResult,
    ) -> Result<HookOutcome, Box<HookFailure>> {
        let delivery = result.delivery_certainty();
        let HookAdapterResult::Settled(settlement) = result;
        if settlement.failure().is_some() {
            let kind = if delivery == DeliveryCertainty::PossiblySent {
                HookFailureKind::DeliveryUncertain
            } else {
                HookFailureKind::HandlerFailed
            };
            return Err(Box::new(HookFailure::new(kind, None)));
        }
        self.outcome(request, settlement.decision(), settlement.feedback())
    }

    fn outcome(
        &self,
        request: &HookInvocationRequest,
        decision: HookDecision,
        feedback: Option<&str>,
    ) -> Result<HookOutcome, Box<HookFailure>> {
        let feedback = feedback
            .map(|feedback| self.feedback(request, feedback))
            .transpose()?;
        Ok(HookOutcome::new(decision, feedback))
    }

    fn failure(&self, kind: HookFailureKind, message: String) -> Box<HookFailure> {
        tracing::warn!(%message, ?kind, "hook invocation adapter refused an effect");
        Box::new(HookFailure::new(kind, None))
    }

    fn feedback(
        &self,
        request: &HookInvocationRequest,
        text: &str,
    ) -> Result<HookFeedback, Box<HookFailure>> {
        let reference = self
            .effects
            .store_hook_feedback(text.as_bytes())
            .map_err(|_| Box::new(HookFailure::new(HookFailureKind::EvidenceFailed, None)))?;
        HookFeedback::bounded(request.hook(), reference, text)
            .map_err(|_| Box::new(HookFailure::new(HookFailureKind::EvidenceFailed, None)))
    }
}

/// Builds one immutable CLI hook authority from one generation policy capture.
pub(crate) struct CliHookInvocationBinding<'a> {
    effects: ToolEffectPorts<'a>,
    prompt: Arc<dyn ToollessGenerationService + 'a>,
}

impl<'a> CliHookInvocationBinding<'a> {
    pub(crate) fn new(
        effects: ToolEffectPorts<'a>,
        prompt: Arc<dyn ToollessGenerationService + 'a>,
    ) -> Self {
        Self { effects, prompt }
    }
}

impl<'binding> HookInvocationBinding for CliHookInvocationBinding<'binding> {
    fn bind<'a>(
        &self,
        policy: Arc<PolicyEngine>,
        mcp: Option<&dyn McpRuntimeBinding>,
    ) -> Result<Arc<dyn HookInvocationPort + 'a>, String>
    where
        Self: 'a,
    {
        let mcp = mcp
            .map(|runtime| runtime.hook_invocation_port(Arc::clone(&policy), self.effects.clone()))
            .transpose()?;
        Ok(Arc::new(HookPortAdapter::new(
            policy,
            self.effects.clone(),
            mcp,
            Arc::clone(&self.prompt),
        )))
    }
}

impl HookInvocationPort for HookPortAdapter<'_> {
    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        Box::pin(async move { self.invoke_owned(request).await.map_err(|failure| *failure) })
    }
}

fn exact_secret_name(value: &str) -> Option<&str> {
    value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .filter(|name| !name.is_empty() && !name.contains('{') && !name.contains('}'))
}
