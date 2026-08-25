//! The consumer-owned transaction port for changes to user configuration.
//!
//! Core names the complete policy change. The filesystem adapter is responsible
//! for locking, comment-preserving editing, strict validation, and durable
//! publication as one operation.

use crate::generator::types::ThinkingLevel;

use super::layer::validate_status_segment_names;

/// One complete, atomic change to the user configuration authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserConfigMutation {
    /// Change the model default and its reasoning effort together.
    ModelAndThinking {
        model: String,
        thinking: ThinkingLevel,
    },
    /// Replace the final ordered status-line segment list.
    Statusline { segments: Vec<String> },
}

impl UserConfigMutation {
    /// Reject a mutation whose policy cannot be represented in config.
    ///
    /// This runs before the adapter acquires a lock or reads the file. A
    /// duplicate status segment is not an ordering instruction: it is an
    /// ambiguous configuration and therefore fails as a whole.
    pub fn validate(&self) -> Result<(), UserConfigError> {
        let Self::Statusline { segments } = self else {
            return Ok(());
        };

        validate_status_segment_names(segments).map_err(UserConfigError::new)
    }
}

/// A failed user-config mutation. Callers must keep their previous in-memory
/// authority whenever this is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserConfigError {
    message: String,
}

impl UserConfigError {
    /// Preserve the boundary's actionable cause without exposing an outer
    /// filesystem error type through the core-owned port.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for UserConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UserConfigError {}

/// What became authoritative after a successful user-config publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserConfigPublication {
    /// The new config was published and its directory entry was synced.
    PublishedDurably,
    /// The new config is authoritative, but its final durability barrier
    /// failed. The caller must surface `diagnostic` and must not retry.
    PublishedDurabilityUncertain { diagnostic: String },
}

/// The narrow consumer port for the user configuration transaction.
pub trait UserConfigStore: Send + Sync {
    /// Publish one complete mutation. `Err` guarantees that publication did
    /// not occur and the previous config remains authoritative.
    fn mutate(
        &self,
        mutation: UserConfigMutation,
    ) -> Result<UserConfigPublication, UserConfigError>;
}
