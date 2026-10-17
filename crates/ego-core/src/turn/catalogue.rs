//! Immutable tool catalogues captured at a main-generation boundary.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use crate::hook::{HookCausality, SessionHookRuntime};
use crate::mcp::McpServerInstructions;
use crate::suspension::{McpCaptureAuthority, McpCapturePin, McpResumeContext};

use super::{GenerationPolicySnapshot, RegisteredTool};

/// Authority retained with the exact catalogue selected for one generation.
#[derive(Clone)]
pub(crate) enum ToolCaptureAuthority {
    Static,
    Mcp(McpCaptureAuthority),
}

impl ToolCaptureAuthority {
    /// What the servers behind this authority say about themselves.
    ///
    /// Empty for a static capture, which is not a fallback: a static registry
    /// has no server that could have described itself.
    pub(crate) fn instructions(&self) -> &[McpServerInstructions] {
        match self {
            Self::Static => &[],
            Self::Mcp(authority) => authority.instructions(),
        }
    }

    /// The MCP servers behind this authority; none for a static capture.
    pub(crate) fn server_names(&self) -> Vec<String> {
        match self {
            Self::Static => Vec::new(),
            Self::Mcp(authority) => authority.server_names(),
        }
    }

    pub(crate) fn resume_context<'s>(
        &self,
        runtime: Option<&SessionHookRuntime<'s>>,
        causality: HookCausality,
        policy_snapshot: &Arc<GenerationPolicySnapshot>,
    ) -> Option<McpResumeContext<'s>> {
        match self {
            Self::Mcp(capture) => runtime.map(|runtime| {
                McpResumeContext::from_authority(
                    runtime.clone(),
                    capture.clone(),
                    causality,
                    Arc::clone(policy_snapshot),
                )
            }),
            Self::Static => None,
        }
    }
}

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
    authority: ToolCaptureAuthority,
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
            authority: ToolCaptureAuthority::Static,
        })
    }

    /// Admit a complete MCP capture and retain its exact immutable authority.
    pub fn new_mcp<P>(
        generation: impl Into<String>,
        tools: Vec<RegisteredTool>,
        capture: P,
    ) -> Result<Self, ToolCatalogueError>
    where
        P: McpCapturePin + 'static,
    {
        validate_distinct(&tools)?;
        Ok(Self {
            generation: Arc::from(generation.into()),
            tools: tools.into(),
            authority: ToolCaptureAuthority::Mcp(McpCaptureAuthority::new(capture)),
        })
    }

    /// The source revision retained with every call prepared from this capture.
    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub fn tools(&self) -> &[RegisteredTool] {
        &self.tools
    }

    pub(crate) fn authority(&self) -> ToolCaptureAuthority {
        self.authority.clone()
    }

    /// What the MCP servers behind this capture say about themselves.
    pub fn instructions(&self) -> &[McpServerInstructions] {
        self.authority.instructions()
    }
}

/// Consumer-owned port for a complete admitted dynamic tool snapshot.
pub trait AdmittedToolSource: Send + Sync {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError>;

    /// Current host-supplied data to show at the next model generation.
    fn context_notes(&self) -> Vec<String> {
        Vec::new()
    }
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
