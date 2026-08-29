//! Prompted compaction into a clean checkpoint-seeded session.

use std::fmt;

use serde::Serialize;

use crate::budget::BudgetDenied;
use crate::event::domain::{CancelCause, GenerationRole};
use crate::event::id::{Seq, SessionId};
use crate::event::time::Timestamp;
use crate::generation_model_source::GenerationModelError;
use crate::generator::capabilities::CapabilityGap;
use crate::generator::port::RequestExpressibilityError;
use crate::generator::types::{GenRequest, HistoryError, StopReason};
use crate::session::TargetSeed;
use crate::store::session_store::StoreError;

pub const COMPACTION_PROMPT_REVISION: u32 = 1;

pub const COMPACTION_PROMPT: &str = r#"Produce one self-contained context checkpoint for continuing this work in a
new session. Preserve every material fact from the complete context above, but
do not continue the task, address the operator, call tools, or invent missing
information.

Use these headings:
# Objective
# Binding constraints and preferences
# Completed work and evidence
# Work in progress
# Decisions and rationale
# Blockers and uncertainty
# Ordered next steps
# Critical references

Include exact file paths, symbols, commands, results, identifiers, and error
details when they matter. Reconcile repeated or superseded information. If an
earlier context checkpoint is present, preserve every still-relevant fact from
it. Return only the checkpoint."#;

const HEADINGS: [&str; 8] = [
    "# Objective",
    "# Binding constraints and preferences",
    "# Completed work and evidence",
    "# Work in progress",
    "# Decisions and rationale",
    "# Blockers and uncertainty",
    "# Ordered next steps",
    "# Critical references",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint(String);

impl Checkpoint {
    pub fn parse(text: impl Into<String>) -> Result<Self, CheckpointError> {
        let text = text.into();
        let text = text.trim().to_string();
        if !text.starts_with(HEADINGS[0]) {
            return Err(CheckpointError::TextBeforeObjective);
        }

        let mut seen = Vec::new();
        for line in text.lines() {
            if !line.starts_with("# ") {
                continue;
            }
            let Some(index) = HEADINGS.iter().position(|heading| *heading == line) else {
                return Err(CheckpointError::UnknownHeading(line.to_string()));
            };
            if seen.contains(&index) {
                return Err(CheckpointError::DuplicateHeading(line.to_string()));
            }
            seen.push(index);
        }
        if seen.len() != HEADINGS.len() {
            let missing = HEADINGS
                .iter()
                .enumerate()
                .find(|(index, _)| !seen.contains(index))
                .map(|(_, heading)| (*heading).to_string())
                .expect("fewer than eight unique headings leaves one missing");
            return Err(CheckpointError::MissingHeading(missing));
        }
        if seen != (0..HEADINGS.len()).collect::<Vec<_>>() {
            return Err(CheckpointError::OutOfOrderHeading);
        }

        let objective_has_text = text
            .lines()
            .skip(1)
            .take_while(|line| *line != HEADINGS[1])
            .any(|line| !line.trim().is_empty());
        if !objective_has_text {
            return Err(CheckpointError::EmptyObjective);
        }
        Ok(Self(text))
    }

    pub fn text(&self) -> &str {
        &self.0
    }

    pub fn into_text(self) -> String {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointError {
    TextBeforeObjective,
    MissingHeading(String),
    DuplicateHeading(String),
    UnknownHeading(String),
    OutOfOrderHeading,
    EmptyObjective,
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TextBeforeObjective => {
                formatter.write_str("checkpoint must start with # Objective")
            }
            Self::MissingHeading(heading) => write!(formatter, "checkpoint is missing {heading}"),
            Self::DuplicateHeading(heading) => write!(formatter, "checkpoint repeats {heading}"),
            Self::UnknownHeading(heading) => {
                write!(formatter, "checkpoint contains unknown heading {heading}")
            }
            Self::OutOfOrderHeading => formatter.write_str("checkpoint headings are out of order"),
            Self::EmptyObjective => formatter.write_str("checkpoint objective is empty"),
        }
    }
}

impl std::error::Error for CheckpointError {}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedCompaction {
    pub(crate) source: SessionId,
    pub(crate) source_seq: Seq,
    pub(crate) checkpoint: Checkpoint,
    pub(crate) baseline: GenRequest,
}

impl PreparedCompaction {
    pub fn source(&self) -> SessionId {
        self.source
    }

    pub fn source_seq(&self) -> Seq {
        self.source_seq
    }

    pub fn prompt_revision(&self) -> u32 {
        COMPACTION_PROMPT_REVISION
    }

    pub fn checkpoint(&self) -> &Checkpoint {
        &self.checkpoint
    }

    pub fn baseline(&self) -> &GenRequest {
        &self.baseline
    }

    pub fn target_seed(&self, at: Timestamp) -> TargetSeed {
        TargetSeed::compaction(
            self.source,
            self.source_seq,
            COMPACTION_PROMPT_REVISION,
            self.checkpoint.text().to_string(),
            at,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBusy {
    AcceptedPrompt,
    PendingQuestion,
    LiveAttempt,
}

#[derive(Debug)]
pub enum CompactionError {
    NotIdle(CompactionBusy),
    History(HistoryError),
    Resolution {
        role: GenerationRole,
        error: GenerationModelError,
    },
    Capability {
        role: GenerationRole,
        gaps: Vec<CapabilityGap>,
    },
    Expressibility {
        role: GenerationRole,
        error: RequestExpressibilityError,
    },
    Budget(BudgetDenied),
    Cancelled(CancelCause),
    UnexpectedStop(StopReason),
    EmptyCheckpoint,
    InvalidCheckpoint(CheckpointError),
    Store(StoreError),
    Internal(String),
}

impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotIdle(reason) => write!(formatter, "compaction is not idle: {reason:?}"),
            Self::History(error) => write!(formatter, "source history cannot be rebuilt: {error}"),
            Self::Resolution { role, error } => {
                write!(formatter, "{role:?} resolution failed: {error}")
            }
            Self::Capability { role, gaps } => {
                write!(formatter, "{role:?} request has capability gaps: {gaps:?}")
            }
            Self::Expressibility { role, error } => {
                write!(formatter, "{role:?} request cannot be encoded: {error}")
            }
            Self::Budget(error) => write!(formatter, "{error}"),
            Self::Cancelled(cause) => write!(formatter, "compaction was cancelled by {cause:?}"),
            Self::UnexpectedStop(stop) => write!(formatter, "compaction stopped with {stop:?}"),
            Self::EmptyCheckpoint => formatter.write_str("compaction returned an empty checkpoint"),
            Self::InvalidCheckpoint(error) => write!(formatter, "{error}"),
            Self::Store(error) => write!(formatter, "{error}"),
            Self::Internal(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for CompactionError {}

impl From<StoreError> for CompactionError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Serialize)]
struct CheckpointFrame<'a> {
    kind: &'static str,
    revision: u32,
    text: &'a str,
}

pub(crate) fn checkpoint_frame(text: &str) -> String {
    serde_json::to_string(&CheckpointFrame {
        kind: "context_checkpoint",
        revision: COMPACTION_PROMPT_REVISION,
        text,
    })
    .expect("the checkpoint frame contains only serializable fields")
}
