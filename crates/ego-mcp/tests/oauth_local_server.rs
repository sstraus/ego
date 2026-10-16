//! OAuth wire contract against a local authorization server (no real credentials).
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ego_core::config::{McpHttpAuth, McpHttpConfig};
use ego_core::credentials::{
    CredentialKey, CredentialRemoval, CredentialStore, CredentialStoreError, SecretMaterial,
};
use ego_mcp::*;
use url::Url;

struct LocalHttp;
impl McpHttpEffects for LocalHttp {
    fn execute(
        &self,
        call: PreparedMcpHttpCall,
    ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
        Box::pin(async move {
            let mut stream =
                TcpStream::connect((call.route().target().host(), call.route().target().port()))
                    .unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let request = call.request();
            write!(stream, "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", request.method(), request.path(), request.body().len()).unwrap();
            stream.write_all(request.body()).unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            let separator = response
                .windows(4)
                .position(|value| value == b"\r\n\r\n")
                .unwrap();
            let status = std::str::from_utf8(&response[..separator])
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse()
                .unwrap();
            Ok(McpHttpResponse::json(status, &response[separator + 4..]))
        })
    }
    fn cancel(&self, id: McpRequestId) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        settlement(id, McpHttpEnd::Cancelled)
    }
    fn timeout(&self, id: McpRequestId) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        settlement(id, McpHttpEnd::TimedOut)
    }
    fn settle(&self, id: McpRequestId) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        settlement(id, McpHttpEnd::Completed)
    }
}
fn settlement(
    id: McpRequestId,
    end: McpHttpEnd,
) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
    Box::pin(async move { McpHttpSettlement::new(id, end, DeliveryCertainty::PossiblySent) })
}
#[derive(Default)]
struct Store(Mutex<HashMap<CredentialKey, Vec<u8>>>);
impl CredentialStore for Store {
    fn get(&self, key: &CredentialKey) -> Result<Option<SecretMaterial>, CredentialStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .map(SecretMaterial::new))
    }
    fn put(&self, key: &CredentialKey, value: SecretMaterial) -> Result<(), CredentialStoreError> {
        self.0
            .lock()
            .unwrap()
            .insert(key.clone(), value.expose_secret_bytes().to_vec());
        Ok(())
    }
    fn delete(&self, key: &CredentialKey) -> Result<CredentialRemoval, CredentialStoreError> {
        Ok(if self.0.lock().unwrap().remove(key).is_some() {
            CredentialRemoval::Removed
        } else {
            CredentialRemoval::Absent
        })
    }
}
struct UserAgent;
impl McpOAuthInteraction for UserAgent {
    fn authorize<'a>(
        &'a self,
        interaction: McpAuthorizationInteraction,
    ) -> McpFuture<'a, Result<McpOAuthCallback, McpError>> {
        Box::pin(async move {
            let url = interaction.authorization_url();
            let mut stream =
                TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).unwrap();
            write!(
                stream,
                "GET {}?{} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                url.path(),
                url.query().unwrap()
            )
            .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            let callback = response
                .lines()
                .find_map(|line| line.strip_prefix("Location: "))
                .unwrap()
                .parse()
                .unwrap();
            McpOAuthCallback::from_callback_url(callback)
        })
    }
}

fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 1024];
        let count = stream.read(&mut chunk).unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let size: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap_or("0")
                .parse()
                .unwrap();
            if bytes.len() >= end + 4 + size {
                return (
                    headers.lines().next().unwrap().into(),
                    bytes[end + 4..].to_vec(),
                );
            }
        }
    }
}
fn respond(stream: &mut TcpStream, body: serde_json::Value) {
    let body = serde_json::to_vec(&body).unwrap();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
    stream.write_all(&body).unwrap();
}

