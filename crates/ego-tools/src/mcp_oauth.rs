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
            read_callback(&self.listener, &self.redirect, self.deadline).await
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
