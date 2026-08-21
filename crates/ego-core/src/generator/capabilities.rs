//! `ModelCapabilities`: the versioned record ARCH §3.1 and I5
//! (`docs/08-codex-critique.md`) require before treating a shared HTTP shape
//! as equivalent conversation semantics.
//!
//! A provider row is configuration, not proof two models behave the same in
//! conversation. `ModelCapabilities` is resolved per generation and checked
//! before a fallback: `fallback_gaps` reports every incompatibility rather
//! than letting a switch proceed and silently drop or mistranslate state.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::types::{GenRequest, ThinkingLevel};

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

/// Which [`ThinkingLevel`]s a model accepts on the wire.
///
/// Three states rather than an `Option`, decided in
/// `docs/decisions/2026-08-21-grok-as-a-provider.md` §4 after Codex refused
/// the `Option` this repository first proposed. `None` would have had to mean
/// "nothing stated, so send anything", and an absent field read as a
/// permissive fact is the mistake
/// `2026-08-21-what-a-model-resolves-to.md` §4 already refused twice: it is
/// `/api/show`'s architecture context read as a served ceiling, and `:cloud`'s
/// `auth: None` read as "not billed". A model that states nothing about
/// `reasoning_effort` is not a model that accepts every level; it is a model
/// nobody asked.
///
/// The distinction is load-bearing because the parameter is not advisory.
/// `reasoning_effort` is always emitted — omitting it selects the schema's
/// documented default of `medium`, which would bill a caller for reasoning
/// they switched off — and an unrecognised value is a hard `400`, not an
/// ignore. xAI accepts a different subset per model; Groq accepts only two
/// values; some rows reject the parameter outright.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffortSupport {
    /// Nobody stated it. Refuse until this model is resolved again.
    ///
    /// What a [`ModelCapabilities`] persisted before this field existed
    /// decodes as, because that is what it proved — hence [`Default`], which
    /// is what `serde` reaches for when the field is absent.
    #[default]
    Unknown,
    /// Measured, not assumed: every level [`ThinkingLevel`] can produce is
    /// accepted. A claim a row may only make from something it observed.
    Any,
    /// Exactly these. Anything else is refused naming both sides.
    Only(BTreeSet<ThinkingLevel>),
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
    /// The reasoning efforts this model accepts. Supplied for hosted models,
    /// discovered for local ones, and [`Unknown`] for anything that predates
    /// the field — see [`ReasoningEffortSupport`].
    ///
    /// [`Unknown`]: ReasoningEffortSupport::Unknown
    #[serde(default)]
    pub reasoning_efforts: ReasoningEffortSupport,
    pub cancel: CancelSemantics,
    pub cache: CacheRetention,
    pub transport: Transport,
}

/// One way a `GenRequest` does not fit a model's capabilities.
///
/// Originally written for a fallback — "would this request still fit if we
/// switched models" — which turned out to be the narrower reading of the same
/// sentence. Every variant is equally a statement about a *first* attempt, so
/// [`request_gaps`](ModelCapabilities::request_gaps) asks it of one model and
/// [`fallback_gaps`](ModelCapabilities::fallback_gaps) asks it of the model
/// being switched to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityGap {
    /// `req` carries reasoning state the model can't accept.
    ReasoningStateCarryoverUnsupported,
    /// `req` requires parallel tool calls the model doesn't support.
    ParallelToolCallsUnsupported,
    /// `req` offers more tools than the model accepts in one request.
    TooManyTools { requested: usize, max: u32 },
    /// `req` asks for more output tokens than the model allows.
    OutputTooLarge { requested: u64, max: u64 },
    /// Nobody ever stated which reasoning efforts this model accepts, so
    /// sending one would be a guess. Distinct from
    /// [`ReasoningEffortUnsupported`] because the answer is "re-resolve this
    /// model", not "ask for a different level".
    ///
    /// [`ReasoningEffortUnsupported`]: CapabilityGap::ReasoningEffortUnsupported
    ReasoningEffortUnstated { requested: ThinkingLevel },
    /// The model states what it accepts, and `req` asked for something else.
    /// Both sides travel, so the refusal can name the levels that would work.
    ReasoningEffortUnsupported {
        requested: ThinkingLevel,
        accepted: BTreeSet<ThinkingLevel>,
    },
}

