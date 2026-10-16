//! Model-requested human input: typed requests, canonical answers, and the
//! consumer-owned elicitation boundary.

use std::collections::HashSet;

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

mod round;
pub use round::*;

use crate::event::id::ItemId;
use crate::generator::types::ToolDescriptor;

/// The shape of answer a question accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionMode {
    Single,
    Multiple,
    FreeText,
    Form {
        schema: Value,
    },
    #[schemars(skip)]
    Round(ElicitationRound),
}

/// The durable consumer of one human answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionTarget {
    ModelTool {
        tool_call: ItemId,
    },
    ForeignContinuation {
        tool_call: ItemId,
        continuation_id: String,
    },
}

impl QuestionTarget {
    pub fn tool_call(&self) -> ItemId {
        match self {
            Self::ModelTool { tool_call } | Self::ForeignContinuation { tool_call, .. } => {
                *tool_call
            }
        }
    }

    pub fn validate(&self) -> Result<(), QuestionError> {
        match self {
            Self::ModelTool { .. } => Ok(()),
            Self::ForeignContinuation {
                continuation_id, ..
            } if continuation_id.trim().is_empty()
                || continuation_id.chars().any(char::is_control) =>
            {
                Err(QuestionError::InvalidTarget)
            }
            Self::ForeignContinuation { .. } => Ok(()),
        }
    }
}

/// One stable, model-visible choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    /// One short sentence that tells the human what choosing this option
    /// means. Shown under the label; omit it when the label says enough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl QuestionOption {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// A canonical question parsed from a model tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionRequest {
    pub prompt: String,
    pub mode: QuestionMode,
    pub options: Vec<QuestionOption>,
    pub free_text: bool,
}

impl QuestionRequest {
    pub fn new(
        prompt: impl Into<String>,
        mode: QuestionMode,
        options: Vec<QuestionOption>,
        free_text: bool,
    ) -> Self {
        Self {
            prompt: prompt.into(),
            mode,
            options,
            free_text,
        }
    }

    /// Validate the complete request before it becomes durable.
    pub fn validate(&self) -> Result<(), QuestionError> {
        if self.prompt.trim().is_empty() {
            return Err(QuestionError::EmptyPrompt);
        }
        let mut ids = HashSet::with_capacity(self.options.len());
        for option in &self.options {
            if option.id.trim().is_empty() {
                return Err(QuestionError::EmptyOptionId);
            }
            if option.label.trim().is_empty() {
                return Err(QuestionError::EmptyOptionLabel);
            }
            if !ids.insert(option.id.as_str()) {
                return Err(QuestionError::DuplicateOptionId(option.id.clone()));
            }
        }
        match &self.mode {
            QuestionMode::Round(round) => {
                if !self.options.is_empty() || self.free_text {
                    return Err(QuestionError::InvalidRound);
                }
                round.validate()
            }
            QuestionMode::Single | QuestionMode::Multiple if self.options.is_empty() => {
                Err(QuestionError::OptionsRequired)
            }
            QuestionMode::FreeText if !self.options.is_empty() => {
                Err(QuestionError::FreeTextHasOptions)
            }
            QuestionMode::FreeText if !self.free_text => Err(QuestionError::FreeTextDisabled),
            QuestionMode::Form { schema } => {
                if !self.options.is_empty() || self.free_text {
                    return Err(QuestionError::InvalidFormGrammar);
                }
                if schema.get("type").and_then(Value::as_str) != Some("object") {
                    return Err(QuestionError::InvalidFormSchema(
                        "the form schema root must have type object".to_owned(),
                    ));
                }
                jsonschema::validator_for(schema)
                    .map(|_| ())
                    .map_err(|error| QuestionError::InvalidFormSchema(error.to_string()))
            }
            _ => Ok(()),
        }
    }

    fn accepts(&self, value: &str) -> bool {
        self.free_text || self.options.iter().any(|option| option.id == value)
    }
}

/// A validated answer. Its variant records the request grammar that admitted
/// it, so persistence never has to infer a mode from an untyped JSON value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum QuestionAnswer {
    Single(String),
    Multiple(Vec<String>),
    FreeText(String),
    Form(Map<String, Value>),
    Round(ElicitationAnswers),
}

impl QuestionAnswer {
    pub fn single(value: impl Into<String>) -> Self {
        Self::Single(value.into())
    }

