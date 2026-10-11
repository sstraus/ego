//! Built-in OAuth protocol contracts for subscription-backed providers.
//!
//! This module owns vendor request shapes and response classification. It
//! owns no browser, TTY, clock, sleep or credential file; `ego-cli` supplies
//! those process concerns and executes these requests through the ordinary
//! HTTP seam.

use std::fmt;
use std::sync::OnceLock;

use base64::Engine;
use futures::StreamExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::http::{AdmittedHttpProfile, Http, HttpRequest, Method, bearer_header, public_header};
use ego_core::config::is_forbidden_auth_header;
use ego_core::permit::HttpHeader;

const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ANTHROPIC_CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const COPILOT_CLIENT: &str = "Iv1.b507a08c87ecfe98";
const XAI_CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const KIMI_CLIENT: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const MINIMAX_CLIENT: &str = "78257093-7e40-4613-99e0-527b14b39113";
// Nous publishes the Hermes CLI id for its portal and no ego-specific client
// registration path. This is the one declared foreign client identity.
const NOUS_CLIENT: &str = "hermes-cli";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const USER_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:user_code";

/// The only client identity ego ever sends.
///
/// One constant rather than a literal per request, because the rule it carries
/// is a single rule: ego names ego, on every flow and every provider, and a
/// vendor's own client string is an operator's `[auth.<provider>.headers]`
/// decision (`ARCHITECTURE.md` §7.8). A second spelling here is
/// how the first exception gets in.
pub const EGO_USER_AGENT: &str = concat!("ego/", env!("CARGO_PKG_VERSION"));

/// The built-in flow table. The enum is closed deliberately: adding a row is
/// adding a client identity ego ships, not registering an arbitrary protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltInFlow {
    OpenAiCodex,
    Anthropic,
    GitHubCopilot,
    Xai,
    KimiCoding,
    MiniMax,
    Nous,
    OpenRouter,
}

const FLOWS: &[BuiltInFlow] = &[
    BuiltInFlow::OpenAiCodex,
    BuiltInFlow::Anthropic,
    BuiltInFlow::GitHubCopilot,
    BuiltInFlow::Xai,
    BuiltInFlow::KimiCoding,
    BuiltInFlow::MiniMax,
    BuiltInFlow::Nous,
    BuiltInFlow::OpenRouter,
];

