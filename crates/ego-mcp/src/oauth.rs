use std::sync::Arc;
use std::time::Instant;

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

enum Registration {
    Cimd,
    Registered,
    Dynamic,
    Interactive,
}

pub struct McpClientMetadata {
    client_id: Option<String>,
    registration: Registration,
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
            || !client_id.username().is_empty()
            || client_id.password().is_some()
            || client_name.trim().is_empty()
        {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid CIMD client metadata",
            ));
        }
        validate_redirect_uri(&redirect_uri)?;
        Ok(Self {
            client_id: Some(client_id.to_string()),
            registration: Registration::Cimd,
            client_name,
            redirect_uri,
        })
    }

    /// Select an operator-issued public client; no dynamic fallback occurs.
    pub fn registered(client_id: String, redirect_uri: Url) -> Result<Self, McpError> {
        if client_id.trim().is_empty() || client_id.chars().any(char::is_control) {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid registered client ID",
            ));
        }
        validate_redirect_uri(&redirect_uri)?;
        Ok(Self {
            client_id: Some(client_id),
            registration: Registration::Registered,
            client_name: "ego".into(),
            redirect_uri,
        })
    }

    /// Select public-client DCR, only when advertised by the bound issuer.
    pub fn dynamic(redirect_uri: Url) -> Result<Self, McpError> {
        validate_redirect_uri(&redirect_uri)?;
        Ok(Self {
            client_id: None,
            registration: Registration::Dynamic,
            client_name: "ego".into(),
            redirect_uri,
        })
    }

    /// Defer the initial choice to a human host after issuer discovery.
    /// This does not permit fallback after the host selects a strategy.
    pub fn interactive(redirect_uri: Url) -> Result<Self, McpError> {
        validate_redirect_uri(&redirect_uri)?;
        Ok(Self {
            client_id: None,
            registration: Registration::Interactive,
            client_name: "ego".into(),
            redirect_uri,
        })
    }

    fn explicit_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }

    pub fn client_id(&self) -> Option<&str> {
        self.explicit_id()
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

/// Validated issuer capabilities, not a registration or credential authority.
#[derive(Clone, Copy, Debug)]
pub struct McpRegistrationOptions {
    pub dynamic: bool,
    pub cimd: bool,
}

pub trait McpOAuthInteraction: Send + Sync {
    /// Explicit CLI clients never call this host-only initial selection.
    fn select_client<'a>(
        &'a self,
        _options: McpRegistrationOptions,
        _redirect_uri: Url,
    ) -> McpFuture<'a, Result<McpClientMetadata, McpError>> {
        Box::pin(async {
            Err(McpError::new(
                McpFailure::Authorization,
                "OAuth client selection requires an interactive host",
            ))
        })
    }

    fn authorize<'a>(
        &'a self,
        interaction: McpAuthorizationInteraction,
    ) -> McpFuture<'a, Result<McpOAuthCallback, McpError>>;
}

pub struct McpLoginResult {
    issuer: Url,
    resource: Url,
    credential_key: CredentialKey,
}