    pub fn multiple<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Multiple(values.into_iter().map(Into::into).collect())
    }

    pub fn free_text(value: impl Into<String>) -> Self {
        Self::FreeText(value.into())
    }

    pub fn form(value: Map<String, Value>) -> Self {
        Self::Form(value)
    }

    /// Parse the JSON spelling accepted by `ego resume --answer` and validate
    /// it against the exact durable request.
    pub fn from_json(request: &QuestionRequest, json: &str) -> Result<Self, QuestionError> {
        request.validate()?;
        let value: Value = serde_json::from_str(json)
            .map_err(|error| QuestionError::MalformedJson(error.to_string()))?;
        let answer = match &request.mode {
            QuestionMode::Round(_) => Self::Round(
                serde_json::from_value(value).map_err(|_| QuestionError::WrongAnswerType)?,
            ),
            QuestionMode::Single => Self::Single(non_empty_string(value)?),
            QuestionMode::Multiple => {
                let Value::Array(values) = value else {
                    return Err(QuestionError::WrongAnswerType);
                };
                if values.is_empty() {
                    return Err(QuestionError::EmptyAnswer);
                }
                let mut unique = HashSet::with_capacity(values.len());
                let mut answers = Vec::with_capacity(values.len());
                for value in values {
                    let answer = non_empty_string(value)?;
                    if !unique.insert(answer.clone()) {
                        return Err(QuestionError::DuplicateAnswer(answer));
                    }
                    answers.push(answer);
                }
                Self::Multiple(answers)
            }
            QuestionMode::FreeText => Self::FreeText(non_empty_string(value)?),
            QuestionMode::Form { .. } => {
                let Value::Object(value) = value else {
                    return Err(QuestionError::WrongAnswerType);
                };
                Self::Form(value)
            }
        };
        answer.validate_for(request)?;
        Ok(answer)
    }

    pub fn validate_for(&self, request: &QuestionRequest) -> Result<(), QuestionError> {
        request.validate()?;
        match (&request.mode, self) {
            (QuestionMode::Round(round), Self::Round(answers)) => round.validate_answers(answers),
            (QuestionMode::Single, Self::Single(value))
            | (QuestionMode::FreeText, Self::FreeText(value)) => validate_value(request, value),
            (QuestionMode::Multiple, Self::Multiple(values)) => {
                if values.is_empty() {
                    return Err(QuestionError::EmptyAnswer);
                }
                let mut unique = HashSet::with_capacity(values.len());
                for value in values {
                    validate_value(request, value)?;
                    if !unique.insert(value) {
                        return Err(QuestionError::DuplicateAnswer(value.clone()));
                    }
                }
                Ok(())
            }
            (QuestionMode::Form { schema }, Self::Form(value)) => {
                let value = Value::Object(value.clone());
                let validator = jsonschema::validator_for(schema)
                    .map_err(|error| QuestionError::InvalidFormSchema(error.to_string()))?;
                if validator.is_valid(&value) {
                    Ok(())
                } else {
                    Err(QuestionError::FormSchemaMismatch)
                }
            }
            _ => Err(QuestionError::WrongAnswerType),
        }
    }

    /// The complete pure result returned to the model-visible tool call.
    pub fn model_text(&self) -> String {
        match self {
            Self::Round(answers) => {
                serde_json::to_string(answers).expect("round answers serialize")
            }
            Self::Single(value) | Self::FreeText(value) => value.clone(),
            Self::Multiple(values) => serde_json::to_string(values)
                .expect("a vector of strings always serializes to JSON"),
            Self::Form(value) => {
                serde_json::to_string(value).expect("a JSON object always serializes to JSON")
            }
        }
    }
}

fn non_empty_string(value: Value) -> Result<String, QuestionError> {
    let Value::String(value) = value else {
        return Err(QuestionError::WrongAnswerType);
    };
    if value.trim().is_empty() {
        return Err(QuestionError::EmptyAnswer);
    }
    Ok(value)
}

fn validate_value(request: &QuestionRequest, value: &str) -> Result<(), QuestionError> {
    if value.trim().is_empty() {
        return Err(QuestionError::EmptyAnswer);
    }
    if !request.accepts(value) {
        return Err(QuestionError::UnknownOption(value.to_string()));
    }
    Ok(())
}

/// Every terminal outcome from the elicitation boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "answer", rename_all = "snake_case")]
pub enum QuestionOutcome {
    Answered(QuestionAnswer),
    Declined,
    Cancelled,
    Unavailable,
    /// One member becomes durable before asking another or approving a retry.
    MemberCompleted {
        key: String,
        answer: Value,
    },
}