impl BuiltInFlow {
    pub fn all() -> &'static [Self] {
        FLOWS
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAiCodex => "openai-codex",
            Self::Anthropic => "anthropic",
            Self::GitHubCopilot => "github-copilot",
            Self::Xai => "xai-oauth",
            Self::KimiCoding => "kimi-coding",
            Self::MiniMax => "minimax-oauth",
            Self::Nous => "nous",
            Self::OpenRouter => "openrouter",
        }
    }

    fn http_profile(self) -> &'static AdmittedHttpProfile {
        static PROFILES: OnceLock<[AdmittedHttpProfile; 8]> = OnceLock::new();
        &PROFILES.get_or_init(|| {
            [
                oauth_profile("openai-codex", &["https://auth.openai.com/"]),
                oauth_profile("anthropic", &["https://platform.claude.com/"]),
                oauth_profile(
                    "github-copilot",
                    &["https://github.com/", "https://api.github.com/"],
                ),
                oauth_profile("xai-oauth", &["https://auth.x.ai/"]),
                oauth_profile("kimi-coding", &["https://auth.kimi.com/"]),
                oauth_profile("minimax-oauth", &["https://api.minimax.io/"]),
                oauth_profile("nous", &["https://portal.nousresearch.com/"]),
                oauth_profile("openrouter", &["https://openrouter.ai/"]),
            ]
        })[match self {
            Self::OpenAiCodex => 0,
            Self::Anthropic => 1,
            Self::GitHubCopilot => 2,
            Self::Xai => 3,
            Self::KimiCoding => 4,
            Self::MiniMax => 5,
            Self::Nous => 6,
            Self::OpenRouter => 7,
        }]
    }

    fn client_id(self) -> Option<&'static str> {
        match self {
            Self::OpenAiCodex => Some(CODEX_CLIENT),
            Self::Anthropic => Some(ANTHROPIC_CLIENT),
            Self::GitHubCopilot => Some(COPILOT_CLIENT),
            Self::Xai => Some(XAI_CLIENT),
            Self::KimiCoding => Some(KIMI_CLIENT),
            Self::MiniMax => Some(MINIMAX_CLIENT),
            Self::Nous => Some(NOUS_CLIENT),
            Self::OpenRouter => None,
        }
    }

    fn token_url(self) -> &'static str {
        match self {
            Self::OpenAiCodex => "https://auth.openai.com/oauth/token",
            Self::Anthropic => "https://platform.claude.com/v1/oauth/token",
            Self::GitHubCopilot => "https://github.com/login/oauth/access_token",
            Self::Xai => "https://auth.x.ai/oauth2/token",
            Self::KimiCoding => "https://auth.kimi.com/api/oauth/token",
            Self::MiniMax => "https://api.minimax.io/oauth/token",
            Self::Nous => "https://portal.nousresearch.com/api/oauth/token",
            Self::OpenRouter => "https://openrouter.ai/api/v1/auth/keys",
        }
    }

    pub fn device_authorization_request(self) -> Result<HttpRequest, FlowError> {
        match self {
            Self::OpenAiCodex => json_request(
                "https://auth.openai.com/api/accounts/deviceauth/usercode",
                serde_json::json!({ "client_id": CODEX_CLIENT }),
            ),
            Self::GitHubCopilot => form_request(
                "https://github.com/login/device/code",
                &[("client_id", COPILOT_CLIENT), ("scope", "read:user")],
            ),
            Self::Xai => form_request(
                "https://auth.x.ai/oauth2/device/code",
                &[
                    ("client_id", XAI_CLIENT),
                    (
                        "scope",
                        "openid profile email offline_access grok-cli:access api:access",
                    ),
                    ("referrer", "ego"),
                ],
            ),
            Self::KimiCoding => form_request(
                "https://auth.kimi.com/api/oauth/device_authorization",
                &[("client_id", KIMI_CLIENT)],
            ),
            Self::Nous => form_request(
                "https://portal.nousresearch.com/api/oauth/device/code",
                &[("client_id", NOUS_CLIENT), ("scope", "inference:invoke")],
            ),
            _ => Err(FlowError::UnsupportedGrant {
                provider: self.id().to_string(),
                grant: "device code".to_string(),
            }),
        }
    }

    pub fn authorization_url(
        self,
        redirect_uri: &str,
        challenge: &str,
        state: &str,
    ) -> Result<Url, FlowError> {
        let (base, fields): (&str, Vec<(&str, &str)>) = match self {
            Self::OpenAiCodex => (
                "https://auth.openai.com/oauth/authorize",
                vec![
                    ("response_type", "code"),
                    ("client_id", CODEX_CLIENT),
                    ("redirect_uri", redirect_uri),
                    ("scope", "openid profile email offline_access"),
                    ("code_challenge", challenge),
                    ("code_challenge_method", "S256"),
                    ("state", state),
                    ("id_token_add_organizations", "true"),
                    ("codex_cli_simplified_flow", "true"),
                    ("originator", "ego"),
                ],
            ),
            Self::Anthropic => (
                "https://claude.ai/oauth/authorize",
                vec![
                    ("code", "true"),
                    ("client_id", ANTHROPIC_CLIENT),
                    ("response_type", "code"),
                    ("redirect_uri", redirect_uri),
                    (
                        "scope",
                        "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
                    ),
                    ("code_challenge", challenge),
                    ("code_challenge_method", "S256"),
                    ("state", state),
                ],
            ),
            Self::OpenRouter => (
                "https://openrouter.ai/auth",
                vec![
                    ("callback_url", redirect_uri),
                    ("code_challenge", challenge),
                    ("code_challenge_method", "S256"),
                ],
            ),
            _ => {
                return Err(FlowError::UnsupportedGrant {
                    provider: self.id().to_string(),
                    grant: "browser PKCE".to_string(),
                });
            }
        };
        let mut url = parse(base)?;
        url.query_pairs_mut().extend_pairs(fields);
        Ok(url)
    }

    pub fn minimax_user_code_request(
        self,
        challenge: &str,
        state: &str,
    ) -> Result<HttpRequest, FlowError> {
        if self != Self::MiniMax {
            return Err(FlowError::UnsupportedGrant {
                provider: self.id().to_string(),
                grant: "MiniMax user code".to_string(),
            });
        }
        form_request(
            "https://api.minimax.io/oauth/code",
            &[
                ("response_type", "code"),
                ("client_id", MINIMAX_CLIENT),
                ("scope", "group_id profile model.completion"),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
                ("state", state),
            ],
        )
    }

    pub fn read_minimax_user_code(
        self,
        response: TokenStatus,
        expected_state: &str,
    ) -> Result<MiniMaxUserCode, FlowError> {
        if self != Self::MiniMax {
            return Err(FlowError::UnsupportedGrant {
                provider: self.id().to_string(),
                grant: "MiniMax user code".to_string(),
            });
        }
        if !(200..300).contains(&response.status) {
            return self.response_error(response);
        }
        let body: Value =
            serde_json::from_slice(&response.body).map_err(|err| FlowError::InvalidResponse {
                provider: self.id().to_string(),
                message: err.to_string(),
            })?;
        if body.get("state").and_then(Value::as_str) != Some(expected_state) {
            return Err(FlowError::InvalidResponse {
                provider: self.id().to_string(),
                message: "state mismatch".to_string(),
            });
        }
        Ok(MiniMaxUserCode {
            user_code: required(&body, "user_code", self)?,
            verification_uri: required(&body, "verification_uri", self)?,
            expires_in: body
                .get("expired_in")
                .and_then(Value::as_u64)
                .ok_or_else(|| missing_field(self, "expired_in"))?,
            interval_ms: body.get("interval").and_then(Value::as_u64).unwrap_or(2000),
        })
    }

    pub fn device_poll_request(
        self,
        device_code: &str,
        user_code: Option<&str>,
    ) -> Result<HttpRequest, FlowError> {
        if self == Self::OpenAiCodex {
            return json_request(
                "https://auth.openai.com/api/accounts/deviceauth/token",
                serde_json::json!({
                    "device_auth_id": device_code,
                    "user_code": user_code.ok_or_else(|| FlowError::InvalidResponse {
                        provider: self.id().to_string(),
                        message: "device poll needs user_code".to_string(),
                    })?,
                }),
            );
        }
        form_request(
            self.token_url(),
            &[
                ("grant_type", DEVICE_GRANT),
                (
                    "client_id",
                    self.client_id()
                        .ok_or_else(|| FlowError::UnsupportedGrant {
                            provider: self.id().to_string(),
                            grant: "device code".to_string(),
                        })?,
                ),
                ("device_code", device_code),
            ],
        )
    }

    pub fn login_request(self, grant: LoginGrant<'_>) -> Result<HttpRequest, FlowError> {
        match (self, grant) {
            (Self::OpenRouter, LoginGrant::AuthorizationCode { code, verifier, .. }) => {
                json_request(
                    self.token_url(),
                    serde_json::json!({
                        "code": code,
                        "code_verifier": verifier,
                        "code_challenge_method": "S256",
                    }),
                )
            }
            (
                Self::Anthropic,
                LoginGrant::AuthorizationCode {
                    code,
                    verifier,
                    redirect_uri,
                    state,
                },
            ) => json_request(
                self.token_url(),
                serde_json::json!({
                    "grant_type": "authorization_code",
                    "client_id": ANTHROPIC_CLIENT,
                    "code": code,
                    "state": state.unwrap_or(verifier),
                    "redirect_uri": redirect_uri,
                    "code_verifier": verifier,
                }),
            ),
            (
                Self::MiniMax,
                LoginGrant::UserCode {
                    user_code,
                    verifier,
                },
            ) => form_request(
                self.token_url(),
                &[
                    ("grant_type", USER_CODE_GRANT),
                    ("client_id", MINIMAX_CLIENT),
                    ("user_code", user_code),
                    ("code_verifier", verifier),
                ],
            ),
            (
                flow,
                LoginGrant::AuthorizationCode {
                    code,
                    verifier,
                    redirect_uri,
                    ..
                },
            ) if flow == Self::OpenAiCodex => form_request(
                flow.token_url(),
                &[
                    ("grant_type", "authorization_code"),
                    (
                        "client_id",
                        flow.client_id().expect("these flows have clients"),
                    ),
                    ("code", code),
                    ("code_verifier", verifier),
                    ("redirect_uri", redirect_uri),
                ],
            ),
            (flow, grant) => Err(FlowError::UnsupportedGrant {
                provider: flow.id().to_string(),
                grant: grant.name().to_string(),
            }),
        }
    }

    pub fn refresh_request(self, refresh_token: &str) -> Result<HttpRequest, FlowError> {
        if matches!(self, Self::OpenRouter | Self::GitHubCopilot) {
            return Err(FlowError::UnsupportedGrant {
                provider: self.id().to_string(),
                grant: "refresh token".to_string(),
            });
        }
        let fields = [
            ("grant_type", "refresh_token"),
            (
                "client_id",
                self.client_id().expect("rotating flows have clients"),
            ),
            ("refresh_token", refresh_token),
        ];
        if self == Self::Anthropic {
            json_request(
                self.token_url(),
                serde_json::json!({
                    "grant_type": "refresh_token",
                    "client_id": ANTHROPIC_CLIENT,
                    "refresh_token": refresh_token,
                }),
            )
        } else {
            form_request(self.token_url(), &fields)
        }
    }

    pub fn copilot_exchange_request(self, github_token: &str) -> Result<HttpRequest, FlowError> {
        if self != Self::GitHubCopilot {
            return Err(FlowError::UnsupportedGrant {
                provider: self.id().to_string(),
                grant: "Copilot token exchange".to_string(),
            });
        }
        // The four Copilot Chat identity headers Pi freezes here — the
        // extension's `user-agent`, its editor and plugin versions, and the
        // `vscode-chat` integration id — are gone: ego is not VS Code and does
        // not say it is. What replaces them is ego's own name and nothing else.
        // GitHub may gate this exchange on a client it recognises; that is a
        // measurement to report, not a reason to send Microsoft's.
        Ok(HttpRequest {
            url: parse("https://api.github.com/copilot_internal/v2/token")?,
            method: Method::Get,
            headers: vec![
                public_header("accept", b"application/json".to_vec()),
                bearer_header(github_token.as_bytes()),
                public_header("user-agent", EGO_USER_AGENT.as_bytes().to_vec()),
            ],
            body: None,
        })
    }

    pub fn read_token(
        self,
        response: TokenStatus,
        now_ms: u64,
        previous_refresh: Option<&str>,
    ) -> Result<OAuthToken, FlowError> {
        if let Some(refusal) = self.classify(&response) {
            return Err(refusal);
        }
        let body: Value =
            serde_json::from_slice(&response.body).map_err(|err| FlowError::InvalidResponse {
                provider: self.id().to_string(),
                message: err.to_string(),
            })?;
        if self == Self::OpenRouter {
            let key = required(&body, "key", self)?;
            return Ok(OAuthToken::ApiKey { key });
        }
        if self == Self::GitHubCopilot && body.get("token").is_some() {
            let access = required(&body, "token", self)?;
            let expires = body
                .get("expires_at")
                .and_then(Value::as_u64)
                .ok_or_else(|| missing_field(self, "expires_at"))?
                .saturating_mul(1000);
            return Ok(OAuthToken::OAuth {
                access,
                refresh: previous_refresh.map(str::to_string),
                expires: Some(expires),
            });
        }
        let access = required(&body, "access_token", self)?;
        let refresh = body
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| previous_refresh.map(str::to_string));
        if refresh.is_none() {
            return Err(missing_field(self, "refresh_token"));
        }
        let expiry_value = body
            .get("expires_in")
            .or_else(|| body.get("expired_in"))
            .and_then(Value::as_u64);
        let expires = expiry_value.map(|raw| {
            if self == Self::MiniMax && raw > now_ms / 2 {
                raw
            } else {
                now_ms.saturating_add(raw.saturating_mul(1000))
            }
        });
        Ok(OAuthToken::OAuth {
            access,
            refresh,
            expires,
        })
    }

    pub fn read_device_authorization(
        self,
        response: TokenStatus,
    ) -> Result<DeviceAuthorization, FlowError> {
        if !(200..300).contains(&response.status) {
            return self.response_error(response);
        }
        let body: Value =
            serde_json::from_slice(&response.body).map_err(|err| FlowError::InvalidResponse {
                provider: self.id().to_string(),
                message: err.to_string(),
            })?;
        let device_code = if self == Self::OpenAiCodex {
            required(&body, "device_auth_id", self)?
        } else {
            required(&body, "device_code", self)?
        };
        let verification_uri = if self == Self::OpenAiCodex {
            body.get("verification_uri")
                .and_then(Value::as_str)
                .unwrap_or("https://auth.openai.com/codex/device")
                .to_string()
        } else {
            required(&body, "verification_uri", self)?
        };
        Ok(DeviceAuthorization {
            device_code,
            user_code: required(&body, "user_code", self)?,
            verification_uri,
            interval_seconds: body.get("interval").and_then(Value::as_u64).unwrap_or(5),
            expires_in: body
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(900),
        })
    }

    pub fn read_poll(
        self,
        response: TokenStatus,
        now_ms: u64,
        previous_refresh: Option<&str>,
    ) -> Result<PollOutcome, FlowError> {
        let value: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        let error = value.get("error").and_then(Value::as_str);
        if response.status == 429
            || matches!(
                error,
                Some(
                    "invalid_grant"
                        | "invalid_token"
                        | "refresh_token_reused"
                        | "invalid_refresh_token"
                )
            )
        {
            return self
                .read_token(response, now_ms, previous_refresh)
                .map(PollOutcome::Token);
        }
        if error == Some("authorization_pending")
            || error == Some("deviceauth_authorization_pending")
            || (self == Self::OpenAiCodex && matches!(response.status, 403 | 404))
        {
            return Ok(PollOutcome::Pending);
        }
        if error == Some("slow_down") {
            return Ok(PollOutcome::SlowDown);
        }
        if self == Self::MiniMax && value.get("status").and_then(Value::as_str) != Some("success") {
            return Ok(PollOutcome::Pending);
        }
        if self == Self::OpenAiCodex && response.status == 200 {
            return Ok(PollOutcome::CodexAuthorization {
                code: required(&value, "authorization_code", self)?,
                verifier: required(&value, "code_verifier", self)?,
            });
        }
        if self == Self::GitHubCopilot && response.status == 200 {
            return Ok(PollOutcome::GitHubToken(required(
                &value,
                "access_token",
                self,
            )?));
        }
        self.read_token(response, now_ms, previous_refresh)
            .map(PollOutcome::Token)
    }

    /// The one place a token-endpoint body is read, and the only place it may
    /// be looked at at all.
    ///
    /// `read_token` and `read_device_authorization` used to classify
    /// separately and to disagree — the first recognised four
    /// grant-invalidation markers and the second only `invalid_grant`, so the
    /// same rotated refresh token was a relogin on one path and a protocol
    /// failure on the other. One door removes that, and it is also what makes
    /// the scrub enforceable: the bytes exist in this function and nowhere
    /// else, going to the log rather than into the returned error.
    ///
    /// `None` means the answer is a 2xx that carries no invalidation marker,
    /// which is the only case a caller may go on to parse.
    fn classify(self, response: &TokenStatus) -> Option<FlowError> {
        let body_text = String::from_utf8_lossy(&response.body);
        let invalidated = [
            "invalid_grant",
            "invalid_token",
            "refresh_token_reused",
            "invalid_refresh_token",
        ]
        .iter()
        .any(|marker| body_text.contains(marker));
        let refusal = if response.status == 429 {
            FlowError::Temporary {
                provider: self.id().to_string(),
                status: response.status,
                cause: TokenRefusal::RateLimited,
            }
        } else if response.status == 401 {
            FlowError::ReloginRequired {
                provider: self.id().to_string(),
                cause: TokenRefusal::CredentialRejected,
            }
        } else if invalidated {
            FlowError::ReloginRequired {
                provider: self.id().to_string(),
                cause: TokenRefusal::GrantInvalidated,
            }
        } else if !(200..300).contains(&response.status) {
            FlowError::Protocol {
                provider: self.id().to_string(),
                status: response.status,
                cause: TokenRefusal::Unanticipated,
            }
        } else {
            return None;
        };
        // The whole answer, for the operator debugging a real outage. `warn`
        // rather than `error` because a rate limit is routine, and the body is
        // a field rather than part of the message so a structured subscriber
        // can drop it without losing the classification beside it. A login
        // answer is the likeliest of all of them to quote a credential back,
        // so it carries the `ego::body` target that keeps it out of every
        // filter an operator did not write by hand — see `http::refusal`.
        tracing::warn!(
            target: "ego::body",
            provider = self.id(),
            status = response.status,
            body = %body_text,
            "a login request was refused"
        );
        Some(refusal)
    }

    fn response_error<T>(self, response: TokenStatus) -> Result<T, FlowError> {
        Err(self.classify(&response).unwrap_or(FlowError::Protocol {
            provider: self.id().to_string(),
            status: response.status,
            cause: TokenRefusal::Unanticipated,
        }))
    }
}

