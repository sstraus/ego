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

use super::types::{ContentBlock, GenRequest, ReasoningState, ThinkingLevel};

/// Every reasoning state `req`'s history carries, in the order it carries
/// them.
fn carried_reasoning(req: &GenRequest) -> Vec<&ReasoningState> {
    req.messages
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Reasoning { state } => Some(state),
            _ => None,
        })
        .collect()
}

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
///
/// One variant, because one is what this build can enact. `Http`
/// (`ego_ai::http`) is request/response plus a byte stream and opens no
/// socket, so there is no seam a second transport could be reached through,
/// no decoder that frames one, and nothing that would select between them.
///
/// enum (`docs/02-pi.md:64`) rather than anything measured here, and neither
/// was ever constructed. `Auto` was the worse of the two on its own terms: it
/// is a selection *policy*, and this record states what a generation did — a
/// manifest saying `auto` names no transport at all.
///
/// They were not merely unreachable. `models.toml` decodes
/// [`ModelCapabilities`] directly (`ego-cli`'s `facts.rs`), so `serde` was a
/// producer an operator could reach: a file could say `web_socket`, decode,
/// and the run would stream SSE anyway. Removing the variants is what makes
/// that file refuse.
///
/// **A variant is added here when a row produces it** — an implemented
/// transport, a decoder that frames it, and a generation that resolves to it.
/// A vendor documenting one as optional beside an SSE this build already
/// speaks is not that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Sse,
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
    /// Which reasoning-state dialect this model mints and accepts, when
    /// anybody said.
    ///
    /// Compared for equality against [`ReasoningState::dialect`], never
    /// matched on: this is the target's half of a check whose other half
    /// travels with the state. It matters only under
    /// [`ReasoningCarryover::SameProviderOnly`], which constrains *whose*
    /// state a model accepts while saying nothing about who minted the state
    /// in hand — so without both tags the check cannot be made at all, and
    /// Anthropic state passes for a Gemini target.
    ///
    /// `None` is "nobody stated one", not "any dialect". A record written
    /// before this field existed decodes that way and has proved nothing
    /// about the model, exactly as a `1` record proves nothing about
    /// [`reasoning_efforts`](ModelCapabilities::reasoning_efforts).
    ///
    /// [`ReasoningState::dialect`]: super::types::ReasoningState::dialect
    #[serde(default)]
    pub reasoning_state_dialect: Option<String>,
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
    /// `req` carries reasoning state minted by one dialect, and this model
    /// accepts only its own — a different one.
    ///
    /// Reached only under [`ReasoningCarryover::SameProviderOnly`]:
    /// `Unsupported` refuses any carried state before the dialects are
    /// consulted, and `Portable` accepts every dialect by definition.
    ReasoningStateDialectMismatch { carried: String, accepted: String },
    /// `req` carries reasoning state and nobody ever stated which dialect
    /// this model speaks, so whether the state is replayable is unknown.
    ///
    /// Separate from [`ReasoningStateDialectMismatch`] on the same grounds
    /// that separate [`ReasoningEffortUnstated`] from
    /// [`ReasoningEffortUnsupported`]: the remedy is "re-resolve this model",
    /// not "carry different state". Failing closed here rather than assuming
    /// a match, because a wrong guess sends state a provider documents a
    /// `400` for.
    ///
    /// [`ReasoningStateDialectMismatch`]: CapabilityGap::ReasoningStateDialectMismatch
    /// [`ReasoningEffortUnstated`]: CapabilityGap::ReasoningEffortUnstated
    /// [`ReasoningEffortUnsupported`]: CapabilityGap::ReasoningEffortUnsupported
    ReasoningStateDialectUnstated { carried: String },
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
            Self::ReasoningStateDialectMismatch { carried, accepted } => write!(
                f,
                "the history carries {carried} reasoning state and this model accepts \
                 only its own {accepted} state"
            ),
            Self::ReasoningStateDialectUnstated { carried } => write!(
                f,
                "the history carries {carried} reasoning state and nobody ever stated \
                 which dialect this model accepts — resolve the model again"
            ),
        }
    }
}

