//! `ModelCapabilities`: the versioned record ARCH §3.1 and I5
//! (`docs/08-codex-critique.md`) require before treating a shared HTTP shape
//! as equivalent conversation semantics.
//!
//! A provider row is configuration, not proof two models behave the same in
//! conversation. `ModelCapabilities` is resolved per generation and checked
//! before a fallback: `fallback_gaps` reports every incompatibility rather
//! than letting a switch proceed and silently drop or mistranslate state.

use serde::{Deserialize, Serialize};

use super::types::GenRequest;

/// Whether a model can accept reasoning state carried over from another
/// generation (its own prior turn, or — for a fallback — a different model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningCarryover {
    /// No reasoning-state carryover accepted at all.
    Unsupported,
    /// Only understands its own provider's reasoning state.
    SameProviderOnly,
    /// Accepts reasoning state produced by any provider.
    Portable,
}

/// How a generation can be stopped mid-stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelSemantics {
    /// The generation cannot be cancelled once started.
    Unsupported,
    /// The provider stops at the next safe boundary (e.g. token boundary).
    Cooperative,
    /// The provider stops immediately, mid-token if necessary.
    Immediate,
}

/// Prompt/response cache retention, pi's `CacheRetention` (`docs/02-pi.md`
/// §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

/// The wire transport a generation streams over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Sse,
    WebSocket,
    Auto,
}

/// A versioned snapshot of what a model can do in conversation, resolved per
/// generation and persisted in the run manifest (ARCH §3.1, I5). Every field
/// here is one the I5 gap explicitly names: tool/schema limits, parallel
/// tool calls, reasoning-state carryover, context/output ceilings, cancel
/// semantics, cache behaviour, transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Bumped whenever the field set or its meaning changes, so a persisted
    /// record can be told apart from a stale shape.
    pub schema_version: u32,
    pub max_context_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// Tool/schema limit: the most tools this model accepts in one request.
    pub max_tools: Option<u32>,
    pub supports_parallel_tool_calls: bool,
    pub reasoning_carryover: ReasoningCarryover,
    pub cancel: CancelSemantics,
    pub cache: CacheRetention,
    pub transport: Transport,
}

/// One way a fallback from one model to another would break conversation
/// semantics for a specific `GenRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityGap {
    /// `req` carries reasoning state the target can't accept.
    ReasoningStateCarryoverUnsupported,
    /// `req` requires parallel tool calls the target doesn't support.
    ParallelToolCallsUnsupported,
    /// `req` offers more tools than the target accepts in one request.
    TooManyTools { requested: usize, max: u32 },
    /// `req` asks for more output tokens than the target allows.
    OutputTooLarge { requested: u64, max: u64 },
}

impl ModelCapabilities {
    /// Every way falling `req` back from a model with these capabilities to
    /// a model with `target`'s capabilities would break — empty means the
    /// fallback is safe. Never attempt a fallback without checking this
    /// first (ARCH §3.1): an incompatible switch must be detected and
    /// reported, not silently attempted.
    pub fn fallback_gaps(
        &self,
        target: &ModelCapabilities,
        req: &GenRequest,
    ) -> Vec<CapabilityGap> {
        let mut gaps = Vec::new();

        if req.reasoning_state.is_some()
            && target.reasoning_carryover == ReasoningCarryover::Unsupported
        {
            gaps.push(CapabilityGap::ReasoningStateCarryoverUnsupported);
        }
        if req.parallel_tool_calls && !target.supports_parallel_tool_calls {
            gaps.push(CapabilityGap::ParallelToolCallsUnsupported);
        }
        if let Some(max) = target.max_tools {
            let requested = req.tools.len();
            if requested > max as usize {
                gaps.push(CapabilityGap::TooManyTools { requested, max });
            }
        }
        if let (Some(requested), Some(max)) = (req.max_output_tokens, target.max_output_tokens) {
            if requested > max {
                gaps.push(CapabilityGap::OutputTooLarge { requested, max });
            }
        }

        gaps
    }
}