fn oauth_profile(id: &str, routes: &[&str]) -> AdmittedHttpProfile {
    let routes = routes
        .iter()
        .map(|route| Url::parse(route).expect("a built-in OAuth route is a URL"))
        .collect::<Vec<_>>();
    AdmittedHttpProfile::from_urls(&format!("aux/oauth/{id}/v1"), &routes)
        .expect("built-in OAuth routes form an admitted profile")
}

pub fn built_in(id: &str) -> Option<BuiltInFlow> {
    FLOWS.iter().copied().find(|flow| flow.id() == id)
}

#[derive(Debug, Clone)]
pub enum LoginGrant<'a> {
    AuthorizationCode {
        code: &'a str,
        verifier: &'a str,
        redirect_uri: &'a str,
        state: Option<&'a str>,
    },
    UserCode {
        user_code: &'a str,
        verifier: &'a str,
    },
}

impl LoginGrant<'_> {
    fn name(&self) -> &'static str {
        match self {
            Self::AuthorizationCode { .. } => "authorization code",
            Self::UserCode { .. } => "user code",
        }
    }
}

pub struct TokenStatus {
    status: u16,
    body: Vec<u8>,
}

impl TokenStatus {
    pub fn new(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            body: body.to_vec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval_seconds: u64,
    pub expires_in: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiniMaxUserCode {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval_ms: u64,
}

pub enum PollOutcome {
    Pending,
    SlowDown,
    CodexAuthorization { code: String, verifier: String },
    GitHubToken(String),
    Token(OAuthToken),
}

impl PollOutcome {
    /// What kind of answer this is, carrying none of it.
    ///
    /// Three of the five variants hold credential material — an authorization
    /// code, a PKCE verifier, a bearer token — so a caller reporting "the
    /// server said something I did not expect" needs a way to say *which*
    /// something without quoting it. This is that way, and
    /// `ego-cli`'s device-code login is the caller that needed it.
    pub fn kind(&self) -> &'static str {
        match self {
            PollOutcome::Pending => "still pending",
            PollOutcome::SlowDown => "a slow-down instruction",
            PollOutcome::CodexAuthorization { .. } => "a Codex authorization grant",
            PollOutcome::GitHubToken(_) => "a GitHub token",
            PollOutcome::Token(_) => "a token",
        }
    }
}

/// Hand-written, and the derive is deliberately gone.
///
/// `#[derive(Debug)]` rendered the authorization code, the PKCE verifier and
/// the GitHub token in full, and `ego-cli`'s device-code login interpolated a
/// whole `PollOutcome` into an operator-visible error with `{other:?}` — so an
/// unexpected answer printed the secret it was complaining about. Naming the
/// variant and dropping the payload makes that impossible to reintroduce at any
/// call site, present or future, rather than only at the one that had it.
impl fmt::Debug for PollOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PollOutcome({})", self.kind())
    }
}

pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Execute one OAuth request through the injected transport and buffer only
/// the small token response. Generation bodies remain streamed elsewhere.
/// The operator's `[auth.<provider>.headers]` table, applied to login.
///
/// The same declared table the generation path applies, reaching the requests
/// that establish a credential rather than only the ones that spend it.
/// Without it, removing a vendor disguise from a login exchange leaves an
/// operator whose vendor gates on a client it recognises with no way back —
/// and putting the disguise back is not one.
///
/// Empty by default, because the shipped identity is ego's own and an operator
/// who declares nothing has declared exactly that.
#[derive(Debug, Clone, Default)]
pub struct LoginHeaders(std::collections::HashMap<String, String>);

impl LoginHeaders {
    pub fn new(headers: std::collections::HashMap<String, String>) -> Self {
        Self(headers)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Overlay the declared table onto one login request.
    ///
    /// A declared name replaces the built-in of the same name, which is what
    /// makes the table an escape hatch rather than an append-only annotation.
    /// The refusal is the exception and it is deliberate: config already
    /// refuses these names at deserialization, so a table carrying one here is
    /// a bug in a caller, and this is the one request family where honouring
    /// it would let a declared string stand in for the credential itself.
    #[must_use]
    pub fn apply(&self, mut request: HttpRequest) -> HttpRequest {
        for (name, value) in &self.0 {
            if is_forbidden_auth_header(name) {
                continue;
            }
            let Ok(header) =
                HttpHeader::new(&name.to_ascii_lowercase(), value.clone().into_bytes())
            else {
                continue;
            };
            request
                .headers
                .retain(|existing| !existing.name().eq_ignore_ascii_case(name));
            request.headers.push(header);
        }
        request
    }
}

/// Run one login request through the injected transport.
///
/// `headers` is not optional on purpose. Every built-in login request goes
/// through this one function, so requiring the table here is what makes the
/// escape hatch reach all of them: a call site cannot forget what it cannot
/// omit, and the next vendor to gate a login needs no second hatch.
pub async fn execute(
    http: &dyn Http,
    flow: BuiltInFlow,
    request: HttpRequest,
    headers: &LoginHeaders,
) -> Result<TokenStatus, FlowError> {
    let response = flow
        .http_profile()
        .execute(http, headers.apply(request))
        .await
        .map_err(|err| FlowError::Temporary {
            provider: "OAuth transport".to_string(),
            status: 0,
            cause: TokenRefusal::Transport(err.to_string()),
        })?;
    let status = response.status;
    let chunks: Vec<_> = response.body.collect().await;
    let mut body = Vec::new();
    for chunk in chunks {
        body.extend_from_slice(&chunk.map_err(|err| FlowError::Temporary {
            provider: "OAuth transport".to_string(),
            status,
            cause: TokenRefusal::Transport(err.to_string()),
        })?);
    }
    Ok(TokenStatus { status, body })
}

/// A parsed credential. Formatters redact; `into_values` is the deliberate
/// transfer into the locked CLI store.
pub enum OAuthToken {
    OAuth {
        access: String,
        refresh: Option<String>,
        expires: Option<u64>,
    },
    ApiKey {
        key: String,
    },
}

impl fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OAuth {
                expires, refresh, ..
            } => f
                .debug_struct("OAuthToken")
                .field("access", &"<redacted>")
                .field("refresh", &refresh.as_ref().map(|_| "<redacted>"))
                .field("expires", expires)
                .finish(),
            Self::ApiKey { .. } => f
                .debug_struct("ApiKeyToken")
                .field("key", &"<redacted>")
                .finish(),
        }
    }
}

