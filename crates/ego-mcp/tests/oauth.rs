//! RED contract for current MCP authorization and credential identity.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ego_core::config::{McpHttpAuth, McpHttpConfig, McpHttpUrl};
use ego_core::credentials::{
    CredentialKey, CredentialRemoval, CredentialStore, CredentialStoreError, CredentialTarget,
    SecretMaterial,
};
use ego_mcp::{
    McpAuthorizationInteraction, McpClientMetadata, McpCredentialBundle, McpError, McpFuture,
    McpHttpEffects, McpHttpEnd, McpHttpPurpose, McpHttpResponse, McpHttpSettlement,
    McpLoginRequest, McpOAuthCallback, McpOAuthInteraction, McpPkceMethod, McpRequestId,
    PreparedMcpHttpCall, login,
};

type RecordedHttpCall = (String, McpHttpPurpose, String, String, String, Vec<u8>);

struct OAuthHttp {
    calls: Mutex<Vec<RecordedHttpCall>>,
    responses: Mutex<VecDeque<McpHttpResponse>>,
}

impl OAuthHttp {
    fn with_responses(responses: impl IntoIterator<Item = McpHttpResponse>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into_iter().collect()),
        })
    }
}

impl McpHttpEffects for OAuthHttp {
    fn execute(
        &self,
        call: PreparedMcpHttpCall,
    ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
        self.calls.lock().expect("OAuth calls").push((
            format!("{:?}", call.request_id()),
            call.purpose(),
            call.request().method().to_owned(),
            call.route().target().host().to_owned(),
            call.request().path().to_owned(),
            call.request().body().to_vec(),
        ));
        let response = self
            .responses
            .lock()
            .expect("OAuth responses")
            .pop_front()
            .expect("OAuth response was queued");
        Box::pin(async move { Ok(response) })
    }

    fn cancel(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        Box::pin(async move {
            McpHttpSettlement::new(
                request_id,
                McpHttpEnd::Cancelled,
                ego_mcp::DeliveryCertainty::DefinitelyUnsent,
            )
        })
    }

    fn timeout(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        Box::pin(async move {
            McpHttpSettlement::new(
                request_id,
                McpHttpEnd::TimedOut,
                ego_mcp::DeliveryCertainty::DefinitelyUnsent,
            )
        })
    }

    fn settle(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        Box::pin(async move {
            McpHttpSettlement::new(
                request_id,
                McpHttpEnd::Completed,
                ego_mcp::DeliveryCertainty::DefinitelyUnsent,
            )
        })
    }
}

struct RecordingInteraction {
    captured: Mutex<Option<McpAuthorizationInteraction>>,
}

impl McpOAuthInteraction for RecordingInteraction {
    fn authorize<'a>(
        &'a self,
        interaction: McpAuthorizationInteraction,
    ) -> McpFuture<'a, McpOAuthCallback> {
        let callback = format!(
            "{}?code=auth-code&state={}&iss=https%3A%2F%2Fissuer.example.test",
            interaction.redirect_uri(),
            interaction.state()
        )
        .parse()
        .expect("callback URL");
        *self.captured.lock().expect("authorization interaction") = Some(interaction);
        Box::pin(async move { McpOAuthCallback::from_callback_url(callback).expect("callback") })
    }
}

#[derive(Clone, Copy)]
enum CallbackTamper {
    State,
    Issuer,
}

struct TamperedInteraction {
    tamper: CallbackTamper,
}

impl McpOAuthInteraction for TamperedInteraction {
    fn authorize<'a>(
        &'a self,
        interaction: McpAuthorizationInteraction,
    ) -> McpFuture<'a, McpOAuthCallback> {
        let state = match self.tamper {
            CallbackTamper::State => "wrong-state",
            CallbackTamper::Issuer => interaction.state(),
        };
        let issuer = match self.tamper {
            CallbackTamper::Issuer => "https://other.example.test",
            CallbackTamper::State => "https://issuer.example.test",
        };
        let callback = format!(
            "{}?code=auth-code&state={state}&iss={issuer}",
            interaction.redirect_uri()
        )
        .parse()
        .expect("callback URL");
        Box::pin(async move { McpOAuthCallback::from_callback_url(callback).expect("callback") })
    }
}

