//! The generation controls an operator seat may change mid-session.
//!
//! Model, reasoning effort and permission mode are the three things a person
//! changes while the session is alive. Each has an owner outside this crate —
//! the model selection, the staged policy snapshot — and each already takes
//! effect at a generation boundary rather than immediately, because that is
//! where [`crate::turn::GenerationPolicySource::capture`] and the fresh model
//! resolve both happen.
//!
//! This module is only the port and its vocabulary. It mints no authority: a
//! seat can ask for a value the state already offers, and nothing else. The
//! available list is the whole offer, so an adapter matching an incoming
//! value against it cannot admit a value the owner never published — the same
//! rule the permission seat applies to option ids.

use std::fmt;

use crate::config::{Mode, ModelRef};
use crate::event::id::SessionId;
use crate::generator::types::ThinkingLevel;

/// One control's current value and every value it will accept right now.
///
/// "Right now" is load-bearing. The efforts a model supports depend on the
/// model, so choosing a model changes what the effort control offers, which
/// is why a change answers with the whole state rather than an
/// acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlSelect<T> {
    /// What the next generation uses: whatever the operator chose, or what
    /// configuration resolved for this workspace when they chose nothing.
    pub current: T,
    /// What the operator explicitly chose, if they chose at all.
    ///
    /// Separate from `current` because inheriting a configured default is not
    /// a decision, and a run that recorded it as one would claim a person
    /// picked a model they never saw. Only this half is carried into a run.
    pub chosen: Option<T>,
    pub available: Vec<T>,
}

/// The generation controls as they stand for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionControlState {
    pub model: ControlSelect<ModelRef>,
    pub effort: ControlSelect<ThinkingLevel>,
    pub mode: ControlSelect<Mode>,
}

/// One change an operator seat asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionControlChange {
    Model(ModelRef),
    Effort(ThinkingLevel),
    Mode(Mode),
}

impl SessionControlChange {
    /// The control this change names, for a refusal that says which one.
    pub const fn control(&self) -> &'static str {
        match self {
            Self::Model(_) => "model",
            Self::Effort(_) => "effort",
            Self::Mode(_) => "mode",
        }
    }
}

/// Why a control change did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// The value is not one this control currently offers.
    Unavailable {
        control: &'static str,
        value: String,
    },
    /// The value exists and this seat may still not set it.
    ///
    /// Separate from `Unavailable` because the two are different answers to
    /// the operator: one says "pick from the list", the other says "this
    /// seat cannot do that at all", and collapsing them would hide the
    /// second behind a list that looks merely incomplete.
    Refused { control: &'static str, why: String },
    /// The controls could not be read or written.
    Internal(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { control, value } => {
                write!(f, "`{value}` is not an available {control}")
            }
            Self::Refused { control, why } => write!(f, "the {control} was refused: {why}"),
            Self::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ControlError {}

/// The operator seat's generation controls, one session at a time.
///
/// Session-scoped because one connection can hold several open sessions and
/// a person changing the model in one of them is not saying anything about
/// the others. A process-wide selection would make a picker in one window
/// silently retune every other run the same process is serving.
///
/// `stage` says what the *next* generation uses, never the one that is
/// running. That is not politeness: an in-flight generation already captured
/// its policy and resolved its model, and a permit minted under one mode
/// cannot be re-judged under another halfway through the sibling tool batch
/// it authorised.
pub trait SessionControls: fmt::Debug + Send + Sync {
    /// What can be changed right now, and what it is set to.
    fn state(&self, session: SessionId) -> Result<SessionControlState, ControlError>;

    /// Stage one change and answer with the state that results.
    ///
    /// Answering with the whole state rather than `()` is what lets a host
    /// stay truthful when one change moves another control: picking a model
    /// that supports fewer efforts can move the effort too.
    fn stage(
        &self,
        session: SessionId,
        change: SessionControlChange,
    ) -> Result<SessionControlState, ControlError>;
}
