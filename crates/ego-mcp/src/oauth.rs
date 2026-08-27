use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ego_core::config::{McpHttpAuth, McpHttpConfig, McpServerName};
use ego_core::credentials::{CredentialKey, CredentialStore, CredentialTarget, SecretMaterial};
use ego_core::permit::HttpRequest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::http::{AdmittedMcpHttpProfile, header};
use crate::{McpError, McpFailure, McpFuture, McpHttpEffects, McpHttpPurpose, McpHttpResponse};

const TOKEN_BUNDLE_PREFIX: &[u8] = b"ego:mcp:token-bundle:v1\n";
const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;

pub struct McpClientMetadata {
    client_id: Url,
    client_name: String,
    redirect_uri: Url,
}

impl McpClientMetadata {
    pub fn new(
        client_id: Url,
        client_name: impl Into<String>,
        redirect_uri: Url,
    ) -> Result<Self, McpError> {
        let client_name = client_name.into();
        if client_id.scheme() != "https"
            || client_id.host_str().is_none()
            || client_id.path().is_empty()
            || client_id.path() == "/"
            || client_id.query().is_some()
            || client_id.fragment().is_some()
            || client_name.trim().is_empty()
        {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid CIMD client metadata",
            ));
        }
        validate_redirect_uri(&redirect_uri)?;
        Ok(Self {
            client_id,
            client_name,
            redirect_uri,
        })
    }

    pub fn client_id(&self) -> &Url {
        &self.client_id
    }

    pub fn client_name(&self) -> &str {
        &self.client_name
    }

    pub fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }
}

pub struct McpLoginRequest {
    server: McpServerName,
    config: McpHttpConfig,
    redirect_uri: Url,
    client_metadata: McpClientMetadata,
    deadline: Instant,
}

impl McpLoginRequest {
    pub fn new(
        server: McpServerName,
        config: McpHttpConfig,
        redirect_uri: Url,
        client_metadata: McpClientMetadata,
        deadline: Instant,
    ) -> Result<Self, McpError> {
        config
            .validate()
            .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
        if config.auth != McpHttpAuth::Oauth
            || redirect_uri != *client_metadata.redirect_uri()
            || deadline <= Instant::now()
        {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid MCP OAuth login request",
            ));
        }
        Ok(Self {
            server,
            config,
            redirect_uri,
            client_metadata,
            deadline,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpPkceMethod {
    S256,
}

pub struct McpPkceContext {
    challenge: String,
    method: McpPkceMethod,
}

impl McpPkceContext {
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    pub fn method(&self) -> McpPkceMethod {
        self.method
    }
}

pub struct McpAuthorizationInteraction {
    authorization_url: Url,
    redirect_uri: Url,
    state: String,
    pkce: McpPkceContext,
}

impl McpAuthorizationInteraction {
    pub fn authorization_url(&self) -> &Url {
        &self.authorization_url
    }

    pub fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn pkce(&self) -> &McpPkceContext {
        &self.pkce
    }
}

pub struct McpOAuthCallback {
    raw_callback_url: Url,
}

impl McpOAuthCallback {
    pub fn from_callback_url(url: Url) -> Result<Self, McpError> {
        if !url.has_host() || url.fragment().is_some() {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid OAuth callback URL",
            ));
        }
        Ok(Self {
            raw_callback_url: url,
        })
    }

    pub fn as_url(&self) -> &Url {
        &self.raw_callback_url
    }
}

pub trait McpOAuthInteraction: Send + Sync {
    fn authorize<'a>(
        &'a self,
        interaction: McpAuthorizationInteraction,
    ) -> McpFuture<'a, McpOAuthCallback>;
}

pub struct McpLoginResult {
    issuer: Url,
    resource: Url,
}

impl McpLoginResult {
    pub fn issuer(&self) -> &Url {
        &self.issuer
    }

    pub fn resource(&self) -> &Url {
        &self.resource
    }
}

pub struct McpCredentialBundle {
    access_token: SecretMaterial,
    refresh_token: Option<SecretMaterial>,
    token_type: String,
    scopes: Vec<String>,
}