struct RecordingCredentials {
    values: Mutex<HashMap<CredentialKey, Vec<u8>>>,
    puts: Mutex<usize>,
    deletes: Mutex<usize>,
}

impl RecordingCredentials {
    fn new() -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
            puts: Mutex::new(0),
            deletes: Mutex::new(0),
        }
    }
}

impl CredentialStore for RecordingCredentials {
    fn get(&self, key: &CredentialKey) -> Result<Option<SecretMaterial>, CredentialStoreError> {
        Ok(self
            .values
            .lock()
            .expect("credentials")
            .get(key)
            .cloned()
            .map(SecretMaterial::new))
    }
    fn put(&self, key: &CredentialKey, value: SecretMaterial) -> Result<(), CredentialStoreError> {
        *self.puts.lock().expect("credential put counter") += 1;
        self.values
            .lock()
            .expect("credentials")
            .insert(key.clone(), value.expose_secret_bytes().to_vec());
        Ok(())
    }
    fn delete(&self, key: &CredentialKey) -> Result<CredentialRemoval, CredentialStoreError> {
        *self.deletes.lock().expect("credential delete counter") += 1;
        Ok(
            if self
                .values
                .lock()
                .expect("credentials")
                .remove(key)
                .is_some()
            {
                CredentialRemoval::Removed
            } else {
                CredentialRemoval::Absent
            },
        )
    }
}

fn client_metadata() -> McpClientMetadata {
    McpClientMetadata::new(
        "https://client.example.test/metadata"
            .parse()
            .expect("client metadata URL"),
        "ego MCP client",
        "http://127.0.0.1:9999/callback"
            .parse()
            .expect("redirect URL"),
    )
    .expect("client metadata")
}

fn login_request_with_metadata(metadata: McpClientMetadata) -> McpLoginRequest {
    let config = McpHttpConfig::new(
        McpHttpUrl::from(
            "https://mcp.example.test/mcp"
                .parse::<url::Url>()
                .expect("MCP URL"),
        ),
        McpHttpAuth::Oauth,
        false,
    )
    .expect("MCP HTTP config");
    McpLoginRequest::new(
        "remote".parse().expect("server name"),
        config,
        "http://127.0.0.1:9999/callback".parse().expect("redirect"),
        metadata,
        Instant::now() + Duration::from_secs(30),
    )
    .expect("login request")
}

fn login_request() -> McpLoginRequest {
    login_request_with_metadata(client_metadata())
}

fn refresh_key(account: &str) -> CredentialKey {
    CredentialKey::new(
        "https://issuer.example.test/".to_owned(),
        CredentialTarget::ProtectedResource("https://mcp.example.test/".to_owned()),
        Some(account.to_owned()),
    )
    .expect("refresh credential key")
}

fn encoded_bundle(access_token: &str, refresh_token: Option<&str>) -> Vec<u8> {
    let refresh_token = refresh_token
        .map(|token| format!(",\"refresh_token\":\"{token}\""))
        .unwrap_or_else(|| ",\"refresh_token\":null".to_owned());
    format!(
        "ego:mcp:token-bundle:v1\n{{\"access_token\":\"{access_token}\"{refresh_token},\"token_type\":\"Bearer\",\"scopes\":[\"tools:execute\",\"tools:read\"]}}"
    )
    .into_bytes()
}

fn seeded_credentials(key: &CredentialKey) -> Arc<RecordingCredentials> {
    let credentials = Arc::new(RecordingCredentials::new());
    credentials.values.lock().expect("credentials").insert(
        key.clone(),
        encoded_bundle("old-access", Some("old-refresh")),
    );
    credentials
}

fn refresh_request(key: CredentialKey) -> ego_mcp::McpTokenRefreshRequest {
    let config = McpHttpConfig::new(
        McpHttpUrl::from(
            "https://mcp.example.test/mcp"
                .parse::<url::Url>()
                .expect("MCP URL"),
        ),
        McpHttpAuth::Oauth,
        false,
    )
    .expect("MCP HTTP config");
    ego_mcp::McpTokenRefreshRequest::new(
        "remote".parse().expect("server name"),
        config,
        client_metadata(),
        key,
        Instant::now() + Duration::from_secs(30),
    )
    .expect("refresh request")
}

