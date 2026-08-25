//! ACP v1 form elicitation adapter for core-owned human-input questions.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use agent_client_protocol::{Client, ConnectionTo};
use agent_client_protocol_schema::v1::{
    ClientCapabilities, CreateElicitationRequest, CreateElicitationResponse, ElicitationAction,
    ElicitationContentValue, ElicitationFormMode, ElicitationRequestScope, ElicitationSchema,
    MultiSelectPropertySchema, RequestId, StringPropertySchema,
};
use ego_core::event::id::ItemId;
use ego_core::question::{
    Elicitor, ElicitorError, QuestionAnswer, QuestionError, QuestionMode, QuestionOutcome,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionMappingError {
    InvalidRequest { id: ItemId, reason: String },
    InvalidAnswer { id: ItemId, reason: String },
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
        }
    }
}

impl std::error::Error for QuestionMappingError {}

/// One connection's ACP elicitation boundary. Durable pending state and
/// cancellation remain core-owned.
#[derive(Clone)]
pub struct AcpQuestionAdapter {
    capabilities: CapabilityCapture,
    connection: Option<ConnectionTo<Client>>,
}

impl std::fmt::Debug for AcpQuestionAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpQuestionAdapter")
            .field("capabilities", &self.capabilities)
            .field("connected", &self.connection.is_some())
            .finish()
    }
}

impl AcpQuestionAdapter {
    pub fn new(capabilities: CapabilityCapture) -> Self {
        Self {
            capabilities,
            connection: None,
        }
    }

    pub fn live(capabilities: CapabilityCapture, connection: ConnectionTo<Client>) -> Self {
        Self {
            capabilities,
            connection: Some(connection),
        }
    }

    pub fn elicitor(&self) -> Arc<dyn Elicitor> {
        Arc::new(self.clone())
    }

    pub fn form_request(
        question: ItemId,
        request: &QuestionRequest,
    ) -> Result<CreateElicitationRequest, QuestionMappingError> {
        request
            .validate()
            .map_err(|error| invalid_request(question, error))?;
        let schema = match request.mode {
            QuestionMode::Single => ElicitationSchema::new().property(
                "answer",
                StringPropertySchema::new().enum_values(
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
                    request
                        .options
                        .iter()
                        .map(|option| option.id.clone())
                        .collect(),
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
                let content = accepted.content.as_ref().ok_or_else(|| {
                    invalid_answer(question, "accepted elicitation has no content")
                })?;
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
                            ));
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
                return Err(invalid_answer(question, "unsupported elicitation action"));
            }
            _ => {
                return Err(invalid_answer(
                    question,
                    "elicitation action is not supported by the pinned ACP contract",
                ));
            }
        };
        Ok(outcome)
    }

    async fn elicit_live(
        &self,
        question: ItemId,
        request: QuestionRequest,
    ) -> Result<QuestionOutcome, ElicitorError> {
        if !self.capabilities.form_available() {
            return Ok(QuestionOutcome::Unavailable);
        }
        let connection = self.connection.as_ref().ok_or_else(|| {
            ElicitorError::new(format!(
                "question {question} advertised ACP form elicitation without a live connection"
            ))
        })?;
        let wire = Self::form_request(question, &request)
            .map_err(|error| ElicitorError::new(error.to_string()))?;
        let response = connection
            .send_request(wire)
            .block_task()
            .await
            .map_err(|error| ElicitorError::new(error.to_string()))?;
        Self::validate_response(question, &request, &response)
            .map_err(|error| ElicitorError::new(error.to_string()))
    }
}

impl Elicitor for AcpQuestionAdapter {
    fn elicit<'a>(
        &'a self,
        question: ItemId,
        request: QuestionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<QuestionOutcome, ElicitorError>> + Send + 'a>> {
        Box::pin(self.elicit_live(question, request))
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
