//! ACP v1 form elicitation adapter for core-owned human-input questions.

use std::collections::HashSet;
use std::sync::Mutex;

use agent_client_protocol_schema::v1::{
    ClientCapabilities, CreateElicitationRequest, CreateElicitationResponse,
    ElicitationAction, ElicitationContentValue, ElicitationFormMode,
    ElicitationRequestScope, ElicitationSchema, MultiSelectPropertySchema, RequestId,
    StringPropertySchema,
};
use ego_core::event::id::ItemId;
use ego_core::question::{QuestionAnswer, QuestionError, QuestionMode, QuestionOutcome};

pub use ego_core::question::QuestionRequest;

/// The elicitation capability observed on the client's initialize request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilityCapture {
    form: bool,
}

impl CapabilityCapture {
    pub fn form() -> Self {
        Self { form: true }
    }

    pub fn capture_initialize(&mut self, capabilities: &ClientCapabilities) {
        self.form = capabilities
            .elicitation
            .as_ref()
            .and_then(|elicitation| elicitation.form.as_ref())
            .is_some();
    }

    pub fn form_available(&self) -> bool {
        self.form
    }
}

/// A protocol response supplied by the ACP host, including its settlement
/// semantics for the suspended core turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionSettlement {
    outcome: QuestionOutcome,
    closes_sibling_batch: bool,
    closes_turn: bool,
}

impl QuestionSettlement {
    fn question(outcome: QuestionOutcome) -> Self {
        Self {
            outcome,
            closes_sibling_batch: false,
            closes_turn: false,
        }
    }

    fn cancellation() -> Self {
        Self {
            outcome: QuestionOutcome::Cancelled,
            closes_sibling_batch: true,
            closes_turn: true,
        }
    }

    pub fn is_declined(&self) -> bool {
        matches!(self.outcome, QuestionOutcome::Declined)
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self.outcome, QuestionOutcome::Cancelled)
    }

    pub fn closes_sibling_batch(&self) -> bool {
        self.closes_sibling_batch
    }

    pub fn closes_turn(&self) -> bool {
        self.closes_turn
    }

    pub fn tool_result(&self) -> QuestionToolResult {
        QuestionToolResult {
            failure: matches!(
                self.outcome,
                QuestionOutcome::Declined | QuestionOutcome::Cancelled
            ),
        }
    }

    pub fn outcome(&self) -> &QuestionOutcome {
        &self.outcome
    }
}

/// The two properties of the core tool result relevant to ACP settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuestionToolResult {
    failure: bool,
}

impl QuestionToolResult {
    pub fn is_pure(self) -> bool {
        true
    }

    pub fn is_failure(self) -> bool {
        self.failure
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionMappingError {
    InvalidRequest { id: ItemId, reason: String },
    InvalidAnswer { id: ItemId, reason: String },
    CapabilityUnavailable { id: ItemId, guidance: String },
}

impl std::fmt::Display for QuestionMappingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest { id, reason } => {
                write!(f, "question {id} cannot be elicited: {reason}")
            }
            Self::InvalidAnswer { id, reason } => {
                write!(f, "answer for question {id} is invalid: {reason}")
            }
            Self::CapabilityUnavailable { guidance, .. } => f.write_str(guidance),
        }
    }
}

impl std::error::Error for QuestionMappingError {}

/// Stateful ACP boundary. Pending identity is retained when the client cannot
/// elicit a form and removed only by settlement or enclosing cancellation.
#[derive(Debug)]
pub struct AcpQuestionAdapter {
    capabilities: CapabilityCapture,
    response: Option<CreateElicitationResponse>,
    pending: Mutex<HashSet<ItemId>>,
}

impl AcpQuestionAdapter {
    pub fn new(capabilities: CapabilityCapture) -> Self {
        Self {
            capabilities,
            response: None,
            pending: Mutex::new(HashSet::new()),
        }
    }

    pub fn with_response(response: CreateElicitationResponse) -> Self {
        Self {
            capabilities: CapabilityCapture::form(),
            response: Some(response),
            pending: Mutex::new(HashSet::new()),
        }
    }

