//! Durable, sanitized evidence emitted by a machine-scoped MCP registry.

use std::fmt;

use serde::{Deserialize, Serialize};

/// One append-only registry fact. It contains no credentials or live handles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpRegistryEvent {
    RefreshRequested {
        request_id: String,
        trigger: String,
        server_scope: Option<String>,
        config_revision: u64,
    },
    SnapshotPublished {
        generation: String,
        predecessor: Option<String>,
        servers: Vec<String>,
        descriptor_digests: Vec<String>,
    },
    RefreshFailed {
        request_id: String,
        cause: String,
        retained_generation: Option<String>,
    },
    HealthChanged {
        server: String,
        prior: String,
        current: String,
    },
    AuthorizationRequired {
        server: String,
        issuer: String,
        protected_resource: String,
        scopes: Vec<String>,
        request_correlation: String,
    },
}

/// Journal operation that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpRegistryJournalOperation {
    Append,
    BeginRefresh,
    Replay,
}

impl fmt::Display for McpRegistryJournalOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Append => "append",
            Self::BeginRefresh => "begin refresh",
            Self::Replay => "replay",
        })
    }
}

/// Closed classification of a journal failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpRegistryJournalErrorKind {
    Unavailable,
    Corrupt,
    UnsupportedVersion,
    DurabilityUncertain,
    Exhausted,
}

impl fmt::Display for McpRegistryJournalErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "unavailable",
            Self::Corrupt => "corrupt",
            Self::UnsupportedVersion => "unsupported version",
            Self::DurabilityUncertain => "durability uncertain",
            Self::Exhausted => "revision space exhausted",
        })
    }
}

/// Sanitized adapter failure; OS and codec errors do not cross the port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRegistryJournalError {
    operation: McpRegistryJournalOperation,
    kind: McpRegistryJournalErrorKind,
}

impl McpRegistryJournalError {
    pub fn new(operation: McpRegistryJournalOperation, kind: McpRegistryJournalErrorKind) -> Self {
        Self { operation, kind }
    }

    pub fn operation(&self) -> McpRegistryJournalOperation {
        self.operation
    }

    pub fn kind(&self) -> McpRegistryJournalErrorKind {
        self.kind
    }
}

impl fmt::Display for McpRegistryJournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MCP registry journal {} failed ({})",
            self.operation, self.kind
        )
    }
}

impl std::error::Error for McpRegistryJournalError {}

/// Consumer-owned machine journal port.
pub trait McpRegistryJournal: Send + Sync {
    fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError>;

    fn begin_refresh(
        &self,
        request_id: String,
        trigger: String,
        server_scope: Option<String>,
    ) -> Result<u64, McpRegistryJournalError>;

    fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError>;
}
