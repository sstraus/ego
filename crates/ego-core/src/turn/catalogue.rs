//! Immutable tool catalogues captured at a main-generation boundary.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use super::RegisteredTool;

/// A failure to construct or capture one unambiguous model-visible catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCatalogueError {
    /// The admitted source could not provide a complete snapshot.
    CaptureUnavailable,
    /// Two entries would give one model-visible name two meanings.
    DuplicateName { name: String },
}

impl fmt::Display for ToolCatalogueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CaptureUnavailable => f.write_str("the admitted tool catalogue is unavailable"),
            Self::DuplicateName { name } => {
                write!(f, "the tool catalogue contains duplicate name {name}")
            }
        }
    }
}

impl std::error::Error for ToolCatalogueError {}

/// One immutable dynamic catalogue admitted by an outer registry controller.
#[derive(Clone)]
pub struct CapturedToolCatalogue {
    generation: Arc<str>,
    tools: Arc<[RegisteredTool]>,
}

impl CapturedToolCatalogue {
    /// Admit a complete capture, rejecting ambiguous names before publication.
    pub fn new(
        generation: impl Into<String>,
        tools: Vec<RegisteredTool>,
    ) -> Result<Self, ToolCatalogueError> {
        validate_distinct(&tools)?;
        Ok(Self {
            generation: Arc::from(generation.into()),
            tools: tools.into(),
        })
    }

    /// The source revision retained with every call prepared from this capture.
    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub(crate) fn tools(&self) -> &[RegisteredTool] {
        &self.tools
    }
}

/// Consumer-owned port for a complete admitted dynamic tool snapshot.
pub trait AdmittedToolSource: Send + Sync {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError>;
}

pub(crate) fn validate_distinct(tools: &[RegisteredTool]) -> Result<(), ToolCatalogueError> {
    let mut names = BTreeSet::new();
    for tool in tools {
        let name = tool.descriptor().name;
        if !names.insert(name.clone()) {
            return Err(ToolCatalogueError::DuplicateName { name });
        }
    }
    Ok(())
}
