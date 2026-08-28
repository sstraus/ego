//! Durable publication of protocol-neutral public run artifacts.

use std::sync::Arc;

use crate::run::RunId;

/// Stable identity of one published run artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunArtifactId(&'static str);

impl RunArtifactId {
    /// The ordinary final result of the latest completed turn.
    pub const fn output() -> Self {
        Self("output")
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Bytes already selected for publication by the ordinary run pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedRunArtifact {
    id: RunArtifactId,
    mime_type: Arc<str>,
    bytes: Arc<[u8]>,
}

impl PublishedRunArtifact {
    pub fn output(mime_type: impl Into<Arc<str>>, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            id: RunArtifactId::output(),
            mime_type: mime_type.into(),
            bytes: bytes.into(),
        }
    }

    pub const fn id(&self) -> RunArtifactId {
        self.id
    }

    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Closed failure vocabulary for the durable artifact boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunArtifactStoreError {
    Conflict {
        run_id: RunId,
        artifact_id: RunArtifactId,
    },
    Absent {
        run_id: RunId,
        artifact_id: RunArtifactId,
    },
    Io {
        message: String,
    },
}

impl std::fmt::Display for RunArtifactStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict {
                run_id,
                artifact_id,
            } => write!(
                formatter,
                "run artifact {run_id}/{} conflicts with its durable publication",
                artifact_id.as_str()
            ),
            Self::Absent {
                run_id,
                artifact_id,
            } => write!(
                formatter,
                "run artifact {run_id}/{} is absent",
                artifact_id.as_str()
            ),
            Self::Io { message } => write!(formatter, "run artifact store I/O error: {message}"),
        }
    }
}

impl std::error::Error for RunArtifactStoreError {}

/// Consumer-owned durable boundary for already-public run artifact bytes.
pub trait RunArtifactStore: Send + Sync {
    /// Atomically publish the stable record. Repeating the exact record succeeds.
    fn publish(
        &self,
        run_id: RunId,
        artifact: PublishedRunArtifact,
    ) -> Result<(), RunArtifactStoreError>;

    fn read(
        &self,
        run_id: RunId,
        artifact_id: RunArtifactId,
    ) -> Result<PublishedRunArtifact, RunArtifactStoreError>;
}
