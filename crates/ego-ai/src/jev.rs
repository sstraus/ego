//! One Decisions client for router, destructive and wake questions.
pub mod calibration;
mod configured;
pub mod router;
use crate::http::{AdmittedHttpProfile, Http, HttpRequest, Method, bearer_header, public_header};
pub use configured::ConfiguredDecisions;
use ego_core::credentials::SecretMaterial;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use url::Url;

/// Closed errors never contain judged text or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionError {
    InvalidConfiguration,
    CredentialsUnavailable,
    InvalidRequest,
    Transport,
    Timeout,
    HttpStatus(u16),
    InvalidResponse,
}
impl std::fmt::Display for DecisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HttpStatus(status) => write!(f, "Decisions endpoint answered {status}"),
            other => f.write_str(match other {
                Self::CredentialsUnavailable => "Decisions credential is unavailable",
                Self::InvalidConfiguration => "invalid Decisions configuration",
                Self::InvalidRequest => "invalid Decisions request",
                Self::Transport => "Decisions transport failed",
                Self::Timeout => "Decisions deadline exceeded",
                Self::InvalidResponse => "invalid Decisions response",
                Self::HttpStatus(_) => unreachable!(),
            }),
        }
    }
}
impl std::error::Error for DecisionError {}

/// Validated answers and provider-reported USD cost; absent cost stays unknown.
#[derive(Debug, Clone)]
pub struct DecisionResponse {
    pub answers: BTreeMap<String, f64>,
    pub cost_usd: Option<String>,
}

/// No retry, implicit endpoint, or provider fallback is performed.
pub struct DecisionsClient {
    endpoint: Url,
    model: String,
    timeout: Duration,
    profile: AdmittedHttpProfile,
}
impl DecisionsClient {
    /// Prepare the exact endpoint selected by trusted configuration.
    pub fn new(endpoint: Url, model: String, timeout: Duration) -> Result<Self, DecisionError> {
        if model.trim().is_empty() || timeout.is_zero() {
            return Err(DecisionError::InvalidConfiguration);
        }
        let profile =
            AdmittedHttpProfile::from_urls("jev.decisions", std::slice::from_ref(&endpoint))
                .map_err(|_| DecisionError::InvalidConfiguration)?;
        Ok(Self {
            endpoint,
            model,
            timeout,
            profile,
        })
    }
    /// Return each named noul probability or refuse the whole answer.
    pub async fn ask(
        &self,
        http: &dyn Http,
        key: SecretMaterial,
        state: &Value,
        questions: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, f64>, DecisionError> {
        self.ask_with_headers(
            http,
            vec![bearer_header(key.expose_secret_bytes())],
            state,
            questions,
        )
        .await
    }
    /// Explicit gateway authentication; credentials are never retained by the client.
    pub async fn ask_with_headers(
        &self,
        http: &dyn Http,
        headers: Vec<ego_core::permit::HttpHeader>,
        state: &Value,
        questions: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, f64>, DecisionError> {
        self.ask_measured(http, headers, state, questions)
            .await
            .map(|reply| reply.answers)
    }
    /// Retain measured cost without retaining provider bodies or judged state.
    pub async fn ask_measured(
        &self,
        http: &dyn Http,
        mut headers: Vec<ego_core::permit::HttpHeader>,
        state: &Value,
        questions: &BTreeMap<String, String>,
    ) -> Result<DecisionResponse, DecisionError> {
        if questions.is_empty()
            || questions
                .iter()
                .any(|(id, q)| id.is_empty() || q.trim().is_empty())
        {
            return Err(DecisionError::InvalidRequest);
        }
        let questions_wire: BTreeMap<_, _> = questions
            .iter()
            .map(|(id, instructions)| (id, json!({"type":"noul","instructions":instructions})))
            .collect();
        headers.push(public_header("content-type", b"application/json".to_vec()));
        let body = serde_json::to_vec(&json!({
            "model":self.model, "state":state, "questions":questions_wire,
            "provider":{"zdr":true,"data_collection":"deny"}
        }))
        .map_err(|_| DecisionError::InvalidRequest)?;
        let request = HttpRequest {
            url: self.endpoint.clone(),
            method: Method::Post,
            headers,
            body: Some(body),
        };
        tokio::time::timeout(self.timeout, async {
            let mut response = self
                .profile
                .execute(http, request)
                .await
                .map_err(|_| DecisionError::Transport)?;
            if !(200..300).contains(&response.status) {
                return Err(DecisionError::HttpStatus(response.status));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.body.next().await {
                bytes.extend_from_slice(&chunk.map_err(|_| DecisionError::Transport)?);
            }
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|_| DecisionError::InvalidResponse)?;
            if value.get("error").is_some() {
                return Err(DecisionError::InvalidResponse);
            }
            let answers = value
                .get("answers")
                .and_then(Value::as_object)
                .ok_or(DecisionError::InvalidResponse)?;
            if answers.len() != questions.len() {
                return Err(DecisionError::InvalidResponse);
            }
            let answers = questions
                .keys()
                .map(|id| {
                    let answer = answers.get(id).ok_or(DecisionError::InvalidResponse)?;
                    if answer.get("type").and_then(Value::as_str) != Some("noul") {
                        return Err(DecisionError::InvalidResponse);
                    }
                    let p = answer
                        .get("noul")
                        .and_then(Value::as_f64)
                        .ok_or(DecisionError::InvalidResponse)?;
                    if !p.is_finite() || !(0.0..=1.0).contains(&p) {
                        return Err(DecisionError::InvalidResponse);
                    }
                    Ok((id.clone(), p))
                })
                .collect::<Result<_, _>>()?;
            let cost_usd = value
                .get("usage")
                .and_then(|usage| usage.get("cost"))
                .filter(|cost| {
                    cost.as_f64()
                        .is_some_and(|cost| cost.is_finite() && cost >= 0.0)
                })
                .map(Value::to_string);
            Ok(DecisionResponse { answers, cost_usd })
        })
        .await
        .map_err(|_| DecisionError::Timeout)?
    }
}