fn refresh_discovery_responses(token_response: &str) -> Vec<McpHttpResponse> {
    vec![
        McpHttpResponse::json(
            200,
            br#"{"resource":"https://mcp.example.test","authorization_servers":["https://issuer.example.test"],"scopes_supported":["tools:read","tools:execute"]}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
        ),
        McpHttpResponse::json(200, token_response.as_bytes()),
    ]
}

#[test]
fn login_uses_cimd_pkce_metadata_and_issuer_resource_bound_token_identity() {
    let http = OAuthHttp::with_responses([
        McpHttpResponse::json(
            200,
            br#"{"resource":"https://mcp.example.test","authorization_servers":["https://issuer.example.test"],"scopes_supported":["tools:read","tools:execute"]}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"access_token":"opaque-access","refresh_token":"opaque-refresh","token_type":"Bearer","scope":"tools:read tools:execute","sub":"account-a"}"#,
        ),
    ]);
    let interaction = Arc::new(RecordingInteraction {
        captured: Mutex::new(None),
    });
    let credentials = Arc::new(RecordingCredentials::new());
    let result = futures::executor::block_on(login(
        login_request(),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
        Arc::clone(&interaction) as Arc<dyn McpOAuthInteraction>,
        Arc::clone(&credentials) as Arc<dyn CredentialStore>,
    ))
    .expect("login");
    assert!(
        interaction
            .captured
            .lock()
            .expect("authorization interaction")
            .as_ref()
            .expect("captured interaction")
            .authorization_url()
            .query_pairs()
            .any(|(name, value)| { name == "code_challenge" && !value.is_empty() })
    );
    let captured = interaction
        .captured
        .lock()
        .expect("authorization interaction");
    let captured = captured.as_ref().expect("captured interaction");
    assert_eq!(captured.pkce().method(), McpPkceMethod::S256);
    assert!(!captured.pkce().challenge().is_empty());
    assert!(!captured.state().is_empty());
    assert_eq!(
        captured.redirect_uri().as_str(),
        "http://127.0.0.1:9999/callback"
    );
    assert!(
        captured
            .authorization_url()
            .query_pairs()
            .any(|(name, value)| { name == "code_challenge_method" && value == "S256" })
    );
    assert!(
        captured
            .authorization_url()
            .query_pairs()
            .any(|(name, value)| name == "scope" && value.contains("tools:execute"))
    );
    assert!(
        captured
            .authorization_url()
            .query_pairs()
            .any(|(name, value)| {
                name == "client_id" && value == "https://client.example.test/metadata"
            })
    );
    assert_eq!(result.issuer().as_str(), "https://issuer.example.test/");
    assert_eq!(result.resource().as_str(), "https://mcp.example.test/");

    let calls = http.calls.lock().expect("OAuth calls");
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].1, McpHttpPurpose::ProtectedResourceDiscovery);
    assert_eq!(calls[0].2, "GET");
    assert_eq!(calls[0].3, "mcp.example.test");
    assert_eq!(calls[0].4, "/.well-known/oauth-protected-resource/mcp");
    assert!(calls[0].5.is_empty());
    assert_eq!(calls[1].1, McpHttpPurpose::AuthorizationServerDiscovery);
    assert_eq!(calls[1].2, "GET");
    assert_eq!(calls[1].3, "issuer.example.test");
    assert_eq!(calls[1].4, "/.well-known/oauth-authorization-server");
    assert_eq!(calls[2].1, McpHttpPurpose::Token);
    assert_eq!(calls[2].2, "POST");
    assert_eq!(calls[2].3, "issuer.example.test");
    assert_eq!(calls[2].4, "/token");
    assert!(
        String::from_utf8_lossy(&calls[2].5)
            .contains("client_id=https%3A%2F%2Fclient.example.test%2Fmetadata")
    );
    assert!(String::from_utf8_lossy(&calls[2].5).contains("code=auth-code"));
    assert!(String::from_utf8_lossy(&calls[2].5).contains("code_verifier"));
    for (index, call) in calls.iter().enumerate() {
        assert!(
            calls[index + 1..]
                .iter()
                .all(|other| call.0.as_str() != other.0.as_str())
        );
    }
    assert!(!calls.iter().any(|(_, _, _, _, _, body)| {
        String::from_utf8_lossy(body).contains("opaque-access")
            || String::from_utf8_lossy(body).contains("opaque-refresh")
    }));
    let keys = credentials.values.lock().expect("credentials");
    assert_eq!(keys.len(), 1);
    let (key, secret) = keys.iter().next().expect("credential");
    let key = key.clone();
    let stored = secret.clone();
    drop(keys);
    assert_eq!(key.issuer(), "https://issuer.example.test/");
    assert!(matches!(
        key.target(),
        CredentialTarget::ProtectedResource(resource) if resource == "https://mcp.example.test/"
    ));
    assert_eq!(key.account(), Some("account-a"));
    assert_ne!(stored, b"opaque-accessopaque-refresh");
    assert!(stored.starts_with(b"ego:mcp:token-bundle:v1"));
    let bundle = McpCredentialBundle::decode(SecretMaterial::new(stored.clone()))
        .expect("versioned token bundle");
    assert_eq!(
        bundle.access_token().expose_secret_bytes(),
        b"opaque-access"
    );
    assert_eq!(
        bundle
            .refresh_token()
            .expect("refresh token")
            .expose_secret_bytes(),
        b"opaque-refresh"
    );
    assert_eq!(bundle.token_type(), "Bearer");
    assert_eq!(
        bundle
            .scopes()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["tools:execute", "tools:read"]
    );
    let debug = format!("{bundle:?}");
    assert!(!debug.contains("opaque-access"));
    assert!(!debug.contains("opaque-refresh"));
    assert!(
        McpCredentialBundle::decode(SecretMaterial::new(b"ego:mcp:token-bundle:v999".to_vec(),))
            .is_err()
    );
    let restored = credentials
        .get(&key)
        .expect("credential restore")
        .expect("stored credential");
    assert_eq!(restored.expose_secret_bytes(), stored.as_slice());
}