/// What the caller is told. Every gap names both what was asked for and what
/// would have worked, because a usage error the caller has to guess at is only
/// marginally better than no message.
impl std::fmt::Display for CapabilityGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReasoningStateCarryoverUnsupported => {
                write!(
                    f,
                    "this model does not accept reasoning state carried over from another generation"
                )
            }
            Self::ParallelToolCallsUnsupported => {
                write!(f, "this model does not support parallel tool calls")
            }
            Self::TooManyTools { requested, max } => {
                write!(
                    f,
                    "{requested} tools offered, this model accepts at most {max}"
                )
            }
            Self::OutputTooLarge { requested, max } => {
                write!(
                    f,
                    "{requested} output tokens requested, this model allows at most {max}"
                )
            }
            Self::ReasoningEffortUnstated { requested } => write!(
                f,
                "reasoning effort {requested:?} was requested, but nobody ever stated which \
                 efforts this model accepts — resolve the model again"
            ),
            Self::ReasoningEffortUnsupported {
                requested,
                accepted,
            } => {
                let accepted: Vec<_> = accepted.iter().map(|level| format!("{level:?}")).collect();
                write!(
                    f,
                    "reasoning effort {requested:?} is not accepted by this model; it accepts {}",
                    accepted.join(", ")
                )
            }
        }
    }
}

impl ModelCapabilities {
    /// The version this build writes. Bumped whenever the field set or its
    /// meaning changes; `2` added [`reasoning_efforts`], so a `1` record
    /// decodes with [`ReasoningEffortSupport::Unknown`] — which is what it
    /// proved about that model, namely nothing.
    ///
    /// [`reasoning_efforts`]: ModelCapabilities::reasoning_efforts
    pub const SCHEMA_VERSION: u32 = 2;

    /// Every way `req` does not fit *this* model — empty means it can be
    /// sent. Checked after the model resolves and before the budget gate
    /// reserves against it: a reservation taken for a request the model will
    /// reject has authorised spending on something that was never going to be
    /// sent.
    pub fn request_gaps(&self, req: &GenRequest) -> Vec<CapabilityGap> {
        let mut gaps = Vec::new();

        if req.reasoning_state.is_some()
            && self.reasoning_carryover == ReasoningCarryover::Unsupported
        {
            gaps.push(CapabilityGap::ReasoningStateCarryoverUnsupported);
        }
        if req.parallel_tool_calls && !self.supports_parallel_tool_calls {
            gaps.push(CapabilityGap::ParallelToolCallsUnsupported);
        }
        if let Some(max) = self.max_tools {
            let requested = req.tools.len();
            if requested > max as usize {
                gaps.push(CapabilityGap::TooManyTools { requested, max });
            }
        }
        if let (Some(requested), Some(max)) = (req.max_output_tokens, self.max_output_tokens)
            && requested > max
        {
            gaps.push(CapabilityGap::OutputTooLarge { requested, max });
        }
        match &self.reasoning_efforts {
            ReasoningEffortSupport::Unknown => {
                gaps.push(CapabilityGap::ReasoningEffortUnstated {
                    requested: req.thinking,
                });
            }
            ReasoningEffortSupport::Any => {}
            ReasoningEffortSupport::Only(accepted) if !accepted.contains(&req.thinking) => {
                gaps.push(CapabilityGap::ReasoningEffortUnsupported {
                    requested: req.thinking,
                    accepted: accepted.clone(),
                });
            }
            ReasoningEffortSupport::Only(_) => {}
        }

        gaps
    }

    /// Every way falling `req` back from a model with these capabilities to a
    /// model with `target`'s would break — empty means the fallback is safe.
    /// Never attempt a fallback without checking this first (ARCH §3.1): an
    /// incompatible switch must be detected and reported, not silently
    /// attempted.
    ///
    /// This is [`request_gaps`](Self::request_gaps) asked of the target, and
    /// nothing else. The question "does `req` fit `target`" does not depend on
    /// where `req` was going to go before — which is why `self` is unused, and
    /// why the two are one function rather than two that can drift apart. The
    /// name is kept because the *reason to ask* differs, and a call site that
    /// says `fallback_gaps` is saying which of the two it means.
    pub fn fallback_gaps(
        &self,
        target: &ModelCapabilities,
        req: &GenRequest,
    ) -> Vec<CapabilityGap> {
        target.request_gaps(req)
    }
}