pub struct OAuthValues {
    pub access: String,
    pub refresh: Option<String>,
    pub expires: Option<u64>,
    pub api_key: bool,
}

impl fmt::Debug for OAuthValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthValues")
            .field("access", &"<redacted>")
            .field("refresh", &self.refresh.as_ref().map(|_| "<redacted>"))
            .field("expires", &self.expires)
            .field("api_key", &self.api_key)
            .finish()
    }
}

impl OAuthToken {
    pub fn into_values(self) -> OAuthValues {
        match self {
            Self::OAuth {
                access,
                refresh,
                expires,
            } => OAuthValues {
                access,
                refresh,
                expires,
                api_key: false,
            },
            Self::ApiKey { key } => OAuthValues {
                access: key,
                refresh: None,
                expires: None,
                api_key: true,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowError {
    UnsupportedGrant {
        provider: String,
        grant: String,
    },
    ReloginRequired {
        provider: String,
        cause: TokenRefusal,
    },
    Temporary {
        provider: String,
        status: u16,
        cause: TokenRefusal,
    },
    Protocol {
        provider: String,
        status: u16,
        cause: TokenRefusal,
    },
    InvalidResponse {
        provider: String,
        message: String,
    },
    MissingAccountId,
}

/// Why a login request failed, said in ego's vocabulary rather than the
/// endpoint's.
///
/// These variants carry no upstream bytes on purpose. A token request carries
/// the operator's refresh token and client credentials, and an identity
/// provider is free to echo them back in its own diagnostic — `invalid_grant:
/// refresh_token <token> was already used` is a real shape — while a proxy in
/// front of it may reflect request headers instead. Relaying that body into
/// `FlowError` put it on a terminal, in `AuthError`, in `RunTermination` and,
/// since the ACP surface landed, in `error.data` on a host's wire.
///
/// `RunTermination::after_turn` already collapses the *mid*-turn provider
/// failure to a fixed string for the same reason; this is that rule applied to
/// the pre-turn half. The body is not discarded — `BuiltInFlow::classify`
/// logs it at `warn` before returning, which is the destination this
/// repository already documents for upstream diagnostics.
///
/// The classification is a type rather than a sentence because callers branch
/// on it: `ego-cli`'s store deletes the entry for a relogin and keeps it for a
/// rate limit, and it must not have to match on prose to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenRefusal {
    /// No answer arrived. The text is ego's own `TransportError`, not
    /// anything an endpoint said, so it is safe to render.
    Transport(String),
    /// HTTP 429.
    RateLimited,
    /// HTTP 401: the endpoint rejected the credential presented.
    CredentialRejected,
    /// The body carried one of the OAuth grant-invalidation markers
    /// (`invalid_grant`, `invalid_token`, `refresh_token_reused`,
    /// `invalid_refresh_token`), which is a relogin whatever the status was.
    GrantInvalidated,
    /// A non-2xx answer matching none of the above.
    Unanticipated,
}

impl fmt::Display for TokenRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(detail) => write!(f, "the transport failed: {detail}"),
            Self::RateLimited => f.write_str("it rate-limited the request"),
            Self::CredentialRejected => f.write_str("it rejected the credential presented"),
            Self::GrantInvalidated => f.write_str("it reported the grant no longer valid"),
            Self::Unanticipated => f.write_str(
                "it refused, in terms ego does not classify; see the log for its answer",
            ),
        }
    }
}