#[test]
fn local_authorization_server_verifies_registered_pkce_and_refresh_on_wire() {
    use sha2::{Digest, Sha256};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server_origin = origin.clone();
    // Replay the recorded public Linear discovery shape at a local AS. Only
    // deployment addresses change; registration and advertised policy stay.
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/oauth/linear.json")).unwrap();
    let mut protected = fixture["protected_resource"].clone();
    protected["resource"] = serde_json::json!(format!("{origin}/mcp"));
    protected["authorization_servers"] = serde_json::json!([origin]);
    let mut metadata = fixture["authorization_server"].clone();
    for key in [
        "issuer",
        "authorization_endpoint",
        "token_endpoint",
        "registration_endpoint",
        "revocation_endpoint",
        "resource",
        "resource_metadata",
    ] {
        if let Some(value) = metadata[key].as_str() {
            metadata[key] = serde_json::json!(value.replace("https://mcp.linear.app", &origin));
        }
    }
    let server = std::thread::spawn(move || {
        let mut challenge = String::new();
        for _ in 0..7 {
            let (mut socket, _) = listener.accept().unwrap();
            let (request, body) = read_request(&mut socket);
            let path = request.split_whitespace().nth(1).unwrap();
            if path.starts_with("/.well-known/oauth-protected-resource") {
                respond(&mut socket, protected.clone());
            } else if path == "/.well-known/oauth-authorization-server" {
                respond(&mut socket, metadata.clone());
            } else if path.starts_with("/authorize?") {
                let url: Url = format!("{server_origin}{path}").parse().unwrap();
                let fields: HashMap<_, _> = url.query_pairs().into_owned().collect();
                assert_eq!(fields["client_id"], "operator-client");
                assert_eq!(fields["code_challenge_method"], "S256");
                challenge = fields["code_challenge"].clone();
                let mut callback: Url = fields["redirect_uri"].parse().unwrap();
                callback
                    .query_pairs_mut()
                    .append_pair("code", "local-code")
                    .append_pair("state", &fields["state"])
                    .append_pair("iss", &server_origin);
                write!(socket, "HTTP/1.1 302 Found\r\nLocation: {callback}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                assert_eq!(path, "/token");
                let fields: HashMap<_, _> =
                    url::form_urlencoded::parse(&body).into_owned().collect();
                assert_eq!(fields["client_id"], "operator-client");
                if fields["grant_type"] == "authorization_code" {
                    assert_eq!(fields["code"], "local-code");
                    let hash = Sha256::digest(fields["code_verifier"].as_bytes());
                    // Independent RFC 4648 encoder in the test server.
                    let alphabet =
                        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
                    let mut encoded = String::new();
                    for chunk in hash.chunks(3) {
                        let n = (u32::from(chunk[0]) << 16)
                            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                            | u32::from(*chunk.get(2).unwrap_or(&0));
                        for shift in [18, 12, 6, 0].into_iter().take(chunk.len() + 1) {
                            encoded.push(alphabet[((n >> shift) & 63) as usize] as char);
                        }
                    }
                    assert_eq!(
                        challenge, encoded,
                        "a code verifier must match its authorization challenge"
                    );
                } else {
                    assert_eq!(fields["refresh_token"], "local-refresh");
                }
                respond(
                    &mut socket,
                    serde_json::json!({"access_token":"local-access","refresh_token":"local-refresh","token_type":"Bearer","scope":"read write"}),
                );
            }
        }
    });
    let config = McpHttpConfig::new(
        format!("{origin}/mcp").parse::<Url>().unwrap(),
        McpHttpAuth::Oauth,
        true,
    )
    .unwrap();
    let metadata = || {
        McpClientMetadata::registered(
            "operator-client".into(),
            "http://127.0.0.1:9999/callback".parse().unwrap(),
        )
        .unwrap()
    };
    let store = Arc::new(Store::default());
    let request = McpLoginRequest::new(
        "local".parse().unwrap(),
        config.clone(),
        "http://127.0.0.1:9999/callback".parse().unwrap(),
        metadata(),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    let result = futures::executor::block_on(login(
        request,
        Arc::new(LocalHttp),
        Arc::new(UserAgent),
        store.clone(),
    ))
    .unwrap();
    let refresh = McpTokenRefreshRequest::new(
        "local".parse().unwrap(),
        config,
        metadata(),
        result.credential_key().clone(),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    futures::executor::block_on(refresh_token(refresh, Arc::new(LocalHttp), store.clone()))
        .unwrap();
    assert_eq!(store.0.lock().unwrap().len(), 1);
    server.join().unwrap();
}