#[test]
fn cimd_requires_an_https_path_and_login_binds_its_redirect() {
    assert!(
        McpClientMetadata::new(
            "http://client.example.test/metadata"
                .parse()
                .expect("client metadata URL"),
            "ego MCP client",
            "http://127.0.0.1:9999/callback"
                .parse()
                .expect("redirect URL"),
        )
        .is_err()
    );
    assert!(
        McpClientMetadata::new(
            "https://client.example.test"
                .parse()
                .expect("client metadata URL"),
            "ego MCP client",
            "http://127.0.0.1:9999/callback"
                .parse()
                .expect("redirect URL"),
        )
        .is_err()
    );

    let metadata = McpClientMetadata::new(
        "https://client.example.test/metadata"
            .parse()
            .expect("client metadata URL"),
        "ego MCP client",
        "https://attacker.example.test/callback"
            .parse()
            .expect("redirect URL"),
    )
    .expect("metadata shape");
    let config = McpHttpConfig::new(
        McpHttpUrl::from(
            "https://mcp.example.test/mcp"
                .parse::<url::Url>()
                .expect("MCP URL"),
        ),
        McpHttpAuth::Oauth,
        false,
    )
    .expect("MCP HTTP config");
    assert!(
        McpLoginRequest::new(
            "remote".parse().expect("server name"),
            config,
            "http://127.0.0.1:9999/callback".parse().expect("redirect"),
            metadata,
            Instant::now() + Duration::from_secs(30),
        )
        .is_err()
    );
}