/// Validation failures are usage errors and never mutate reducer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionError {
    EmptyPrompt,
    EmptyOptionId,
    EmptyOptionLabel,
    DuplicateOptionId(String),
    OptionsRequired,
    FreeTextHasOptions,
    FreeTextDisabled,
    InvalidFormGrammar,
    InvalidFormSchema(String),
    FormSchemaMismatch,
    MalformedJson(String),
    WrongAnswerType,
    EmptyAnswer,
    DuplicateAnswer(String),
    UnknownOption(String),
    NoPendingQuestion,
    WrongQuestion,
    PendingQuestion,
    AlreadySettled,
    ForeignContinuationRequired,
    InvalidTarget,
    InvalidRound,
}

impl std::fmt::Display for QuestionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPrompt => f.write_str("the question prompt is empty"),
            Self::EmptyOptionId => f.write_str("a question option id is empty"),
            Self::EmptyOptionLabel => f.write_str("a question option label is empty"),
            Self::DuplicateOptionId(id) => write!(f, "question option id `{id}` is duplicated"),
            Self::OptionsRequired => f.write_str("this question mode requires options"),
            Self::FreeTextHasOptions => f.write_str("a free-text question cannot have options"),
            Self::FreeTextDisabled => f.write_str("free-text mode requires free_text=true"),
            Self::InvalidFormGrammar => {
                f.write_str("a form question cannot have options or enable free text")
            }
            Self::InvalidFormSchema(error) => write!(f, "the form schema is invalid: {error}"),
            Self::FormSchemaMismatch => f.write_str("the form answer does not match its schema"),
            Self::MalformedJson(error) => write!(f, "the answer is not valid JSON: {error}"),
            Self::WrongAnswerType => f.write_str("the answer has the wrong JSON type"),
            Self::EmptyAnswer => f.write_str("the answer is empty"),
            Self::DuplicateAnswer(value) => write!(f, "answer `{value}` is duplicated"),
            Self::UnknownOption(value) => write!(f, "answer `{value}` is not an allowed option"),
            Self::NoPendingQuestion => f.write_str("there is no pending question"),
            Self::WrongQuestion => f.write_str("the answer names a different question"),
            Self::PendingQuestion => {
                f.write_str("a pending question must be answered before a new prompt")
            }
            Self::AlreadySettled => f.write_str("the question has already been settled"),
            Self::ForeignContinuationRequired => {
                f.write_str("the foreign question requires its continuation adapter")
            }
            Self::InvalidRound => f.write_str("the elicitation round is invalid or incomplete"),
            Self::InvalidTarget => f.write_str("the question target is invalid"),
        }
    }
}

impl std::error::Error for QuestionError {}

/// Parses a model-visible human-input call without granting effect authority.
pub trait HumanInputTool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    fn parse(&self, raw: Value) -> Result<QuestionRequest, String>;
}

/// The built-in parser for the `ask_question` model tool.
#[derive(Debug, Clone, Copy, Default)]
pub struct AskQuestionTool;

impl HumanInputTool for AskQuestionTool {
    fn descriptor(&self) -> ToolDescriptor {
        ask_question_descriptor()
    }

    fn parse(&self, raw: Value) -> Result<QuestionRequest, String> {
        let request: QuestionRequest =
            serde_json::from_value(raw).map_err(|error| error.to_string())?;
        request.validate().map_err(|error| error.to_string())?;
        if matches!(request.mode, QuestionMode::Round(_)) {
            return Err("external elicitation rounds cannot originate from model tools".to_owned());
        }
        Ok(request)
    }
}

/// The name the model calls the question tool by.
pub const ASK_QUESTION_TOOL: &str = "ask_question";

/// The model-facing descriptor shared by every host adapter.
pub fn ask_question_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: ASK_QUESTION_TOOL.to_string(),
        description: "Ask the human one structured question and wait for its answer".to_string(),
        parameters: schemars::schema_for!(QuestionRequest).to_value(),
    }
}

/// A failure of the host adapter or its transport while eliciting an answer.
///
/// This is deliberately distinct from [`QuestionOutcome::Unavailable`]: an
/// unavailable capability is a successful host answer that suspends the turn,
/// while this value terminates the active run and leaves the durable request
/// pending for recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitorError {
    message: String,
}

impl ElicitorError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ElicitorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ElicitorError {}

impl From<String> for ElicitorError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ElicitorError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Core's narrow human-input port. Terminal and ACP crates own its adapters.
pub trait Elicitor: Send + Sync {
    fn elicit<'a>(
        &'a self,
        question: crate::event::id::ItemId,
        request: QuestionRequest,
    ) -> BoxFuture<'a, Result<QuestionOutcome, ElicitorError>>;
}