impl McpLoginResult {
    pub fn credential_key(&self) -> &CredentialKey {
        &self.credential_key
    }
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
    client_id: Option<String>,
    expires_at: Option<u64>,
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
            client_id: wire.client_id,
            expires_at: wire.expires_at,
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
            client_id: self.client_id.clone(),
            expires_at: self.expires_at,
        };
        let mut bytes = TOKEN_BUNDLE_PREFIX.to_vec();
        bytes.extend_from_slice(&serde_json::to_vec(&wire)?);
        Ok(SecretMaterial::new(bytes))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialBundleWire {
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    client_id: Option<String>,
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
    #[serde(default)]
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
    authorization_endpoint: Url,
    token_endpoint: Url,
    issuer: Url,
    #[serde(default)]
    resource: Option<Url>,
    #[serde(default)]
    audience: Option<Url>,
    #[serde(default)]
    registration_endpoint: Option<Url>,
    #[serde(default)]
    client_id_metadata_document_supported: bool,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    expires_in: Option<u64>,
    access_token: String,
    refresh_token: Option<String>,
    token_type: String,
    #[serde(default)]
    scope: Option<String>,
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
        let configured_resource = validated_identity(config.url.as_url())?.to_string();
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
    let resource = validated_identity(&protected.resource)?;
    let configured_resource = validated_identity(&initial_url)?;
    if resource != configured_resource || protected.authorization_servers.len() != 1 {
        return Err(McpError::new(
            McpFailure::Authorization,
            "invalid protected resource binding",
        ));
    }
    let issuer = validated_identity(&protected.authorization_servers[0])?;
    let profile = initial_profile.admitting(server, &issuer)?;
    let mut as_discovery = normalized_origin(&issuer)?;
    as_discovery.set_path(&format!(
        "/.well-known/oauth-authorization-server{}",
        issuer.path().trim_end_matches('/')
    ));
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
    if validated_identity(&metadata.issuer)? != issuer
        || metadata
            .resource
            .as_ref()
            .is_some_and(|value| validated_identity(value).ok().as_ref() != Some(&resource))
        || metadata
            .audience
            .as_ref()
            .is_some_and(|value| validated_identity(value).ok().as_ref() != Some(&resource))
        || normalized_origin(&metadata.authorization_endpoint)? != normalized_origin(&issuer)?
        || normalized_origin(&metadata.token_endpoint)? != normalized_origin(&issuer)?
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

pub async fn login<'a>(
    mut request: McpLoginRequest,
    effects: Arc<dyn McpHttpEffects>,
    interaction: Arc<dyn McpOAuthInteraction + 'a>,
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

    validate_public_client(&metadata)?;
    if matches!(
        request.client_metadata.registration,
        Registration::Interactive
    ) {
        request.client_metadata = interaction
            .select_client(
                McpRegistrationOptions {
                    dynamic: metadata.registration_endpoint.is_some(),
                    cimd: metadata.client_id_metadata_document_supported,
                },
                request.redirect_uri.clone(),
            )
            .await?;
        if request.client_metadata.redirect_uri != request.redirect_uri
            || matches!(
                request.client_metadata.registration,
                Registration::Interactive
            )
        {
            return Err(McpError::new(
                McpFailure::Authorization,
                "invalid interactive OAuth client selection",
            ));
        }
    }
    let client_id = match &request.client_metadata.registration {
        Registration::Cimd if !metadata.client_id_metadata_document_supported => {
            return Err(McpError::new(
                McpFailure::Authorization,
                "issuer does not support CIMD",
            ));
        }
        Registration::Dynamic => {
            let endpoint = metadata.registration_endpoint.as_ref().ok_or_else(|| {
                McpError::new(McpFailure::Authorization, "issuer does not advertise DCR")
            })?;
            if normalized_origin(endpoint)? != normalized_origin(&issuer)? {
                return Err(McpError::new(
                    McpFailure::Authorization,
                    "DCR endpoint issuer mismatch",
                ));
            }
            let body = serde_json::to_vec(&serde_json::json!({
                "client_name": request.client_metadata.client_name(),
                "redirect_uris": [request.redirect_uri.as_str()],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none"
            }))?;
            let registration = HttpRequest::new(
                "POST",
                &crate::http::path_and_query(endpoint),
                vec![header("content-type", b"application/json".to_vec())?],
                body,
            )
            .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
            let response = execute(
                &as_profile,
                &request.server,
                McpHttpPurpose::ClientMetadata,
                endpoint,
                registration,
                request.deadline,
                &effects,
            )
            .await?;
            fn field_present<'de, D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> Result<bool, D::Error> {
                serde::de::IgnoredAny::deserialize(deserializer).map(|_| true)
            }
            fn requested_public_auth_method() -> String {
                "none".to_owned()
            }
            #[derive(Deserialize)]
            struct Registered {
                client_id: String,
                #[serde(default = "requested_public_auth_method")]
                token_endpoint_auth_method: String,
                #[serde(default, deserialize_with = "field_present")]
                client_secret: bool,
                redirect_uris: Vec<Url>,
            }
            let registered: Registered = decode_json_response(response).await?;
            // Interoperability exception for publishwith.ai, not RFC compliance:
            // RFC 7591 §2 defaults omission to client_secret_basic, unusable without
            // a secret. Retain our requested public method only when no secret is
            // returned; §3.2.1 still requires checking explicit metadata bindings.
            if registered.client_id.trim().is_empty()
                || registered.client_secret
                || registered.token_endpoint_auth_method != "none"
                || registered.redirect_uris != [request.redirect_uri.clone()]
            {
                return Err(McpError::new(
                    McpFailure::Authorization,
                    "DCR public-client binding mismatch",
                ));
            }
            registered.client_id
        }
        _ => request
            .client_metadata
            .explicit_id()
            .expect("explicit registration")
            .to_owned(),
    };
    let state = opaque_nonce()?;
    let verifier = opaque_nonce()?;
    let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
    let mut authorization_url = metadata.authorization_endpoint.clone();
    {
        let mut query = authorization_url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", request.redirect_uri.as_str())
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", resource.as_str());
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
        .await?;
    validate_callback_binding(&request.redirect_uri, callback.as_url())?;
    let mut seen = std::collections::HashSet::new();
    if callback
        .as_url()
        .query_pairs()
        .any(|(key, _)| !seen.insert(key.into_owned()))
    {
        return Err(McpError::new(
            McpFailure::Authorization,
            "duplicate OAuth callback parameter",
        ));
    }
    let callback_values = callback
        .as_url()
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    if callback_values.contains_key("error") {
        return Err(McpError::new(
            McpFailure::Authorization,
            "OAuth authorization was denied",
        ));
    }
    let code = callback_values
        .get("code")
        .filter(|code| !code.is_empty())
        .ok_or_else(|| McpError::new(McpFailure::Authorization, "OAuth callback has no code"))?;
    if callback_values.get("state").map(|value| value.as_ref()) != Some(state.as_str())
        || callback_values.get("iss").is_some_and(|value| {
            value.as_ref().trim_end_matches('/') != issuer.as_str().trim_end_matches('/')
        })
    {
        return Err(McpError::new(
            McpFailure::Authorization,
            "OAuth callback binding mismatch",
        ));
    }

    let token_profile = as_profile.admitting(&request.server, &metadata.token_endpoint)?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", &client_id)
        .append_pair("code", code)
        .append_pair("redirect_uri", request.redirect_uri.as_str())
        .append_pair("resource", resource.as_str())
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
            .as_deref()
            .unwrap_or(&scopes.join(" "))
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
    if token.refresh_token.as_ref().is_some_and(String::is_empty) {
        return Err(McpError::new(
            McpFailure::Authorization,
            "empty OAuth refresh token",
        ));
    }
    let bundle = McpCredentialBundle {
        access_token: SecretMaterial::new(token.access_token.into_bytes()),
        refresh_token: token
            .refresh_token
            .map(|value| SecretMaterial::new(value.into_bytes())),
        token_type: token.token_type,
        scopes: granted,
        client_id: Some(client_id),
        expires_at: token_expiry(token.expires_in)?,
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
    Ok(McpLoginResult {
        issuer,
        resource,
        credential_key: key,
    })
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
    let client_id = previous
        .client_id
        .as_deref()
        .or_else(|| request.client_metadata.explicit_id())
        .ok_or_else(|| {
            McpError::new(McpFailure::Credential, "stored DCR client identity absent")
        })?;
    if request
        .client_metadata
        .explicit_id()
        .is_some_and(|id| id != client_id)
    {
        return Err(McpError::new(
            McpFailure::Credential,
            "refresh client identity mismatch",
        ));
    }
    if matches!(request.client_metadata.registration, Registration::Cimd)
        && !discovery.metadata.client_id_metadata_document_supported
    {
        return Err(McpError::new(
            McpFailure::Authorization,
            "issuer does not support CIMD",
        ));
    }
    validate_public_client(&discovery.metadata)?;
    let required_scopes = previous.scopes.clone();

    let token_profile = discovery
        .profile
        .admitting(&request.server, &discovery.metadata.token_endpoint)?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("client_id", client_id)
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
            .as_deref()
            .unwrap_or(&required_scopes.join(" "))
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
        client_id: Some(client_id.to_owned()),
        expires_at: token_expiry(token.expires_in)?,
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
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || (url.scheme() == "http"
            && !url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }))
    {
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
    if (url.scheme() == "https" || (url.scheme() == "http" && loopback))
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
    {
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

fn opaque_nonce() -> Result<String, McpError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| McpError::new(McpFailure::Credential, "OAuth entropy unavailable"))?;
    Ok(base64url(&bytes))
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

fn validated_identity(url: &Url) -> Result<Url, McpError> {
    normalized_origin(url)?;
    if url.query().is_some() {
        return Err(McpError::new(
            McpFailure::Authorization,
            "OAuth identity has a query",
        ));
    }
    Ok(url.clone())
}

fn unix_now() -> Result<u64, McpError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .map_err(|_| McpError::new(McpFailure::Credential, "OAuth clock unavailable"))
}