#[test]
fn refresh_rotates_access_and_refresh_tokens_without_changing_credential_identity() {
    let key = refresh_key("account-a");
    let credentials = seeded_credentials(&key);
    let http = OAuthHttp::with_responses(refresh_discovery_responses(
        r#"{"access_token":"new-access","refresh_token":"new-refresh","token_type":"Bearer","scope":"tools:read tools:execute","sub":"account-a"}"#,
    ));
    let result = futures::executor::block_on(ego_mcp::refresh_token(
        refresh_request(key.clone()),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
        Arc::clone(&credentials) as Arc<dyn CredentialStore>,
    ))
    .expect("refresh");
    assert_eq!(result.credential_key(), &key);
    assert_eq!(result.issuer().as_str(), "https://issuer.example.test/");
    assert_eq!(result.resource().as_str(), "https://mcp.example.test/");
    assert_eq!(result.scopes(), &["tools:execute", "tools:read"]);
    let calls = http.calls.lock().expect("OAuth calls");
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].1, McpHttpPurpose::ProtectedResourceDiscovery);
    assert_eq!(calls[1].1, McpHttpPurpose::AuthorizationServerDiscovery);
    assert_eq!(calls[2].1, McpHttpPurpose::TokenRefresh);
    assert_eq!(calls[2].2, "POST");
    assert_eq!(calls[2].3, "issuer.example.test");
    assert_eq!(calls[2].4, "/token");
    assert!(String::from_utf8_lossy(&calls[2].5).contains("grant_type=refresh_token"));
    assert!(
        String::from_utf8_lossy(&calls[2].5)
            .contains("client_id=https%3A%2F%2Fclient.example.test%2Fmetadata")
    );
    assert!(String::from_utf8_lossy(&calls[2].5).contains("refresh_token=old-refresh"));
    let stored = credentials
        .values
        .lock()
        .expect("credentials")
        .get(&key)
        .cloned()
        .expect("stored credential");
    let bundle = McpCredentialBundle::decode(SecretMaterial::new(stored)).expect("bundle");
    assert_eq!(bundle.access_token().expose_secret_bytes(), b"new-access");
    assert_eq!(
        bundle
            .refresh_token()
            .expect("refresh")
            .expose_secret_bytes(),
        b"new-refresh"
    );
    assert!(!format!("{bundle:?}").contains("new-access"));
    assert!(!format!("{bundle:?}").contains("new-refresh"));
}

#[test]
fn refresh_preserves_old_refresh_token_when_response_omits_one() {
    let key = refresh_key("account-a");
    let credentials = seeded_credentials(&key);
    let http = OAuthHttp::with_responses(refresh_discovery_responses(
        r#"{"access_token":"new-access","token_type":"Bearer","scope":"tools:read tools:execute","sub":"account-a"}"#,
    ));
    futures::executor::block_on(ego_mcp::refresh_token(
        refresh_request(key.clone()),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
        Arc::clone(&credentials) as Arc<dyn CredentialStore>,
    ))
    .expect("refresh");
    let stored = credentials
        .values
        .lock()
        .expect("credentials")
        .get(&key)
        .cloned()
        .expect("stored credential");
    let bundle = McpCredentialBundle::decode(SecretMaterial::new(stored)).expect("bundle");
    assert_eq!(bundle.access_token().expose_secret_bytes(), b"new-access");
    assert_eq!(
        bundle
            .refresh_token()
            .expect("preserved refresh")
            .expose_secret_bytes(),
        b"old-refresh"
    );
}

#[test]
fn refresh_rejects_identity_or_scope_mismatch_without_mutating_the_old_credential() {
    for (name, metadata, token) in [
        (
            "issuer",
            r#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://other.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
            r#"{"access_token":"new-access","token_type":"Bearer","scope":"tools:read tools:execute","sub":"account-a"}"#,
        ),
        (
            "resource",
            r#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://other.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
            r#"{"access_token":"new-access","token_type":"Bearer","scope":"tools:read tools:execute","sub":"account-a"}"#,
        ),
        (
            "audience",
            r#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://other.example.test","client_id_metadata_document_supported":true}"#,
            r#"{"access_token":"new-access","token_type":"Bearer","scope":"tools:read tools:execute","sub":"account-a"}"#,
        ),
        (
            "CIMD",
            r#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":false}"#,
            r#"{"access_token":"new-access","token_type":"Bearer","scope":"tools:read tools:execute","sub":"account-a"}"#,
        ),
        (
            "token endpoint",
            r#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://other.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
            r#"{"access_token":"new-access","token_type":"Bearer","scope":"tools:read tools:execute","sub":"account-a"}"#,
        ),
        (
            "account",
            r#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
            r#"{"access_token":"new-access","token_type":"Bearer","scope":"tools:read tools:execute","sub":"other-account"}"#,
        ),
        (
            "scope",
            r#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
            r#"{"access_token":"new-access","token_type":"Bearer","scope":"tools:read","sub":"account-a"}"#,
        ),
    ] {
        let key = refresh_key("account-a");
        let credentials = seeded_credentials(&key);
        let before = credentials
            .values
            .lock()
            .expect("credentials")
            .get(&key)
            .cloned();
        let responses = vec![
            McpHttpResponse::json(
                200,
                br#"{"resource":"https://mcp.example.test","authorization_servers":["https://issuer.example.test"],"scopes_supported":["tools:read","tools:execute"]}"#,
            ),
            McpHttpResponse::json(200, metadata.as_bytes()),
            McpHttpResponse::json(200, token.as_bytes()),
        ];
        let http = OAuthHttp::with_responses(responses);
        assert!(
            futures::executor::block_on(ego_mcp::refresh_token(
                refresh_request(key.clone()),
                Arc::clone(&http) as Arc<dyn McpHttpEffects>,
                Arc::clone(&credentials) as Arc<dyn CredentialStore>,
            ))
            .is_err(),
            "{name} mismatch must fail closed"
        );
        assert_eq!(
            credentials
                .values
                .lock()
                .expect("credentials")
                .get(&key)
                .cloned(),
            before
        );
    }
}