impl fmt::Display for FlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedGrant { provider, grant } => {
                write!(f, "{provider} does not support {grant}")
            }
            Self::ReloginRequired { provider, cause } => write!(
                f,
                "{provider} login expired; run `ego auth login {provider}`: {cause}"
            ),
            Self::Temporary {
                provider,
                status,
                cause,
            } => write!(
                f,
                "{provider} token endpoint temporarily failed ({status}): {cause}"
            ),
            Self::Protocol {
                provider,
                status,
                cause,
            } => write!(f, "{provider} token endpoint failed ({status}): {cause}"),
            Self::InvalidResponse { provider, message } => write!(
                f,
                "{provider} returned an invalid token response: {message}"
            ),
            Self::MissingAccountId => {
                f.write_str("OpenAI Codex access token has no chatgpt_account_id claim")
            }
        }
    }
}

impl std::error::Error for FlowError {}

pub enum ProviderAuth {
    Headers(Vec<ego_core::permit::HttpHeader>),
}

impl fmt::Debug for ProviderAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Headers(headers) => f
                .debug_tuple("Headers")
                .field(
                    &headers
                        .iter()
                        .map(|header| header.name())
                        .collect::<Vec<_>>(),
                )
                .finish(),
        }
    }
}

impl ProviderAuth {
    pub fn headers(self) -> Vec<ego_core::permit::HttpHeader> {
        match self {
            Self::Headers(headers) => headers,
        }
    }
}