impl std::fmt::Debug for McpCredentialBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCredentialBundle")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("token_type", &self.token_type)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl McpCredentialBundle {
    pub fn access_token(&self) -> &SecretMaterial {
        &self.access_token
    }

    pub fn refresh_token(&self) -> Option<&SecretMaterial> {
        self.refresh_token.as_ref()
    }

    pub fn token_type(&self) -> &str {
        &self.token_type
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn decode(material: SecretMaterial) -> Result<Self, McpError> {
        let bytes = material.expose_secret_bytes();
        let payload = bytes.strip_prefix(TOKEN_BUNDLE_PREFIX).ok_or_else(|| {
            McpError::new(
                McpFailure::Credential,
                "unsupported credential bundle version",
            )
        })?;
        let wire: CredentialBundleWire = serde_json::from_slice(payload)
            .map_err(|_| McpError::new(McpFailure::Credential, "corrupt credential bundle"))?;
        wire.validate()?;
        Ok(Self {
            access_token: SecretMaterial::new(wire.access_token.into_bytes()),
            refresh_token: wire
                .refresh_token
                .map(|token| SecretMaterial::new(token.into_bytes())),
            token_type: wire.token_type,
            scopes: wire.scopes,
        })
    }

    fn encode(&self) -> Result<SecretMaterial, McpError> {
        let wire = CredentialBundleWire {
            access_token: std::str::from_utf8(self.access_token.expose_secret_bytes())
                .map_err(|_| McpError::new(McpFailure::Credential, "invalid access token"))?
                .to_owned(),
            refresh_token: self
                .refresh_token
                .as_ref()
                .map(|token| std::str::from_utf8(token.expose_secret_bytes()).map(str::to_owned))
                .transpose()
                .map_err(|_| McpError::new(McpFailure::Credential, "invalid refresh token"))?,
            token_type: self.token_type.clone(),
            scopes: self.scopes.clone(),
        };
        let mut bytes = TOKEN_BUNDLE_PREFIX.to_vec();
        bytes.extend_from_slice(&serde_json::to_vec(&wire)?);
        Ok(SecretMaterial::new(bytes))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialBundleWire {
    access_token: String,
    refresh_token: Option<String>,
    token_type: String,
    scopes: Vec<String>,
}

impl CredentialBundleWire {
    fn validate(&self) -> Result<(), McpError> {
        if self.access_token.is_empty()
            || self.refresh_token.as_ref().is_some_and(String::is_empty)
            || self.token_type != "Bearer"
            || self
                .scopes
                .iter()
                .any(|scope| scope.is_empty() || scope.contains(' '))
        {
            return Err(McpError::new(
                McpFailure::Credential,
                "invalid credential bundle",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    resource: Url,
    authorization_servers: Vec<Url>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Deserialize)]
struct AuthorizationServerMetadata {
    authorization_endpoint: Url,
    token_endpoint: Url,
    issuer: Url,
    resource: Url,
    audience: Url,
    #[serde(default)]
    client_id_metadata_document_supported: bool,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    token_type: String,
    scope: String,
    sub: Option<String>,
}

pub struct McpTokenRefreshRequest {
    server: McpServerName,
    config: McpHttpConfig,
    client_metadata: McpClientMetadata,
    credential_key: CredentialKey,
    deadline: Instant,
}

impl McpTokenRefreshRequest {
    pub fn new(
        server: McpServerName,
        config: McpHttpConfig,
        client_metadata: McpClientMetadata,
        credential_key: CredentialKey,
        deadline: Instant,
    ) -> Result<Self, McpError> {
        config
            .validate()
            .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
        let configured_resource = normalized_origin(config.url.as_url())?.to_string();
        if config.auth != McpHttpAuth::Oauth
            || deadline <= Instant::now()
            || !matches!(
                credential_key.target(),
                CredentialTarget::ProtectedResource(resource) if resource == &configured_resource
            )
        {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid MCP OAuth token refresh request",
            ));
        }
        Ok(Self {
            server,
            config,
            client_metadata,
            credential_key,
            deadline,
        })
    }
}

pub struct McpTokenRefreshResult {
    credential_key: CredentialKey,
    issuer: Url,
    resource: Url,
    scopes: Vec<String>,
}

impl McpTokenRefreshResult {
    pub fn credential_key(&self) -> &CredentialKey {
        &self.credential_key
    }

    pub fn issuer(&self) -> &Url {
        &self.issuer
    }

    pub fn resource(&self) -> &Url {
        &self.resource
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }
}

struct OAuthDiscovery {
    profile: AdmittedMcpHttpProfile,
    issuer: Url,
    resource: Url,
    scopes: Vec<String>,
    metadata: AuthorizationServerMetadata,
}

async fn discover_oauth(
    server: &McpServerName,
    config: &McpHttpConfig,
    deadline: Instant,
    effects: &Arc<dyn McpHttpEffects>,
) -> Result<OAuthDiscovery, McpError> {
    let initial_url = config.url.as_url().clone();
    let initial_profile = AdmittedMcpHttpProfile::initial(server, &initial_url)?;
    let resource_discovery = protected_resource_metadata_url(&initial_url)?;
    let response = execute(
        &initial_profile,
        server,
        McpHttpPurpose::ProtectedResourceDiscovery,
        &resource_discovery,
        oauth_request(&resource_discovery, "GET", Vec::new())?,
        deadline,
        effects,
    )
    .await?;
    let protected: ProtectedResourceMetadata = decode_json_response(response).await?;
    let resource = normalized_origin(&protected.resource)?;
    let configured_resource = normalized_origin(&initial_url)?;
    if resource != configured_resource || protected.authorization_servers.len() != 1 {
        return Err(McpError::new(
            McpFailure::Authorization,
            "invalid protected resource binding",
        ));
    }
    let issuer = normalized_origin(&protected.authorization_servers[0])?;
    let profile = initial_profile.admitting(server, &issuer)?;
    let as_discovery = issuer
        .join(".well-known/oauth-authorization-server")
        .map_err(|error| McpError::new(McpFailure::Authorization, error.to_string()))?;
    let response = execute(
        &profile,
        server,
        McpHttpPurpose::AuthorizationServerDiscovery,
        &as_discovery,
        oauth_request(&as_discovery, "GET", Vec::new())?,
        deadline,
        effects,
    )
    .await?;
    let metadata: AuthorizationServerMetadata = decode_json_response(response).await?;
    if normalized_origin(&metadata.issuer)? != issuer
        || normalized_origin(&metadata.resource)? != resource
        || normalized_origin(&metadata.audience)? != resource
        || !metadata.client_id_metadata_document_supported
        || normalized_origin(&metadata.authorization_endpoint)? != issuer
        || normalized_origin(&metadata.token_endpoint)? != issuer
    {
        return Err(McpError::new(
            McpFailure::Authorization,
            "OAuth metadata binding mismatch",
        ));
    }
    Ok(OAuthDiscovery {
        profile,
        issuer,
        resource,
        scopes: normalized_scopes(protected.scopes_supported)?,
        metadata,
    })
}

pub async fn login(
    request: McpLoginRequest,
    effects: Arc<dyn McpHttpEffects>,
    interaction: Arc<dyn McpOAuthInteraction>,
    credentials: Arc<dyn CredentialStore>,
) -> Result<McpLoginResult, McpError> {
    let discovery =
        discover_oauth(&request.server, &request.config, request.deadline, &effects).await?;
    let OAuthDiscovery {
        profile: as_profile,
        issuer,
        resource,
        scopes,
        metadata,
    } = discovery;

    let state = opaque_nonce(b"state");
    let verifier = opaque_nonce(b"verifier");
    let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
    let mut authorization_url = metadata.authorization_endpoint.clone();
    {
        let mut query = authorization_url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", request.client_metadata.client_id().as_str())
            .append_pair("redirect_uri", request.redirect_uri.as_str())
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if !scopes.is_empty() {
            query.append_pair("scope", &scopes.join(" "));
        }
    }
    let callback = interaction
        .authorize(McpAuthorizationInteraction {
            authorization_url,
            redirect_uri: request.redirect_uri.clone(),
            state: state.clone(),
            pkce: McpPkceContext {
                challenge,
                method: McpPkceMethod::S256,
            },
        })
        .await;
    validate_callback_binding(&request.redirect_uri, callback.as_url())?;
    let callback_values = callback
        .as_url()
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    let code = callback_values
        .get("code")
        .filter(|code| !code.is_empty())
        .ok_or_else(|| McpError::new(McpFailure::Authorization, "OAuth callback has no code"))?;
    if callback_values.get("state").map(|value| value.as_ref()) != Some(state.as_str())
        || callback_values.get("iss").map(|value| value.as_ref())
            != Some(issuer.as_str().trim_end_matches('/'))
    {
        return Err(McpError::new(
            McpFailure::Authorization,
            "OAuth callback binding mismatch",
        ));
    }

    let token_profile = as_profile.admitting(&request.server, &metadata.token_endpoint)?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", request.client_metadata.client_id().as_str())
        .append_pair("code", code)
        .append_pair("redirect_uri", request.redirect_uri.as_str())
        .append_pair("code_verifier", &verifier)
        .finish()
        .into_bytes();
    let response = execute(
        &token_profile,
        &request.server,
        McpHttpPurpose::Token,
        &metadata.token_endpoint,
        oauth_request(&metadata.token_endpoint, "POST", body)?,
        request.deadline,
        &effects,
    )
    .await?;
    let token: TokenResponse = decode_json_response(response).await?;
    if token.token_type != "Bearer" || token.access_token.is_empty() {
        return Err(McpError::new(
            McpFailure::Authorization,
            "invalid token response",
        ));
    }
    let granted = normalized_scopes(
        token
            .scope
            .split_ascii_whitespace()
            .map(str::to_owned)
            .collect(),
    )?;
    if scopes.iter().any(|scope| !granted.contains(scope)) {
        return Err(McpError::new(
            McpFailure::Authorization,
            "required OAuth scope was not granted",
        ));
    }
    let bundle = McpCredentialBundle {
        access_token: SecretMaterial::new(token.access_token.into_bytes()),
        refresh_token: token
            .refresh_token
            .map(|value| SecretMaterial::new(value.into_bytes())),
        token_type: token.token_type,
        scopes: granted,
    };
    let key = CredentialKey::new(
        issuer.to_string(),
        CredentialTarget::ProtectedResource(resource.to_string()),
        token.sub,
    )
    .map_err(|error| McpError::new(McpFailure::Credential, error.to_string()))?;
    credentials
        .put(&key, bundle.encode()?)
        .map_err(|error| McpError::new(McpFailure::Credential, error.to_string()))?;
    Ok(McpLoginResult { issuer, resource })
}

pub async fn refresh_token(
    request: McpTokenRefreshRequest,
    effects: Arc<dyn McpHttpEffects>,
    credentials: Arc<dyn CredentialStore>,
) -> Result<McpTokenRefreshResult, McpError> {
    let stored = credentials
        .get(&request.credential_key)
        .map_err(|error| McpError::new(McpFailure::Credential, error.to_string()))?
        .ok_or_else(|| McpError::new(McpFailure::Credential, "MCP credential was not found"))?;
    let previous = McpCredentialBundle::decode(stored)?;
    if previous.token_type != "Bearer" {
        return Err(McpError::new(
            McpFailure::Credential,
            "stored MCP credential has an invalid token type",
        ));
    }
    let refresh_token = previous.refresh_token.as_ref().ok_or_else(|| {
        McpError::new(
            McpFailure::Credential,
            "stored MCP credential has no refresh token",
        )
    })?;
    let refresh_token = std::str::from_utf8(refresh_token.expose_secret_bytes())
        .map_err(|_| McpError::new(McpFailure::Credential, "invalid stored refresh token"))?
        .to_owned();

    let discovery =
        discover_oauth(&request.server, &request.config, request.deadline, &effects).await?;
    if discovery.issuer.as_str() != request.credential_key.issuer() {
        return Err(McpError::new(
            McpFailure::Authorization,
            "OAuth refresh issuer binding mismatch",
        ));
    }
    let target = match request.credential_key.target() {
        CredentialTarget::ProtectedResource(resource) => resource,
        CredentialTarget::Audience(_) => {
            return Err(McpError::new(
                McpFailure::Authorization,
                "OAuth refresh requires a protected-resource credential",
            ));
        }
    };
    if discovery.resource.as_str() != target {
        return Err(McpError::new(
            McpFailure::Authorization,
            "OAuth refresh resource binding mismatch",
        ));
    }
    let required_scopes = previous.scopes.clone();

    let token_profile = discovery
        .profile
        .admitting(&request.server, &discovery.metadata.token_endpoint)?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("client_id", request.client_metadata.client_id().as_str())
        .append_pair("refresh_token", &refresh_token)
        .append_pair("resource", discovery.resource.as_str())
        .append_pair("scope", &required_scopes.join(" "))
        .finish()
        .into_bytes();
    let response = execute(
        &token_profile,
        &request.server,
        McpHttpPurpose::TokenRefresh,
        &discovery.metadata.token_endpoint,
        oauth_request(&discovery.metadata.token_endpoint, "POST", body)?,
        request.deadline,
        &effects,
    )
    .await?;
    let token: TokenResponse = decode_json_response(response).await?;
    if token.token_type != "Bearer" || token.access_token.is_empty() {
        return Err(McpError::new(
            McpFailure::Authorization,
            "invalid refreshed token response",
        ));
    }
    if let Some(subject) = token.sub.as_deref()
        && request.credential_key.account() != Some(subject)
    {
        return Err(McpError::new(
            McpFailure::Authorization,
            "OAuth refresh account binding mismatch",
        ));
    }
    let granted = normalized_scopes(
        token
            .scope
            .split_ascii_whitespace()
            .map(str::to_owned)
            .collect(),
    )?;
    if required_scopes.iter().any(|scope| !granted.contains(scope)) {
        return Err(McpError::new(
            McpFailure::Authorization,
            "refreshed token dropped a granted OAuth scope",
        ));
    }
    let rotated_refresh = match token.refresh_token {
        Some(value) if !value.is_empty() => Some(SecretMaterial::new(value.into_bytes())),
        Some(_) => {
            return Err(McpError::new(
                McpFailure::Authorization,
                "refreshed token contains an empty refresh token",
            ));
        }
        None => previous.refresh_token,
    };
    let bundle = McpCredentialBundle {
        access_token: SecretMaterial::new(token.access_token.into_bytes()),
        refresh_token: rotated_refresh,
        token_type: token.token_type,
        scopes: granted.clone(),
    };
    credentials
        .put(&request.credential_key, bundle.encode()?)
        .map_err(|error| McpError::new(McpFailure::Credential, error.to_string()))?;
    Ok(McpTokenRefreshResult {
        credential_key: request.credential_key,
        issuer: discovery.issuer,
        resource: discovery.resource,
        scopes: granted,
    })
}

async fn execute(
    profile: &AdmittedMcpHttpProfile,
    server: &McpServerName,
    purpose: McpHttpPurpose,
    url: &Url,
    request: HttpRequest,
    deadline: Instant,
    effects: &Arc<dyn McpHttpEffects>,
) -> Result<McpHttpResponse, McpError> {
    let response = effects
        .execute(profile.prepare(server, purpose, url, request, deadline)?)
        .await?;
    if response.location().is_some() {
        return Err(McpError::new(
            McpFailure::Authorization,
            "OAuth redirect is not admitted",
        ));
    }
    Ok(response)
}

fn oauth_request(url: &Url, method: &str, body: Vec<u8>) -> Result<HttpRequest, McpError> {
    let mut headers = vec![header("accept", b"application/json".to_vec())?];
    if method == "POST" {
        headers.push(header(
            "content-type",
            b"application/x-www-form-urlencoded".to_vec(),
        )?);
    }
    HttpRequest::new(method, &crate::http::path_and_query(url), headers, body)
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))
}

async fn decode_json_response<T: for<'de> Deserialize<'de>>(
    response: McpHttpResponse,
) -> Result<T, McpError> {
    if !(200..300).contains(&response.status()) {
        return Err(McpError::new(
            McpFailure::Authorization,
            "OAuth endpoint rejected request",
        ));
    }
    serde_json::from_slice(&response.into_bytes(MAX_OAUTH_RESPONSE_BYTES).await?)
        .map_err(Into::into)
}

fn protected_resource_metadata_url(resource: &Url) -> Result<Url, McpError> {
    let mut url = normalized_origin(resource)?;
    let suffix = resource.path().trim_start_matches('/');
    let path = if suffix.is_empty() {
        "/.well-known/oauth-protected-resource".to_owned()
    } else {
        format!("/.well-known/oauth-protected-resource/{suffix}")
    };
    url.set_path(&path);
    Ok(url)
}

fn normalized_origin(url: &Url) -> Result<Url, McpError> {
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        return Err(McpError::new(
            McpFailure::Authorization,
            "invalid OAuth authority URL",
        ));
    }
    let mut normalized = url.clone();
    normalized.set_path("/");
    normalized.set_query(None);
    normalized.set_fragment(None);
    Ok(normalized)
}

