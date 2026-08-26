//! The consumer-owned transaction port for changes to user configuration.
//!
//! Core names the complete policy change. The filesystem adapter is responsible
//! for locking, comment-preserving editing, strict validation, and durable
//! publication as one operation.

use crate::generator::types::ThinkingLevel;

use super::layer::{McpLayerEntry, McpServerConfig, McpServerName, validate_status_segment_names};

/// The exact user-owned table changed by an MCP mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserMcpSlot {
    Base,
    Profile(String),
}

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
    /// Set one complete server in the exact base or profile slot.
    McpSet {
        slot: UserMcpSlot,
        name: McpServerName,
        expected_user_entry: Option<McpLayerEntry>,
        server: McpServerConfig,
    },
    /// Delete exactly the entry the caller resolved from a user-owned slot.
    McpDeleteUserEntry {
        slot: UserMcpSlot,
        name: McpServerName,
        expected_entry: McpLayerEntry,
    },
    /// Hide a system entry only if the base user slot is still absent.
    McpInsertUserTombstone {
        name: McpServerName,
        expected_user_entry: Option<McpLayerEntry>,
    },
}

impl UserConfigMutation {
    /// Reject a mutation whose policy cannot be represented in config.
    ///
    /// This runs before the adapter acquires a lock or reads the file. A
    /// duplicate status segment is not an ordering instruction: it is an
    /// ambiguous configuration and therefore fails as a whole.
    pub fn validate(&self) -> Result<(), UserConfigError> {
        match self {
            Self::Statusline { segments } => {
                validate_status_segment_names(segments).map_err(UserConfigError::new)
            }
            Self::McpSet {
                expected_user_entry,
                server,
                ..
            } => {
                if let Some(McpLayerEntry::Server(expected)) = expected_user_entry {
                    expected
                        .validate()
                        .map_err(|error| UserConfigError::new(error.to_string()))?;
                }
                server
                    .validate()
                    .map_err(|error| UserConfigError::new(error.to_string()))
            }
            Self::McpDeleteUserEntry {
                expected_entry: McpLayerEntry::Server(server),
                ..
            } => server
                .validate()
                .map_err(|error| UserConfigError::new(error.to_string())),
            Self::McpInsertUserTombstone {
                expected_user_entry: Some(_),
                ..
            } => Err(UserConfigError::new(
                "an MCP tombstone insertion must compare against an absent user entry",
            )),
            Self::ModelAndThinking { .. }
            | Self::McpDeleteUserEntry {
                expected_entry: McpLayerEntry::Tombstone,
                ..
            }
            | Self::McpInsertUserTombstone {
                expected_user_entry: None,
                ..
            } => Ok(()),
        }
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
