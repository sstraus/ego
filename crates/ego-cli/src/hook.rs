//! Prompt hooks over the shared tool-less generation lifecycle.

use std::future::Future;
use std::pin::Pin;

use ego_core::event::domain::GenerationRole;
use ego_core::event::hook::HookDecision;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::hook::{
    EffectScope, HookHandlerConfig, HookHandlerRequestV1, HookHandlerResultV1,
    HookInvocationRequest,
};
use ego_core::store::evidence::EvidenceRef;

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
