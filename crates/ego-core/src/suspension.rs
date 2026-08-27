//! Durable foreign-operation suspension and continuation journal port.
//!
//! Core owns the state machine and serializable record vocabulary. Adapters
//! persist records and events; they do not reinterpret authority or time.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::event::time::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationKind {
    Tool,
    Resource,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationSubject {
    kind: ContinuationKind,
    server: String,
    operation: String,
    admission_generation: String,
    descriptor_fingerprint: String,
}

impl ContinuationSubject {
    pub fn new(
        server: impl Into<String>,
        operation: impl Into<String>,
        admission_generation: impl Into<String>,
        descriptor_fingerprint: impl Into<String>,
    ) -> Result<Self, ContinuationError> {
        let operation = operation.into();
        let kind = match operation.as_str() {
            "tools/call" => ContinuationKind::Tool,
            "resources/read" => ContinuationKind::Resource,
            "prompts/get" => ContinuationKind::Prompt,
            _ => return Err(ContinuationError::UnsupportedSubject(operation)),
        };
        let subject = Self {
            kind,
            server: server.into(),
            operation,
            admission_generation: admission_generation.into(),
            descriptor_fingerprint: descriptor_fingerprint.into(),
        };
        if [
            &subject.server,
            &subject.admission_generation,
            &subject.descriptor_fingerprint,
        ]
        .into_iter()
        .any(|part| !valid_value(part))
        {
            return Err(ContinuationError::InvalidSubject);
        }
        Ok(subject)
    }

    pub fn kind(&self) -> ContinuationKind {
        self.kind
    }
    pub fn server(&self) -> &str {
        &self.server
    }
    pub fn operation(&self) -> &str {
        &self.operation
    }
    pub fn admission_generation(&self) -> &str {
        &self.admission_generation
    }
    pub fn descriptor_fingerprint(&self) -> &str {
        &self.descriptor_fingerprint
    }
}

/// Integrity-protected authority retained for one exact resumed operation.
///
/// It is intentionally not `Clone`: duplicating a grant would make its
/// single-use property depend on caller discipline. Adapters may serialize
/// the opaque sealed bytes but cannot construct the type without validation.
#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeGrant {
    prepared_call_fingerprint_hash: [u8; 32],
    sealed: Vec<u8>,
}

impl ResumeGrant {
    pub fn new(
        prepared_call_fingerprint: impl Into<String>,
        sealed: &[u8],
    ) -> Result<Self, ContinuationError> {
        let prepared_call_fingerprint = prepared_call_fingerprint.into();
        if !valid_value(&prepared_call_fingerprint) || sealed.is_empty() {
            return Err(ContinuationError::InvalidGrant);
        }
        Ok(Self {
            prepared_call_fingerprint_hash: Sha256::digest(prepared_call_fingerprint.as_bytes())
                .into(),
            sealed: sealed.to_vec(),
        })
    }