impl ModelCapabilities {
    /// The version this build writes. Bumped whenever the field set or its
    /// meaning changes; `2` added [`reasoning_efforts`], so a `1` record
    /// decodes with [`ReasoningEffortSupport::Unknown`] — which is what it
    /// proved about that model, namely nothing. `3` added
    /// [`reasoning_state_dialect`] on the same terms: a `2` record decodes
    /// with `None`, which is likewise what it proved.
    ///
    /// Read this at merge rather than trusting it. A sibling ADR names a
    /// possible `2` -> `3` for an Anthropic thinking budget; if both land
    /// together it is one bump and not two, and the number here is arithmetic
    /// on the base this branch forked from.
    ///
    /// [`reasoning_efforts`]: ModelCapabilities::reasoning_efforts
    /// [`reasoning_state_dialect`]: ModelCapabilities::reasoning_state_dialect
    pub const SCHEMA_VERSION: u32 = 3;

    /// Every way `req` does not fit *this* model — empty means it can be
    /// sent. Checked after the model resolves and before the budget gate
    /// reserves against it: a reservation taken for a request the model will
    /// reject has authorised spending on something that was never going to be
    /// sent.
    pub fn request_gaps(&self, req: &GenRequest) -> Vec<CapabilityGap> {
        let mut gaps = Vec::new();

        // Read off the conversation rather than a field beside it: the state
        // is positional, so the block list is the only place it can live, and
        // a second copy on the request would be the same fact twice.
        let carried = carried_reasoning(req);
        match self.reasoning_carryover {
            _ if carried.is_empty() => {}
            ReasoningCarryover::Unsupported => {
                gaps.push(CapabilityGap::ReasoningStateCarryoverUnsupported);
            }
            // Constrains *whose* state this model accepts, so the tags have
            // to be compared. Reported once per offending dialect and not
            // once per block: a history with forty blocks from one other
            // provider is one problem with one remedy.
            ReasoningCarryover::SameProviderOnly => {
                let mut reported = BTreeSet::new();
                for state in &carried {
                    match &self.reasoning_state_dialect {
                        Some(mine) if *mine == state.dialect => {}
                        _ if !reported.insert(state.dialect.clone()) => {}
                        Some(mine) => gaps.push(CapabilityGap::ReasoningStateDialectMismatch {
                            carried: state.dialect.clone(),
                            accepted: mine.clone(),
                        }),
                        None => gaps.push(CapabilityGap::ReasoningStateDialectUnstated {
                            carried: state.dialect.clone(),
                        }),
                    }
                }
            }
            ReasoningCarryover::Portable => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The converse of removing the two variants: an operator's file must be
    /// *refused* for naming one, not quietly accepted.
    ///
    /// `models.toml` decodes [`ModelCapabilities`] directly, so `serde` is a
    /// producer an operator can reach. Removing `WebSocket` and `Auto` only
    /// closes that door if the decoder says no, and nothing here asserted it
    /// did — the enum's own doc claims the removal "is what makes that file
    /// refuse", which is a claim about behaviour with no test behind it.
    #[test]
    fn a_transport_this_build_cannot_speak_does_not_decode() {
        for spelling in ["web_socket", "auto", "websocket", "grpc"] {
            let named = format!("\"{spelling}\"");

            assert!(
                serde_json::from_str::<Transport>(&named).is_err(),
                "`{spelling}` decoded as a transport this build cannot speak"
            );
        }
    }

    /// And the one it can still does, so the guard above cannot pass by
    /// refusing everything.
    #[test]
    fn the_transport_this_build_speaks_still_decodes() {
        assert_eq!(
            serde_json::from_str::<Transport>("\"sse\"").expect("sse decodes"),
            Transport::Sse
        );
    }
}
