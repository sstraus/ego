//! Google Application Default Credentials: the access token Vertex AI reads.
//!
//! The ADC file is the operator's own Google credential, written by
//! `gcloud auth application-default login` (`authorized_user`). Ego ships no
//! Google client identity: the client is the one inside that file. Other ADC
//! types, service-account keys included, are refused by their type name.
//!
//! This module turns the file into one token request and the answer into a
//! token. It opens no file and reads no environment; the CLI owns both.
//!
//! Source, read 2026-11-05: Google's OAuth 2.0 refresh grant
//! (<https://developers.google.com/identity/protocols/oauth2/web-server#offline>).

use std::fmt;
use std::sync::OnceLock;

use futures::StreamExt;
use serde::Deserialize;

use crate::http::{AdmittedHttpProfile, Http, HttpRequest};
use crate::oauth::OAuthToken;

/// Google's token endpoint, the only one this module sends a credential to.
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// One parsed `authorized_user` ADC file. The formatter prints no field.
pub struct Adc {
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

impl fmt::Debug for Adc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Adc")
            .field("type", &"authorized_user")
            .finish()
    }
}

/// Why no access token came out of the file or the exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdcError {
    /// The file is not an ADC JSON document.
    Malformed(String),
    /// A credential type this build does not mint tokens for.
    Unsupported(String),
    /// Google answered with a refusal.
    Refused { status: u16, message: String },
    /// The request did not complete.
    Transport(String),
}

impl fmt::Display for AdcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(message) => {
                write!(
                    f,
                    "the Application Default Credentials file is invalid: {message}"
                )
            }
            Self::Unsupported(kind) => write!(
                f,
                "Application Default Credentials of type `{kind}` are not supported; \
                 run `gcloud auth application-default login`"
            ),
            Self::Refused { status, message } => {
                write!(f, "Google refused the token request ({status}): {message}")
            }
            Self::Transport(message) => write!(f, "the Google token request failed: {message}"),
        }
    }
}

impl std::error::Error for AdcError {}

#[derive(Deserialize)]
struct File {
    #[serde(rename = "type")]
    kind: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
}

fn field(value: Option<String>, name: &str) -> Result<String, AdcError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AdcError::Malformed(format!("`{name}` is missing")))
}

/// Parse an ADC file.
pub fn parse(bytes: &[u8]) -> Result<Adc, AdcError> {
    // The serde error text never quotes a value, so it cannot carry a secret.
    let file: File =
        serde_json::from_slice(bytes).map_err(|error| AdcError::Malformed(error.to_string()))?;
    if file.kind != "authorized_user" {
        return Err(AdcError::Unsupported(file.kind));
    }
    Ok(Adc {
        client_id: field(file.client_id, "client_id")?,
        client_secret: field(file.client_secret, "client_secret")?,
        refresh_token: field(file.refresh_token, "refresh_token")?,
    })
}

impl Adc {
    /// The refresh-grant request for this credential.
    pub fn token_request(&self) -> Result<HttpRequest, AdcError> {
        crate::oauth::form_request(
            TOKEN_URL,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("refresh_token", &self.refresh_token),
            ],
        )
        .map_err(|error| AdcError::Malformed(error.to_string()))
    }
}

fn profile() -> &'static AdmittedHttpProfile {
    static PROFILE: OnceLock<AdmittedHttpProfile> = OnceLock::new();
    PROFILE
        .get_or_init(|| crate::oauth::oauth_profile("vertex", &["https://oauth2.googleapis.com/"]))
}

/// Send one token request and read the answer.
pub async fn exchange(
    http: &dyn Http,
    request: HttpRequest,
    now_ms: u64,
) -> Result<OAuthToken, AdcError> {
    let response = profile()
        .execute(http, request)
        .await
        .map_err(|error| AdcError::Transport(error.to_string()))?;
    let status = response.status;
    let mut body = Vec::new();
    let chunks: Vec<_> = response.body.collect().await;
    for chunk in chunks {
        body.extend_from_slice(&chunk.map_err(|error| AdcError::Transport(error.to_string()))?);
    }
    read_token(status, &body, now_ms)
}

#[derive(Deserialize)]
struct TokenBody {
    access_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

/// The token in Google's answer, with its expiry in Unix milliseconds.
///
/// The refresh grant returns no new refresh token: the next token is minted
/// from the ADC file again, so the stored entry carries no refresh secret.
pub fn read_token(status: u16, body: &[u8], now_ms: u64) -> Result<OAuthToken, AdcError> {
    let parsed: Option<TokenBody> = serde_json::from_slice(body).ok();
    if status != 200 {
        // `error` and `error_description` are Google's diagnostic codes and
        // sentences; nothing else of the body is relayed.
        let message = parsed
            .map(|body| {
                [body.error, body.error_description]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(": ")
            })
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| "no error description".to_string());
        return Err(AdcError::Refused { status, message });
    }
    let parsed =
        parsed.ok_or_else(|| AdcError::Malformed("the token answer is not JSON".into()))?;
    let access = parsed
        .access_token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| AdcError::Malformed("the token answer has no `access_token`".into()))?;
    let lifetime = parsed
        .expires_in
        .ok_or_else(|| AdcError::Malformed("the token answer has no `expires_in`".into()))?;
    Ok(OAuthToken::OAuth {
        access,
        refresh: None,
        expires: Some(now_ms.saturating_add(lifetime.saturating_mul(1000))),
    })
}
