//! Bounded loopback interaction; protocol correlation stays in ego-mcp.
use std::time::Instant;

use ego_mcp::{
    McpAuthorizationInteraction, McpError, McpFailure, McpFuture, McpOAuthCallback,
    McpOAuthInteraction,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

pub struct LoopbackOAuthInteraction {
    listener: TcpListener,
    redirect: Url,
    deadline: Instant,
}

impl LoopbackOAuthInteraction {
    pub async fn bind(port: u16, deadline: Instant) -> Result<Self, McpError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|_| failure("OAuth loopback listener could not bind"))?;
        let port = listener
            .local_addr()
            .map_err(|_| failure("OAuth callback address unavailable"))?
            .port();
        let redirect = format!("http://127.0.0.1:{port}/callback")
            .parse()
            .map_err(|_| failure("invalid OAuth callback address"))?;
        Ok(Self {
            listener,
            redirect,
            deadline,
        })
    }

    pub async fn receive_callback(&self) -> Result<McpOAuthCallback, McpError> {
        read_callback(&self.listener, &self.redirect, self.deadline).await
    }

    pub fn redirect_uri(&self) -> &Url {
        &self.redirect
    }
}

impl McpOAuthInteraction for LoopbackOAuthInteraction {
    fn authorize<'a>(
        &'a self,
        interaction: McpAuthorizationInteraction,
    ) -> McpFuture<'a, Result<McpOAuthCallback, McpError>> {
        Box::pin(async move {
            if interaction.redirect_uri() != &self.redirect {
                return Err(failure("OAuth interaction redirect mismatch"));
            }
            // This URL is intended for the human, not model or diagnostic logs.
            println!("Authorize: {}", interaction.authorization_url());
            println!("Callback: {}", self.redirect);
            self.receive_callback().await
        })
    }
}

fn failure(message: &str) -> McpError {
    McpError::new(McpFailure::Authorization, message)
}

async fn read_callback(
    listener: &TcpListener,
    redirect: &Url,
    deadline: Instant,
) -> Result<McpOAuthCallback, McpError> {
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), async {
                let (mut socket, _) = listener.accept().await.map_err(|_| failure("OAuth callback accept failed"))?;
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0u8; 512];
                    let size = socket.read(&mut chunk).await.map_err(|_| failure("OAuth callback read failed"))?;
                    if size == 0 || bytes.len() + size > 8192 { return Err(failure("OAuth callback headers invalid")); }
                    bytes.extend_from_slice(&chunk[..size]);
                    if bytes.windows(4).any(|value| value == b"\r\n\r\n") { break; }
                }
                let text = std::str::from_utf8(&bytes).map_err(|_| failure("OAuth callback encoding invalid"))?;
                let mut request = text.lines().next().unwrap_or_default().split_whitespace();
                let method = request.next();
                let path = request.next().ok_or_else(|| failure("OAuth callback path absent"))?;
                if method != Some("GET") || !path.starts_with("/callback?") || request.next() != Some("HTTP/1.1") || request.next().is_some() {
                    return Err(failure("OAuth callback request invalid"));
                }
                let mut callback = redirect.clone();
                callback.set_query(path.split_once('?').map(|(_, query)| query));
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 49\r\nConnection: close\r\n\r\nCallback received. Return to ego for login status.\n").await;
                McpOAuthCallback::from_callback_url(callback)
    }).await.map_err(|_| failure("OAuth callback timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn loopback_rejects_wrong_path_and_incomplete_headers_by_deadline() {
        for payload in [
            b"GET /wrong?code=secret HTTP/1.1\r\n\r\n".as_slice(),
            b"GET /callback?code=secret HTTP/1.1\r\n".as_slice(),
        ] {
            let deadline = Instant::now() + Duration::from_millis(150);
            let interaction = LoopbackOAuthInteraction::bind(0, deadline).await.unwrap();
            let address = interaction.listener.local_addr().unwrap();
            let sender = async {
                let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
                socket.write_all(payload).await.unwrap();
                tokio::time::sleep(Duration::from_millis(200)).await;
            };
            let reader = read_callback(&interaction.listener, &interaction.redirect, deadline);
            let (result, ()) = tokio::join!(reader, sender);
            assert!(
                result.is_err(),
                "malformed or incomplete callback must not authorize"
            );
        }
    }

    #[tokio::test]
    async fn loopback_returns_exact_callback_without_logging_code() {
        let deadline = Instant::now() + Duration::from_secs(2);
        let interaction = LoopbackOAuthInteraction::bind(0, deadline).await.unwrap();
        let address = interaction.listener.local_addr().unwrap();
        let sender = async {
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            socket.write_all(b"GET /callback?code=test-code&state=test-state HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
            let mut response = Vec::new();
            socket.read_to_end(&mut response).await.unwrap();
            assert!(!String::from_utf8_lossy(&response).contains("test-code"));
        };
        let (result, ()) = tokio::join!(
            read_callback(&interaction.listener, &interaction.redirect, deadline),
            sender
        );
        assert_eq!(
            result.unwrap().as_url().query(),
            Some("code=test-code&state=test-state")
        );
    }
}

/// Non-secret selection record. Tokens stay in the injected credential store.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthLocator {
    resource: String,
    issuer: String,
    account: Option<String>,
}