#[test]
fn refresh_rejects_missing_credential_before_network() {
    let stored_key = refresh_key("account-a");
    let requested_key = refresh_key("missing-account");
    let credentials = seeded_credentials(&stored_key);
    let http = OAuthHttp::with_responses([]);

    let result = futures::executor::block_on(ego_mcp::refresh_token(
        refresh_request(requested_key.clone()),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
        Arc::clone(&credentials) as Arc<dyn CredentialStore>,
    ));

    assert!(result.is_err());
    assert!(http.calls.lock().expect("HTTP call log").is_empty());
    assert_eq!(*credentials.puts.lock().expect("credential put counter"), 0);
    assert_eq!(
        *credentials
            .deletes
            .lock()
            .expect("credential delete counter"),
        0
    );
}

#[test]
fn refresh_rejects_corrupt_credential_before_network() {
    let key = refresh_key("account-a");
    let credentials = seeded_credentials(&key);
    credentials
        .values
        .lock()
        .expect("credential values")
        .insert(key.clone(), b"not-a-token-bundle".to_vec());
    let http = OAuthHttp::with_responses([]);

    let result = futures::executor::block_on(ego_mcp::refresh_token(
        refresh_request(key.clone()),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
        Arc::clone(&credentials) as Arc<dyn CredentialStore>,
    ));

    assert!(result.is_err());
    assert!(http.calls.lock().expect("HTTP call log").is_empty());
    assert_eq!(*credentials.puts.lock().expect("credential put counter"), 0);
    assert_eq!(
        *credentials
            .deletes
            .lock()
            .expect("credential delete counter"),
        0
    );
}

#[test]
fn refresh_rejects_missing_refresh_token_before_network() {
    let key = refresh_key("account-a");
    let credentials = seeded_credentials(&key);
    credentials
        .values
        .lock()
        .expect("credential values")
        .insert(key.clone(), encoded_bundle("old-access", None));
    let http = OAuthHttp::with_responses([]);

    let result = futures::executor::block_on(ego_mcp::refresh_token(
        refresh_request(key.clone()),
        Arc::clone(&http) as Arc<dyn McpHttpEffects>,
        Arc::clone(&credentials) as Arc<dyn CredentialStore>,
    ));

    assert!(result.is_err());
    assert!(http.calls.lock().expect("HTTP call log").is_empty());
    assert_eq!(*credentials.puts.lock().expect("credential put counter"), 0);
    assert_eq!(
        *credentials
            .deletes
            .lock()
            .expect("credential delete counter"),
        0
    );
}