pub fn provider_auth(provider: &str, credential: &str) -> Result<ProviderAuth, FlowError> {
    if provider == "openai-codex" {
        return Ok(ProviderAuth::Headers(vec![
            bearer_header(credential.as_bytes()),
            ego_core::permit::HttpHeader::secret(
                "chatgpt-account-id",
                codex_account_id(credential)?.into(),
            )
            .expect("the account header name is valid"),
            public_header("openai-beta", b"responses=experimental".to_vec()),
        ]));
    }
    if provider == "anthropic" {
        if credential.starts_with("sk-ant-oat") {
            // The prefix is the whole dispatch: an oat credential 401s as
            // `x-api-key` and is presented as a Bearer instead, and
            // `oauth-2025-04-20` is the beta that makes Messages accept a
            // Bearer at all. That pair is the wire contract for this token
            // type. Pi's `user-agent: claude-cli/…`, `x-app: cli` and
            // `claude-code-20250219` are not: they are Claude Code's identity,
            // which Anthropic's legal-and-compliance page reserves to its own
            // clients, and ego does not wear it.
            return Ok(ProviderAuth::Headers(vec![
                bearer_header(credential.as_bytes()),
                public_header("anthropic-beta", b"oauth-2025-04-20".to_vec()),
            ]));
        }
        return Ok(ProviderAuth::Headers(vec![
            ego_core::permit::HttpHeader::secret("x-api-key", credential.into())
                .expect("the API key header name is valid"),
        ]));
    }
    Ok(ProviderAuth::Headers(vec![bearer_header(
        credential.as_bytes(),
    )]))
}

