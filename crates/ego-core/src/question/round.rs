//! Host-facing MRTR rounds. URLs never enter an answer or a model projection.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{QuestionError, QuestionMode, QuestionRequest};

pub const MAX_ELICITATION_REQUESTS: usize = 8;
pub const MAX_ELICITATION_ROUNDS: u8 = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElicitationRound {
    pub requests: BTreeMap<String, ElicitationMember>,
    #[serde(default)]
    pub completed: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElicitationMember {
    pub prompt: String,
    pub kind: ElicitationKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ElicitationKind {
    Form { schema: Value },
    Url { url: String, elicitation_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElicitationAnswers {
    pub confirmed: bool,
    pub responses: BTreeMap<String, Value>,
}

impl ElicitationMember {
    pub fn validate(&self) -> Result<(), QuestionError> {
        if self.prompt.trim().is_empty() {
            return Err(QuestionError::EmptyPrompt);
        }
        match &self.kind {
            ElicitationKind::Form { schema } => self.form_request(schema).validate(),
            ElicitationKind::Url {
                url,
                elicitation_id,
            } => {
                if elicitation_id.trim().is_empty() || elicitation_id.chars().any(char::is_control)
                {
                    return Err(QuestionError::InvalidRound);
                }
                validate_url(url)
            }
        }
    }

    pub fn form_request(&self, schema: &Value) -> QuestionRequest {
        QuestionRequest::new(
            &self.prompt,
            QuestionMode::Form {
                schema: schema.clone(),
            },
            vec![],
            false,
        )
    }

    pub fn validate_answer(&self, answer: &Value) -> Result<(), QuestionError> {
        match &self.kind {
            ElicitationKind::Form { schema } => {
                let validator =
                    jsonschema::validator_for(schema).map_err(|_| QuestionError::InvalidRound)?;
                if validator.is_valid(answer) {
                    Ok(())
                } else {
                    Err(QuestionError::FormSchemaMismatch)
                }
            }
            ElicitationKind::Url { .. } => {
                if answer == &serde_json::json!({"completed":true}) {
                    Ok(())
                } else {
                    Err(QuestionError::WrongAnswerType)
                }
            }
        }
    }
}

impl ElicitationRound {
    pub fn validate(&self) -> Result<(), QuestionError> {
        if self.requests.len() > MAX_ELICITATION_REQUESTS {
            return Err(QuestionError::InvalidRound);
        }
        for (key, request) in &self.requests {
            if key.trim().is_empty() || key.chars().any(char::is_control) {
                return Err(QuestionError::InvalidRound);
            }
            request.validate()?;
        }
        for (key, answer) in &self.completed {
            self.requests
                .get(key)
                .ok_or(QuestionError::InvalidRound)?
                .validate_answer(answer)?;
        }
        Ok(())
    }

    pub fn next_member(&self) -> Option<(&str, &ElicitationMember)> {
        self.requests
            .iter()
            .find(|(key, _)| !self.completed.contains_key(*key))
            .map(|(key, member)| (key.as_str(), member))
    }

    pub fn complete_member(&mut self, key: &str, answer: Value) -> Result<(), QuestionError> {
        if self.completed.contains_key(key) {
            return Err(QuestionError::AlreadySettled);
        }
        self.requests
            .get(key)
            .ok_or(QuestionError::WrongQuestion)?
            .validate_answer(&answer)?;
        self.completed.insert(key.to_owned(), answer);
        Ok(())
    }

    pub fn validate_answers(&self, answers: &ElicitationAnswers) -> Result<(), QuestionError> {
        self.validate()?;
        if !answers.confirmed || answers.responses.len() != self.requests.len() {
            return Err(QuestionError::InvalidRound);
        }
        for (key, member) in &self.requests {
            let answer = answers
                .responses
                .get(key)
                .ok_or(QuestionError::WrongQuestion)?;
            member.validate_answer(answer)?;
            if self
                .completed
                .get(key)
                .is_some_and(|completed| completed != answer)
            {
                return Err(QuestionError::AlreadySettled);
            }
        }
        Ok(())
    }

    /// Protocol-owned response map, with no URL, prompt, or host-supplied identity.
    pub fn input_responses(&self, answers: &ElicitationAnswers) -> Result<Value, QuestionError> {
        self.validate_answers(answers)?;
        let mut responses = serde_json::Map::new();
        for (key, member) in &self.requests {
            let response = match member.kind {
                ElicitationKind::Form { .. } => {
                    serde_json::json!({"action":"accept", "content":answers.responses[key]})
                }
                ElicitationKind::Url { .. } => serde_json::json!({"action":"accept"}),
            };
            responses.insert(key.clone(), response);
        }
        Ok(Value::Object(responses))
    }
}

/// Validation precedes parsing, because URL parsers normalize whitespace.
pub fn validate_url(value: &str) -> Result<(), QuestionError> {
    if value.chars().any(char::is_control) || value.trim() != value {
        return Err(QuestionError::InvalidRound);
    }
    let url = url::Url::parse(value).map_err(|_| QuestionError::InvalidRound)?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => host == "localhost",
        None => false,
    };
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err(QuestionError::InvalidRound);
    }
    Ok(())
}
