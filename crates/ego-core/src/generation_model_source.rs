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
        role: GenerationRole,
    ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>>;
}