pub fn persist_oauth_locator(
    root: &std::path::Path,
    name: &ego_core::config::McpServerName,
    key: &ego_core::credentials::CredentialKey,
) -> Result<(), ego_mcp::McpError> {
    use crate::store::StoreIo;
    let ego_core::credentials::CredentialTarget::ProtectedResource(resource) = key.target() else {
        return Err(locator_error());
    };
    std::fs::create_dir_all(root).map_err(|_| locator_error())?;
    crate::privacy::harden_private_path(root, crate::privacy::PrivatePathKind::Directory)
        .map_err(|_| locator_error())?;
    let locator = OAuthLocator {
        resource: resource.clone(),
        issuer: key.issuer().into(),
        account: key.account().map(str::to_owned),
    };
    let bytes = serde_json::to_vec(&locator).map_err(|_| locator_error())?;
    crate::store::OsIo
        .replace_atomic_private(&locator_path(root, name), &bytes)
        .map_err(|_| locator_error())
}

fn locator_path(
    root: &std::path::Path,
    name: &ego_core::config::McpServerName,
) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    root.join(format!(
        "oauth-{:x}.json",
        Sha256::digest(name.as_str().as_bytes())
    ))
}

fn locator_error() -> ego_mcp::McpError {
    ego_mcp::McpError::new(
        ego_mcp::McpFailure::Credential,
        "MCP OAuth locator unavailable; run ego mcp login NAME",
    )
}

pub fn oauth_session(
    root: &std::path::Path,
    name: &ego_core::config::McpServerName,
    config: &ego_core::config::McpHttpConfig,
    credentials: std::sync::Arc<dyn ego_core::credentials::CredentialStore>,
) -> Result<std::sync::Arc<ego_mcp::McpOAuthSession>, ego_mcp::McpError> {
    let bytes = std::fs::read(locator_path(root, name)).map_err(|_| locator_error())?;
    if bytes.len() > 16 * 1024 {
        return Err(locator_error());
    }
    let locator: OAuthLocator = serde_json::from_slice(&bytes).map_err(|_| locator_error())?;
    let key = ego_core::credentials::CredentialKey::new(
        locator.issuer,
        ego_core::credentials::CredentialTarget::ProtectedResource(locator.resource),
        locator.account,
    )
    .map_err(|_| locator_error())?;
    ego_mcp::McpOAuthSession::new(name.clone(), config.clone(), key, credentials).map(|session| {
        std::sync::Arc::new(
            session.with_refresh_lock(std::sync::Arc::new(OAuthRefreshLock(
                root.join("oauth-refresh.lock"),
            ))),
        )
    })
}

struct OAuthRefreshLock(std::path::PathBuf);
struct OAuthRefreshLease(std::fs::File);
impl ego_mcp::McpOAuthRefreshLease for OAuthRefreshLease {}
impl Drop for OAuthRefreshLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
impl ego_mcp::McpOAuthRefreshLock for OAuthRefreshLock {
    fn acquire(&self) -> Result<Box<dyn ego_mcp::McpOAuthRefreshLease>, ego_mcp::McpError> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.0)
            .map_err(|_| locator_error())?;
        file.try_lock().map_err(|_| {
            ego_mcp::McpError::new(
                ego_mcp::McpFailure::Credential,
                "another MCP OAuth refresh is active; retry after it completes",
            )
        })?;
        Ok(Box::new(OAuthRefreshLease(file)))
    }
}

#[cfg(test)]
mod locator_tests {
    use super::*;
    use ego_core::credentials::{CredentialKey, CredentialTarget};
    use std::sync::Arc;

    #[test]
    fn restart_locator_retains_exact_resource_and_rejects_server_substitution() {
        // Catches restart losing the account/issuer and same-origin token leakage.
        let root = tempfile::tempdir().unwrap();
        let name = "linear".parse().unwrap();
        let key = CredentialKey::new(
            "https://mcp.linear.app/".into(),
            CredentialTarget::ProtectedResource("https://mcp.linear.app/mcp".into()),
            Some("account".into()),
        )
        .unwrap();
        persist_oauth_locator(root.path(), &name, &key).unwrap();
        let bytes = std::fs::read(locator_path(root.path(), &name)).unwrap();
        let locator: OAuthLocator = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(locator.account.as_deref(), Some("account"));
        assert_eq!(locator.issuer, key.issuer());
        let config = |path| {
            ego_core::config::McpHttpConfig::new(
                format!("https://mcp.linear.app/{path}")
                    .parse::<url::Url>()
                    .unwrap(),
                ego_core::config::McpHttpAuth::Oauth,
                false,
            )
            .unwrap()
        };
        // Construction never reads the native store. This adapter is not used.
        let credentials = Arc::new(crate::credentials::OsCredentialStore::default());
        assert!(oauth_session(root.path(), &name, &config("mcp"), credentials.clone()).is_ok());
        assert!(oauth_session(root.path(), &name, &config("other"), credentials).is_err());
    }

    #[test]
    fn refresh_lock_excludes_concurrent_cli_and_runtime_transactions() {
        // Catches independent lock domains permitting rotated-token reuse.
        use ego_mcp::McpOAuthRefreshLock;
        let root = tempfile::tempdir().unwrap();
        let lock = OAuthRefreshLock(root.path().join("oauth-refresh.lock"));
        let first = lock.acquire().unwrap();
        assert!(lock.acquire().is_err());
        drop(first);
        assert!(lock.acquire().is_ok());
    }
}