    pub fn form_request(
        question: ItemId,
        request: &QuestionRequest,
    ) -> Result<CreateElicitationRequest, QuestionMappingError> {
        request.validate().map_err(|error| invalid_request(question, error))?;
        let schema = match request.mode {
            QuestionMode::Single => ElicitationSchema::new().property(
                "answer",
                StringPropertySchema::new()
                    .enum_values(
                        request
                            .options
                            .iter()
                            .map(|option| option.id.clone())
                            .collect::<Vec<_>>(),
                    ),
                true,
            ),
            QuestionMode::Multiple => ElicitationSchema::new().property(
                "answers",
                MultiSelectPropertySchema::new(
                    request.options.iter().map(|option| option.id.clone()).collect(),
                ),
                true,
            ),
            QuestionMode::FreeText => ElicitationSchema::new().string("answer", true),
        };
        let scope = ElicitationRequestScope::new(RequestId::Str(question.to_string()));
        Ok(CreateElicitationRequest::new(
            ElicitationFormMode::new(scope, schema),
            request.prompt.clone(),
        ))
    }

    pub fn validate_response(
        question: ItemId,
        request: &QuestionRequest,
        response: &CreateElicitationResponse,
    ) -> Result<QuestionOutcome, QuestionMappingError> {
        let outcome = match &response.action {
            ElicitationAction::Accept(accepted) => {
                let content = accepted.content.as_ref().ok_or_else(|| invalid_answer(
                    question,
                    "accepted elicitation has no content",
                ))?;
                let answer = match request.mode {
                    QuestionMode::Single => match content.get("answer") {
                        Some(ElicitationContentValue::String(value)) => {
                            QuestionAnswer::single(value.clone())
                        }
                        _ => return Err(invalid_answer(question, "`answer` must be a string")),
                    },
                    QuestionMode::Multiple => match content.get("answers") {
                        Some(ElicitationContentValue::StringArray(values)) => {
                            QuestionAnswer::multiple(values.clone())
                        }
                        _ => {
                            return Err(invalid_answer(
                                question,
                                "`answers` must be an array of strings",
                            ))
                        }
                    },
                    QuestionMode::FreeText => match content.get("answer") {
                        Some(ElicitationContentValue::String(value)) => {
                            QuestionAnswer::free_text(value.clone())
                        }
                        _ => return Err(invalid_answer(question, "`answer` must be a string")),
                    },
                };
                answer
                    .validate_for(request)
                    .map_err(|error| invalid_answer(question, error.to_string()))?;
                QuestionOutcome::Answered(answer)
            }
            ElicitationAction::Decline => QuestionOutcome::Declined,
            ElicitationAction::Cancel => QuestionOutcome::Cancelled,
            ElicitationAction::Other(_) => {
                return Err(invalid_answer(question, "unsupported elicitation action"))
            }
            _ => {
                return Err(invalid_answer(
                    question,
                    "elicitation action is not supported by the pinned ACP contract",
                ))
            }
        };
        Ok(outcome)
    }

    pub fn elicit(
        &self,
        question: ItemId,
        request: &QuestionRequest,
    ) -> Result<QuestionSettlement, QuestionMappingError> {
        self.pending.lock().expect("pending lock poisoned").insert(question);
        if !self.capabilities.form_available() {
            return Err(QuestionMappingError::CapabilityUnavailable {
                id: question,
                guidance: format!(
                    "ACP form elicitation is unavailable; answer the pending question with `ego resume --answer {question}=JSON`"
                ),
            });
        }
        let Some(response) = &self.response else {
            return Err(QuestionMappingError::CapabilityUnavailable {
                id: question,
                guidance: format!(
                    "ACP elicitation has no response; answer the pending question with `ego resume --answer {question}=JSON`"
                ),
            });
        };
        let outcome = Self::validate_response(question, request, response)?;
        self.pending.lock().expect("pending lock poisoned").remove(&question);
        Ok(QuestionSettlement::question(outcome))
    }

    pub fn pending(&self, question: ItemId) -> bool {
        self.pending
            .lock()
            .expect("pending lock poisoned")
            .contains(&question)
    }

    pub fn cancel_process(&self, question: ItemId) -> QuestionSettlement {
        self.pending.lock().expect("pending lock poisoned").remove(&question);
        QuestionSettlement::cancellation()
    }

    pub fn cancel_session(&self, question: ItemId) -> QuestionSettlement {
        self.cancel_process(question)
    }
}

fn invalid_request(question: ItemId, error: QuestionError) -> QuestionMappingError {
    QuestionMappingError::InvalidRequest {
        id: question,
        reason: error.to_string(),
    }
}

fn invalid_answer(question: ItemId, reason: impl Into<String>) -> QuestionMappingError {
    QuestionMappingError::InvalidAnswer {
        id: question,
        reason: reason.into(),
    }
}