    pub fn matches_prepared_call_fingerprint(&self, fingerprint: &str) -> bool {
        let presented = Sha256::digest(fingerprint.as_bytes());
        self.prepared_call_fingerprint_hash
            .iter()
            .zip(presented.iter())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

impl fmt::Debug for ResumeGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumeGrant")
            .field("prepared_call_fingerprint", &"<redacted>")
            .field("sealed", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationState {
    Pending,
    Accepted,
    Claimed,
    Completed,
    Failed,
    Uncertain,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationRecord {
    id: String,
    subject: ContinuationSubject,
    grant: ResumeGrant,
    original_parameters: Vec<u8>,
    request_state: Vec<u8>,
    correlation_schema: Vec<u8>,
    transport_fingerprint: Option<String>,
    accepted_answer: Option<Vec<u8>>,
    expires_at: Timestamp,
    state: ContinuationState,
}

impl ContinuationRecord {
    pub fn new(
        id: impl Into<String>,
        subject: ContinuationSubject,
        grant: ResumeGrant,
        expires_at: Timestamp,
    ) -> Result<Self, ContinuationError> {
        let id = id.into();
        if !valid_value(&id) {
            return Err(ContinuationError::InvalidId);
        }
        Ok(Self {
            id,
            subject,
            grant,
            original_parameters: Vec::new(),
            request_state: Vec::new(),
            correlation_schema: Vec::new(),
            transport_fingerprint: None,
            accepted_answer: None,
            expires_at,
            state: ContinuationState::Pending,
        })
    }

    pub fn with_protocol_state(
        mut self,
        original_parameters: Vec<u8>,
        request_state: Vec<u8>,
        correlation_schema: Vec<u8>,
        transport_fingerprint: impl Into<String>,
    ) -> Result<Self, ContinuationError> {
        let transport_fingerprint = transport_fingerprint.into();
        if original_parameters.is_empty()
            || request_state.is_empty()
            || correlation_schema.is_empty()
            || !valid_value(&transport_fingerprint)
        {
            return Err(ContinuationError::InvalidRecord);
        }
        self.original_parameters = original_parameters;
        self.request_state = request_state;
        self.correlation_schema = correlation_schema;
        self.transport_fingerprint = Some(transport_fingerprint);
        Ok(self)
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn subject(&self) -> &ContinuationSubject {
        &self.subject
    }
    pub fn admission_generation(&self) -> &str {
        self.subject.admission_generation()
    }
    pub fn descriptor_fingerprint(&self) -> &str {
        self.subject.descriptor_fingerprint()
    }
    pub fn matches_prepared_call_fingerprint(&self, fingerprint: &str) -> bool {
        self.grant.matches_prepared_call_fingerprint(fingerprint)
    }
    pub fn transport_fingerprint(&self) -> Option<&str> {
        self.transport_fingerprint.as_deref()
    }
    pub fn expires_at(&self) -> Timestamp {
        self.expires_at
    }
    pub fn state(&self) -> ContinuationState {
        self.state
    }

    pub fn validate_fingerprints(
        &self,
        admission_generation: &str,
        descriptor_fingerprint: &str,
        prepared_call_fingerprint: &str,
    ) -> Result<(), ContinuationError> {
        if self.subject.admission_generation() != admission_generation
            || self.subject.descriptor_fingerprint() != descriptor_fingerprint
            || !self
                .grant
                .matches_prepared_call_fingerprint(prepared_call_fingerprint)
        {
            return Err(ContinuationError::AuthorityMismatch);
        }
        Ok(())
    }

    pub fn accept_answer(&mut self, answer: Vec<u8>) -> Result<(), ContinuationError> {
        if self.state != ContinuationState::Pending || self.accepted_answer.is_some() {
            return Err(ContinuationError::DuplicateAnswer);
        }
        if answer.is_empty() {
            return Err(ContinuationError::InvalidAnswer);
        }
        self.accepted_answer = Some(answer);
        self.state = ContinuationState::Accepted;
        Ok(())
    }

    pub fn claim(&mut self, now: Timestamp) -> Result<ClaimOutcome, ContinuationError> {
        if now >= self.expires_at {
            return Err(ContinuationError::Expired);
        }
        match self.state {
            ContinuationState::Accepted => {
                self.state = ContinuationState::Claimed;
                Ok(ClaimOutcome::First)
            }
            ContinuationState::Claimed
            | ContinuationState::Completed
            | ContinuationState::Failed
            | ContinuationState::Uncertain => Ok(ClaimOutcome::Duplicate),
            ContinuationState::Pending => Err(ContinuationError::AnswerRequired),
        }
    }

    pub fn settle(&mut self, state: ContinuationState) -> Result<(), ContinuationError> {
        if self.state != ContinuationState::Claimed
            || !matches!(
                state,
                ContinuationState::Completed
                    | ContinuationState::Failed
                    | ContinuationState::Uncertain
            )
        {
            return Err(ContinuationError::InvalidTransition);
        }
        self.state = state;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimOutcome {
    First,
    Duplicate,
}

impl ClaimOutcome {
    pub fn is_first(self) -> bool {
        matches!(self, Self::First)
    }
    pub fn is_duplicate(self) -> bool {
        matches!(self, Self::Duplicate)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ContinuationEvent {
    Recorded {
        record: ContinuationRecordEvent,
    },
    AnswerAccepted {
        id: String,
        answer: Vec<u8>,
    },
    Claimed {
        id: String,
        at: Timestamp,
    },
    Settled {
        id: String,
        state: ContinuationState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationRecordEvent {
    pub id: String,
    pub subject: ContinuationSubject,
    pub expires_at: Timestamp,
}

impl From<&ContinuationRecord> for ContinuationRecordEvent {
    fn from(record: &ContinuationRecord) -> Self {
        Self {
            id: record.id.clone(),
            subject: record.subject.clone(),
            expires_at: record.expires_at,
        }
    }
}

/// Persistence boundary for durable, atomic continuation transitions.
pub trait ContinuationJournal: Send + Sync {
    fn record(&self, record: ContinuationRecord) -> Result<(), ContinuationError>;
    fn accept_answer(&self, id: &str, answer: &[u8]) -> Result<(), ContinuationError>;
    fn load(&self, id: &str) -> Result<ContinuationRecord, ContinuationError>;
    fn claim(&self, id: &str, now: Timestamp) -> Result<ClaimOutcome, ContinuationError>;
    fn settle(&self, id: &str, state: ContinuationState) -> Result<(), ContinuationError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationError {
    InvalidId,
    InvalidSubject,
    UnsupportedSubject(String),
    InvalidGrant,
    InvalidRecord,
    InvalidAnswer,
    DuplicateAnswer,
    AnswerRequired,
    AuthorityMismatch,
    Expired,
    NotFound,
    InvalidTransition,
    Corrupt(String),
    Unavailable(String),
}

pub type JournalError = ContinuationError;

impl ContinuationError {
    pub fn is_expired(&self) -> bool {
        matches!(self, Self::Expired)
    }
}

impl fmt::Display for ContinuationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId => formatter.write_str("invalid continuation identifier"),
            Self::InvalidSubject => formatter.write_str("invalid continuation subject"),
            Self::UnsupportedSubject(operation) => {
                write!(formatter, "unsupported continuation subject: {operation}")
            }
            Self::InvalidGrant => formatter.write_str("invalid sealed resume grant"),
            Self::InvalidRecord => formatter.write_str("invalid continuation record"),
            Self::InvalidAnswer => formatter.write_str("invalid continuation answer"),
            Self::DuplicateAnswer => {
                formatter.write_str("continuation answer was already accepted")
            }
            Self::AnswerRequired => formatter.write_str("continuation answer is required"),
            Self::AuthorityMismatch => formatter.write_str("continuation authority mismatch"),
            Self::Expired => formatter.write_str("continuation expired"),
            Self::NotFound => formatter.write_str("continuation not found"),
            Self::InvalidTransition => formatter.write_str("invalid continuation state transition"),
            Self::Corrupt(message) => write!(formatter, "corrupt continuation journal: {message}"),
            Self::Unavailable(message) => {
                write!(formatter, "continuation journal unavailable: {message}")
            }
        }
    }
}

impl std::error::Error for ContinuationError {}

fn valid_value(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}
