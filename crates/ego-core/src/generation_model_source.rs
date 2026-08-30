//! The fresh model-resolution boundary owned by the turn loop.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::budget::{Metering, ResolvedRates};
use crate::config::ModelRef;
use crate::generator::capabilities::ModelCapabilities;
use crate::generator::port::Generator;
use crate::generator::types::ThinkingLevel;

pub use crate::event::domain::GenerationRole;

/// Model authority requested for one generation boundary.
///
/// Skill overrides are carried with the request so they cannot mutate or
/// outlive the session selection that supplies ordinary generations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationModelRequest {
    role: GenerationRole,
    model: Option<ModelRef>,
    thinking: Option<ThinkingLevel>,
}

impl GenerationModelRequest {
    pub fn new(role: GenerationRole) -> Self {
        Self {
            role,
            model: None,
            thinking: None,
        }
    }

    pub fn with_skill_scope(
        mut self,
        model: Option<ModelRef>,
        thinking: Option<ThinkingLevel>,
    ) -> Self {
        self.model = model;
        self.thinking = thinking;
        self
    }

    pub fn role(&self) -> GenerationRole {
        self.role
    }

    pub fn model(&self) -> Option<&ModelRef> {
        self.model.as_ref()
    }

    pub fn thinking(&self) -> Option<ThinkingLevel> {
        self.thinking
    }
}

/// Everything one generation attempt needs from the currently selected model.
pub struct GenerationProfile {
    pub identity: ModelRef,
    pub request_model: String,
    pub thinking: ThinkingLevel,
    pub generator: Arc<dyn Generator>,
    pub capabilities: ModelCapabilities,
    pub metering: Metering,
    pub rates: Option<ResolvedRates>,
}

impl GenerationProfile {
    /// The response bound used by both ordinary and judge requests.
    pub fn output_ceiling(&self) -> Option<u64> {
        self.capabilities
            .max_output_tokens
            .or(self.capabilities.max_context_tokens)
    }
}

/// Which existing process-exit policy applies when fresh resolution fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationModelErrorClass {
    Usage,
    Provider,
    Internal,
}

/// A typed failure to establish current generation authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationModelError {
    pub class: GenerationModelErrorClass,
    pub message: String,
}

impl GenerationModelError {
    pub fn new(class: GenerationModelErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}

impl fmt::Display for GenerationModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GenerationModelError {}

/// Resolves the current authority immediately before one generation attempt.
pub trait GenerationModelSource: Send + Sync {
    fn resolve<'a>(
        &'a self,
        request: GenerationModelRequest,
    ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>>;
}