fn validate_redirect_uri(url: &Url) -> Result<(), McpError> {
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() == "https" || (url.scheme() == "http" && loopback) {
        Ok(())
    } else {
        Err(McpError::new(
            McpFailure::InvalidValue,
            "invalid OAuth redirect URI",
        ))
    }
}

fn validate_callback_binding(expected: &Url, actual: &Url) -> Result<(), McpError> {
    let mut actual_base = actual.clone();
    actual_base.set_query(None);
    if &actual_base == expected {
        Ok(())
    } else {
        Err(McpError::new(
            McpFailure::Authorization,
            "OAuth redirect URI mismatch",
        ))
    }
}

fn normalized_scopes(mut scopes: Vec<String>) -> Result<Vec<String>, McpError> {
    if scopes
        .iter()
        .any(|scope| scope.is_empty() || scope.chars().any(char::is_whitespace))
    {
        return Err(McpError::new(
            McpFailure::Authorization,
            "invalid OAuth scope",
        ));
    }
    scopes.sort();
    scopes.dedup();
    Ok(scopes)
}

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

fn opaque_nonce(domain: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ego:mcp:oauth-nonce:v1");
    digest.update(domain);
    digest.update(NONCE_COUNTER.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    digest.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_be_bytes(),
    );
    base64url(&digest.finalize())
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(char::from(ALPHABET[((bits >> 18) & 63) as usize]));
        out.push(char::from(ALPHABET[((bits >> 12) & 63) as usize]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[((bits >> 6) & 63) as usize]));
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[(bits & 63) as usize]));
        }
    }
    out
}