#[test]
fn refresh_invalid_grant_does_not_retry_delete_or_corrupt_the_existing_credential() {
    let key = refresh_key("account-a");
    let credentials = seeded_credentials(&key);
    let before = credentials
        .values
        .lock()
        .expect("credentials")
        .get(&key)
        .cloned();
    let http = OAuthHttp::with_responses([
        McpHttpResponse::json(
            200,
            br#"{"resource":"https://mcp.example.test","authorization_servers":["https://issuer.example.test"],"scopes_supported":["tools:read","tools:execute"]}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
        ),
        McpHttpResponse::json(400, br#"{"error":"invalid_grant"}"#),
    ]);
    assert!(
        futures::executor::block_on(ego_mcp::refresh_token(
            refresh_request(key.clone()),
            Arc::clone(&http) as Arc<dyn McpHttpEffects>,
            Arc::clone(&credentials) as Arc<dyn CredentialStore>,
        ))
        .is_err()
    );
    assert_eq!(http.calls.lock().expect("OAuth calls").len(), 3);
    assert_eq!(*credentials.puts.lock().expect("credential put counter"), 0);
    assert_eq!(
        *credentials
            .deletes
            .lock()
            .expect("credential delete counter"),
        0
    );
    assert_eq!(
        credentials
            .values
            .lock()
            .expect("credentials")
            .get(&key)
            .cloned(),
        before
    );
}

#[test]
fn explicit_login_does_not_follow_an_initial_redirect_to_an_unconfigured_server() {
    let http = OAuthHttp::with_responses([McpHttpResponse::redirect(
        302,
        Vec::new(),
        b"https://unconfigured.example.test/mcp".to_vec(),
    )]);
    let interaction = Arc::new(RecordingInteraction {
        captured: Mutex::new(None),
    });
    let credentials = Arc::new(RecordingCredentials::new());
    assert!(
        futures::executor::block_on(login(
            login_request(),
            Arc::clone(&http) as Arc<dyn McpHttpEffects>,
            Arc::clone(&interaction) as Arc<dyn McpOAuthInteraction>,
            Arc::clone(&credentials) as Arc<dyn CredentialStore>,
        ))
        .is_err()
    );
    let calls = http.calls.lock().expect("OAuth calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, McpHttpPurpose::ProtectedResourceDiscovery);
    assert_eq!(calls[0].3, "mcp.example.test");
    assert!(
        interaction
            .captured
            .lock()
            .expect("authorization interaction")
            .is_none()
    );
}

#[test]
fn login_rejects_a_token_that_does_not_step_up_to_the_required_scope() {
    let http = OAuthHttp::with_responses([
        McpHttpResponse::json(
            200,
            br#"{"resource":"https://mcp.example.test","authorization_servers":["https://issuer.example.test"],"scopes_supported":["tools:execute"]}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"access_token":"opaque-access","refresh_token":"opaque-refresh","token_type":"Bearer","scope":"tools:read","sub":"account-a"}"#,
        ),
    ]);
    let interaction = Arc::new(RecordingInteraction {
        captured: Mutex::new(None),
    });
    let credentials = Arc::new(RecordingCredentials::new());
    assert!(
        futures::executor::block_on(login(
            login_request(),
            Arc::clone(&http) as Arc<dyn McpHttpEffects>,
            Arc::clone(&interaction) as Arc<dyn McpOAuthInteraction>,
            Arc::clone(&credentials) as Arc<dyn CredentialStore>,
        ))
        .is_err()
    );
    assert!(credentials.values.lock().expect("credentials").is_empty());
}

fn assert_callback_tamper_is_rejected(tamper: CallbackTamper) {
    let http = OAuthHttp::with_responses([
        McpHttpResponse::json(
            200,
            br#"{"resource":"https://mcp.example.test","authorization_servers":["https://issuer.example.test"],"scopes_supported":["tools:execute"]}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
        ),
    ]);
    let interaction = Arc::new(TamperedInteraction { tamper });
    let credentials = Arc::new(RecordingCredentials::new());
    assert!(
        futures::executor::block_on(login(
            login_request(),
            Arc::clone(&http) as Arc<dyn McpHttpEffects>,
            Arc::clone(&interaction) as Arc<dyn McpOAuthInteraction>,
            Arc::clone(&credentials) as Arc<dyn CredentialStore>,
        ))
        .is_err()
    );
    let calls = http.calls.lock().expect("OAuth calls");
    assert_eq!(calls.len(), 2, "invalid callback must not exchange a token");
    assert!(calls.iter().all(|call| call.1 != McpHttpPurpose::Token));
    assert!(credentials.values.lock().expect("credentials").is_empty());
}

#[test]
fn login_validates_callback_state_and_rfc9207_issuer_before_token_exchange() {
    assert_callback_tamper_is_rejected(CallbackTamper::State);
    assert_callback_tamper_is_rejected(CallbackTamper::Issuer);
}

#[test]
fn login_fails_closed_when_the_authorization_code_is_rejected() {
    let http = OAuthHttp::with_responses([
        McpHttpResponse::json(
            200,
            br#"{"resource":"https://mcp.example.test","authorization_servers":["https://issuer.example.test"],"scopes_supported":["tools:execute"]}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test","client_id_metadata_document_supported":true}"#,
        ),
        McpHttpResponse::json(400, br#"{"error":"invalid_grant"}"#),
    ]);
    let interaction = Arc::new(RecordingInteraction {
        captured: Mutex::new(None),
    });
    let credentials = Arc::new(RecordingCredentials::new());
    assert!(
        futures::executor::block_on(login(
            login_request(),
            Arc::clone(&http) as Arc<dyn McpHttpEffects>,
            Arc::clone(&interaction) as Arc<dyn McpOAuthInteraction>,
            Arc::clone(&credentials) as Arc<dyn CredentialStore>,
        ))
        .is_err()
    );
    let calls = http.calls.lock().expect("OAuth calls");
    assert_eq!(calls.len(), 3);
    assert!(String::from_utf8_lossy(&calls[2].5).contains("code=auth-code"));
    assert!(credentials.values.lock().expect("credentials").is_empty());
}

#[test]
fn issuer_resource_or_audience_mismatch_and_scope_step_up_fail_closed() {
    let http = OAuthHttp::with_responses([
        McpHttpResponse::json(
            200,
            br#"{"resource":"https://mcp.example.test","authorization_servers":["https://issuer.example.test"]}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://other.example.test","resource":"https://mcp.example.test","audience":"https://mcp.example.test"}"#,
        ),
    ]);
    let interaction = Arc::new(RecordingInteraction {
        captured: Mutex::new(None),
    });
    let credentials = Arc::new(RecordingCredentials::new());
    assert!(
        futures::executor::block_on(login(
            login_request(),
            Arc::clone(&http) as Arc<dyn McpHttpEffects>,
            Arc::clone(&interaction) as Arc<dyn McpOAuthInteraction>,
            Arc::clone(&credentials) as Arc<dyn CredentialStore>,
        ))
        .is_err()
    );

    let http = OAuthHttp::with_responses([
        McpHttpResponse::json(
            200,
            br#"{"resource":"https://mcp.example.test","authorization_servers":["https://issuer.example.test"]}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://other.example.test","audience":"https://mcp.example.test"}"#,
        ),
    ]);
    let interaction = Arc::new(RecordingInteraction {
        captured: Mutex::new(None),
    });
    let credentials = Arc::new(RecordingCredentials::new());
    assert!(
        futures::executor::block_on(login(
            login_request(),
            Arc::clone(&http) as Arc<dyn McpHttpEffects>,
            Arc::clone(&interaction) as Arc<dyn McpOAuthInteraction>,
            Arc::clone(&credentials) as Arc<dyn CredentialStore>,
        ))
        .is_err()
    );

    let http = OAuthHttp::with_responses([
        McpHttpResponse::json(
            200,
            br#"{"resource":"https://mcp.example.test","authorization_servers":["https://issuer.example.test"]}"#,
        ),
        McpHttpResponse::json(
            200,
            br#"{"authorization_endpoint":"https://issuer.example.test/authorize","token_endpoint":"https://issuer.example.test/token","issuer":"https://issuer.example.test","resource":"https://mcp.example.test","audience":"https://other.example.test"}"#,
        ),
    ]);
    let interaction = Arc::new(RecordingInteraction {
        captured: Mutex::new(None),
    });
    let credentials = Arc::new(RecordingCredentials::new());
    assert!(
        futures::executor::block_on(login(
            login_request(),
            Arc::clone(&http) as Arc<dyn McpHttpEffects>,
            Arc::clone(&interaction) as Arc<dyn McpOAuthInteraction>,
            Arc::clone(&credentials) as Arc<dyn CredentialStore>,
        ))
        .is_err()
    );

    let metadata = McpClientMetadata::new(
        "https://client.example.test/metadata"
            .parse()
            .expect("client metadata URL"),
        "ego MCP client",
        "https://attacker.example.test/callback"
            .parse()
            .expect("redirect URL"),
    )
    .expect("metadata shape");
    let config = McpHttpConfig::new(
        McpHttpUrl::from(
            "https://mcp.example.test/mcp"
                .parse::<url::Url>()
                .expect("MCP URL"),
        ),
        McpHttpAuth::Oauth,
        false,
    )
    .expect("MCP HTTP config");
    assert!(
        McpLoginRequest::new(
            "remote".parse().expect("server name"),
            config,
            "http://127.0.0.1:9999/callback".parse().expect("redirect"),
            metadata,
            Instant::now() + Duration::from_secs(30),
        )
        .is_err()
    );
}