fn token_expiry(seconds: Option<u64>) -> Result<Option<u64>, McpError> {
    seconds
        .map(|seconds| {
            unix_now()?
                .checked_add(seconds)
                .ok_or_else(|| McpError::new(McpFailure::Authorization, "OAuth expiry overflow"))
        })
        .transpose()
}

/// Adapter-owned exclusive refresh transaction; held through durable token publication.
pub trait McpOAuthRefreshLease: Send {}
pub trait McpOAuthRefreshLock: Send + Sync {
    fn acquire(&self) -> Result<Box<dyn McpOAuthRefreshLease>, McpError>;
}

/// Live credential authority; identities can be persisted, token bytes cannot.
pub struct McpOAuthSession {
    server: McpServerName,
    config: McpHttpConfig,
    key: CredentialKey,
    credentials: Arc<dyn CredentialStore>,
    state: tokio::sync::Mutex<Option<Arc<dyn McpHttpEffects>>>,
    refresh_lock: Option<Arc<dyn McpOAuthRefreshLock>>,
    // rmcp can erase typed transport failures. Retain only this sanitized
    // credential outcome so admission never diagnoses revocation as config.
    credential_required: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for McpOAuthSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOAuthSession")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl McpOAuthSession {
    pub fn new(
        server: McpServerName,
        config: McpHttpConfig,
        key: CredentialKey,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, McpError> {
        if !matches!(key.target(), CredentialTarget::ProtectedResource(resource) if resource == config.url.as_url().as_str())
        {
            return Err(McpError::new(
                McpFailure::Credential,
                "OAuth locator resource mismatch",
            ));
        }
        Ok(Self {
            server,
            config,
            key,
            credentials,
            state: tokio::sync::Mutex::new(None),
            refresh_lock: None,
            credential_required: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(crate) fn credential_required(&self) -> bool {
        self.credential_required
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn matches(&self, name: &McpServerName, config: &McpHttpConfig) -> bool {
        &self.server == name && &self.config == config
    }

    pub fn with_refresh_lock(mut self, lock: Arc<dyn McpOAuthRefreshLock>) -> Self {
        self.refresh_lock = Some(lock);
        self
    }

    fn hint(&self) -> McpError {
        self.credential_required
            .store(true, std::sync::atomic::Ordering::Release);
        McpError::new(
            McpFailure::Credential,
            format!(
                "MCP credential unavailable or revoked; run ego mcp login {} with an explicit registration strategy",
                self.server
            ),
        )
    }

    fn load(&self) -> Result<McpCredentialBundle, McpError> {
        self.credentials
            .get(&self.key)
            .map_err(|_| self.hint())?
            .ok_or_else(|| self.hint())
            .and_then(McpCredentialBundle::decode)
    }

    async fn refresh(
        &self,
        effects: Arc<dyn McpHttpEffects>,
        deadline: Instant,
    ) -> Result<(), McpError> {
        let _lease = self
            .refresh_lock
            .as_ref()
            .map(|lock| lock.acquire())
            .transpose()?;
        let bundle = self.load()?;
        let id = bundle.client_id.ok_or_else(|| self.hint())?;
        let metadata = McpClientMetadata::registered(
            id,
            "http://127.0.0.1/callback".parse().expect("static URL"),
        )?;
        let request = McpTokenRefreshRequest::new(
            self.server.clone(),
            self.config.clone(),
            metadata,
            self.key.clone(),
            deadline,
        )?;
        refresh_token(request, effects, Arc::clone(&self.credentials))
            .await
            .map_err(|_| self.hint())?;
        Ok(())
    }

    pub(crate) async fn headers(
        &self,
        effects: Option<Arc<dyn McpHttpEffects>>,
        deadline: Instant,
    ) -> Result<Vec<ego_core::permit::HttpHeader>, McpError> {
        let mut state = self.state.lock().await;
        if let Some(effects) = effects {
            *state = Some(effects);
        }
        let bundle = self.load()?;
        if bundle
            .expires_at
            .is_some_and(|expiry| expiry <= unix_now().unwrap_or(u64::MAX).saturating_add(30))
        {
            self.refresh(state.clone().ok_or_else(|| self.hint())?, deadline)
                .await?;
        }
        let bundle = self.load()?;
        let mut value = SecretMaterial::new(b"Bearer ".to_vec());
        value.extend_from_slice(bundle.access_token.expose_secret_bytes());
        self.credential_required
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(vec![
            ego_core::permit::HttpHeader::secret("authorization", value)
                .map_err(|_| self.hint())?,
        ])
    }

    pub(crate) async fn unauthorized(&self, deadline: Instant) -> McpError {
        // A selected operation's one-shot authority is never replayed. Refresh
        // the next attempt instead, even if this 401 was returned for tools/call.
        let state = self.state.lock().await;
        match state.clone() {
            Some(effects) if self.refresh(Arc::clone(&effects), deadline).await.is_ok() => {
                McpError::new(
                    McpFailure::Authorization,
                    "MCP credential refreshed after HTTP 401; retry the operation",
                )
            }
            _ => self.hint(),
        }
    }
}

pub(crate) struct AuthorizedMcpHttpEffects {
    pub admission: Arc<crate::McpServerAdmission>,
    pub inner: Arc<dyn McpHttpEffects>,
}

impl McpHttpEffects for AuthorizedMcpHttpEffects {
    fn execute(
        &self,
        call: crate::PreparedMcpHttpCall,
    ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
        let admission = Arc::clone(&self.admission);
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let deadline = call.deadline();
            let headers = admission
                .authorization_headers(Some(Arc::clone(&inner)), deadline)
                .await?;
            let response = inner.execute(call.with_headers(&headers)?).await?;
            if response.status() == 401
                && let Some(session) = admission.oauth_session()
            {
                return Err(session.unauthorized(deadline).await);
            }
            Ok(response)
        })
    }
    fn cancel(
        &self,
        id: crate::McpRequestId,
    ) -> McpFuture<'static, Result<crate::McpHttpSettlement, McpError>> {
        self.inner.cancel(id)
    }
    fn timeout(
        &self,
        id: crate::McpRequestId,
    ) -> McpFuture<'static, Result<crate::McpHttpSettlement, McpError>> {
        self.inner.timeout(id)
    }
    fn settle(
        &self,
        id: crate::McpRequestId,
    ) -> McpFuture<'static, Result<crate::McpHttpSettlement, McpError>> {
        self.inner.settle(id)
    }
}