/// The account-specific API address carried by a credential, if any.
pub fn provider_base(provider: &str, credential: &str) -> Result<Option<Url>, FlowError> {
    if provider != "github-copilot" {
        return Ok(None);
    }
    let proxy = credential
        .split(';')
        .find_map(|part| part.strip_prefix("proxy-ep="))
        .ok_or_else(|| FlowError::InvalidResponse {
            provider: provider.to_string(),
            message: "Copilot token is missing `proxy-ep`".to_string(),
        })?;
    let suffix = proxy
        .strip_prefix("proxy.")
        .ok_or_else(|| FlowError::InvalidResponse {
            provider: provider.to_string(),
            message: "Copilot `proxy-ep` is not a proxy host".to_string(),
        })?;
    parse(&format!("https://api.{suffix}/")).map(Some)
}

pub fn codex_account_id(access_token: &str) -> Result<String, FlowError> {
    let payload = access_token
        .split('.')
        .nth(1)
        .ok_or(FlowError::MissingAccountId)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| FlowError::MissingAccountId)?;
    let value: Value = serde_json::from_slice(&decoded).map_err(|_| FlowError::MissingAccountId)?;
    value
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or(FlowError::MissingAccountId)
}

fn parse(raw: &str) -> Result<Url, FlowError> {
    Url::parse(raw).map_err(|err| FlowError::InvalidResponse {
        provider: "built-in OAuth table".to_string(),
        message: err.to_string(),
    })
}

fn form_request(url: &str, fields: &[(&str, &str)]) -> Result<HttpRequest, FlowError> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs(fields.iter().copied());
    Ok(HttpRequest {
        url: parse(url)?,
        method: Method::Post,
        headers: vec![
            public_header("accept", b"application/json".to_vec()),
            public_header(
                "content-type",
                b"application/x-www-form-urlencoded".to_vec(),
            ),
            public_header("user-agent", EGO_USER_AGENT.as_bytes().to_vec()),
        ],
        body: Some(serializer.finish().into_bytes()),
    })
}

fn json_request(url: &str, body: Value) -> Result<HttpRequest, FlowError> {
    Ok(HttpRequest {
        url: parse(url)?,
        method: Method::Post,
        headers: vec![
            public_header("accept", b"application/json".to_vec()),
            public_header("content-type", b"application/json".to_vec()),
            public_header("user-agent", EGO_USER_AGENT.as_bytes().to_vec()),
        ],
        body: Some(serde_json::to_vec(&body).expect("a JSON value serialises")),
    })
}

fn required(body: &Value, field: &str, flow: BuiltInFlow) -> Result<String, FlowError> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| missing_field(flow, field))
}

fn missing_field(flow: BuiltInFlow, field: &str) -> FlowError {
    FlowError::InvalidResponse {
        provider: flow.id().to_string(),
        message: format!("missing `{field}`"),
    }
}