fn validate_public_client(metadata: &AuthorizationServerMetadata) -> Result<(), McpError> {
    if metadata
        .token_endpoint_auth_methods_supported
        .as_ref()
        .is_some_and(|methods| !methods.iter().any(|method| method == "none"))
    {
        return Err(McpError::new(
            McpFailure::Authorization,
            "issuer does not advertise public-client token authentication",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::{McpHttpEnd, McpHttpSettlement, McpRequestId, PreparedMcpHttpCall};
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Store(Mutex<HashMap<CredentialKey, Vec<u8>>>);
    impl CredentialStore for Store {
        fn get(
            &self,
            key: &CredentialKey,
        ) -> Result<Option<SecretMaterial>, ego_core::credentials::CredentialStoreError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .map(SecretMaterial::new))
        }
        fn put(
            &self,
            key: &CredentialKey,
            value: SecretMaterial,
        ) -> Result<(), ego_core::credentials::CredentialStoreError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.clone(), value.expose_secret_bytes().to_vec());
            Ok(())
        }
        fn delete(
            &self,
            key: &CredentialKey,
        ) -> Result<
            ego_core::credentials::CredentialRemoval,
            ego_core::credentials::CredentialStoreError,
        > {
            self.0.lock().unwrap().remove(key);
            Ok(ego_core::credentials::CredentialRemoval::Removed)
        }
    }
    struct Http {
        responses: Mutex<VecDeque<McpHttpResponse>>,
        authorization: Mutex<Vec<Vec<u8>>>,
    }
    impl McpHttpEffects for Http {
        fn execute(
            &self,
            call: PreparedMcpHttpCall,
        ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
            for header in call.request().headers() {
                if header.name() == "authorization" {
                    self.authorization
                        .lock()
                        .unwrap()
                        .push(header.value().to_vec());
                }
            }
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("bounded response contract");
            Box::pin(async move { Ok(response) })
        }
        fn cancel(
            &self,
            id: McpRequestId,
        ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
            settle(id, McpHttpEnd::Cancelled)
        }
        fn timeout(
            &self,
            id: McpRequestId,
        ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
            settle(id, McpHttpEnd::TimedOut)
        }
        fn settle(
            &self,
            id: McpRequestId,
        ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
            settle(id, McpHttpEnd::Completed)
        }
    }
    fn settle(
        id: McpRequestId,
        end: McpHttpEnd,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        Box::pin(
            async move { McpHttpSettlement::new(id, end, crate::DeliveryCertainty::PossiblySent) },
        )
    }
    fn setup(
        expiry: Option<u64>,
        responses: Vec<McpHttpResponse>,
    ) -> (Arc<McpOAuthSession>, Arc<Store>, Arc<Http>) {
        let config = McpHttpConfig::new(
            "https://mcp.linear.app/mcp".parse::<Url>().unwrap(),
            McpHttpAuth::Oauth,
            false,
        )
        .unwrap();
        let key = CredentialKey::new(
            "https://mcp.linear.app/".into(),
            CredentialTarget::ProtectedResource("https://mcp.linear.app/mcp".into()),
            None,
        )
        .unwrap();
        let store = Arc::new(Store::default());
        let bundle = McpCredentialBundle {
            access_token: "old-access".into(),
            refresh_token: Some("refresh".into()),
            token_type: "Bearer".into(),
            scopes: vec!["read".into(), "write".into()],
            client_id: Some("operator-client".into()),
            expires_at: expiry,
        };
        store.put(&key, bundle.encode().unwrap()).unwrap();
        let session = Arc::new(
            McpOAuthSession::new("linear".parse().unwrap(), config, key, store.clone()).unwrap(),
        );
        (
            session,
            store,
            Arc::new(Http {
                responses: Mutex::new(responses.into()),
                authorization: Mutex::new(Vec::new()),
            }),
        )
    }
    fn refresh_responses(status: u16) -> Vec<McpHttpResponse> {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/oauth/linear.json")).unwrap();
        vec![
            McpHttpResponse::json(
                200,
                serde_json::to_vec(&fixture["protected_resource"]).unwrap(),
            ),
            McpHttpResponse::json(
                200,
                serde_json::to_vec(&fixture["authorization_server"]).unwrap(),
            ),
            McpHttpResponse::json(
                status,
                br#"{"access_token":"new-access","token_type":"Bearer","expires_in":3600}"#,
            ),
        ]
    }
    #[tokio::test]
    async fn stored_oauth_credential_authenticates_current_protocol_discovery() {
        // Catches management admission ignoring retained authorization material.
        let (session, _, http) = setup(None, vec![McpHttpResponse::json(200,
            br#"{"jsonrpc":"2.0","id":0,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"remote","version":"1"}}}}"#)]);
        let admission = Arc::new(
            crate::McpServerAdmission::new(
                session.server.clone(),
                ego_core::config::McpServerConfig::Http(session.config.clone()),
                crate::McpTransportMaterial::oauth(session.clone()),
            )
            .unwrap(),
        );
        let effects = Arc::new(AuthorizedMcpHttpEffects {
            admission,
            inner: http.clone(),
        });
        let options = crate::McpClientOptions::new(
            1024 * 1024,
            Instant::now() + std::time::Duration::from_secs(10),
        )
        .unwrap();
        let mut client = crate::McpClient::connect_http(
            session.server.clone(),
            session.config.clone(),
            options,
            effects,
        )
        .unwrap();
        client.discover().await.unwrap();
        client.settle().await.unwrap();
        assert_eq!(
            *http.authorization.lock().unwrap(),
            vec![b"Bearer old-access".to_vec()]
        );
    }

    #[tokio::test]
    async fn expired_credential_refreshes_once_before_runtime_header_attachment() {
        // Catches attaching stale bytes and concurrent refresh-token reuse.
        let (session, _, http) = setup(Some(0), refresh_responses(200));
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let (first, second) = tokio::join!(
            session.headers(Some(http.clone()), deadline),
            session.headers(Some(http.clone()), deadline)
        );
        assert_eq!(first.unwrap()[0].value(), b"Bearer new-access");
        assert_eq!(second.unwrap()[0].value(), b"Bearer new-access");
        assert!(http.responses.lock().unwrap().is_empty());
        assert!(
            http.authorization.lock().unwrap().is_empty(),
            "tokens never attach to metadata/token calls"
        );
    }
    #[tokio::test]
    async fn runtime_401_refreshes_without_replaying_selected_side_effect() {
        // Catches reusing spent selected authority after an unauthorized response.
        let mut responses = vec![McpHttpResponse::json(401, b"{}")];
        responses.extend(refresh_responses(200));
        let (session, _, http) = setup(None, responses);
        let admission = Arc::new(
            crate::McpServerAdmission::new(
                session.server.clone(),
                ego_core::config::McpServerConfig::Http(session.config.clone()),
                crate::McpTransportMaterial::oauth(session.clone()),
            )
            .unwrap(),
        );
        let effects = AuthorizedMcpHttpEffects {
            admission,
            inner: http.clone(),
        };
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let url = session.config.url.as_url();
        let profile = AdmittedMcpHttpProfile::initial(&session.server, url).unwrap();
        let call = profile
            .prepare(
                &session.server,
                McpHttpPurpose::Protocol,
                url,
                oauth_request(url, "POST", b"{}".to_vec()).unwrap(),
                deadline,
            )
            .unwrap();
        let error = effects.execute(call).await.err().unwrap();
        assert!(error.to_string().contains("retry the operation"));
        assert_eq!(
            *http.authorization.lock().unwrap(),
            vec![b"Bearer old-access".to_vec()]
        );
        assert_eq!(
            session.headers(None, deadline).await.unwrap()[0].value(),
            b"Bearer new-access"
        );
    }
    #[tokio::test]
    async fn missing_and_revoked_credentials_offer_login_without_overwriting_store() {
        // Catches publishing failed refresh output or hiding the recovery command.
        let (session, store, http) = setup(Some(0), refresh_responses(400));
        let original = store.0.lock().unwrap().clone();
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        assert!(
            session
                .headers(Some(http), deadline)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("ego mcp login linear")
        );
        assert!(session.credential_required());
        assert_eq!(*store.0.lock().unwrap(), original);
        store.0.lock().unwrap().clear();
        assert!(
            session
                .headers(None, deadline)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("ego mcp login linear")
        );
    }
}
