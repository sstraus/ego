//! Current MCP 2026-07-28 protocol ownership and guarded transport adapters.

mod client;
mod effects;
mod error;
mod http;
mod oauth;
mod server;
mod state;
mod stdio;

pub use client::{
    MCP_PROTOCOL_REVISION, McpClient, McpClientCancellation, McpClientOptions, McpClientSettlement,
};
pub use effects::{
    McpDuplexEffects, McpFuture, McpHttpEffects, McpHttpEnd, McpHttpPurpose, McpHttpSettlement,
    McpPreparedHttpParts, McpRequestId, PreparedMcpHttpCall,
};
pub use ego_core::generator::attempt::DeliveryCertainty;
pub use error::{McpError, McpFailure};
pub use http::{McpHttpBody, McpHttpResponse};
pub use oauth::{
    McpAuthorizationInteraction, McpClientMetadata, McpCredentialBundle, McpLoginRequest,
    McpLoginResult, McpOAuthCallback, McpOAuthInteraction, McpPkceContext, McpPkceMethod,
    McpTokenRefreshRequest, McpTokenRefreshResult, login, refresh_token,
};
pub use server::{McpServerPort, McpServerToolCall, serve_stdio};
pub use state::{
    CacheScope, McpInputRequestId, McpInputResponse, McpInputState, McpRefreshRequest,
    McpResultMetadata, McpRetryRequest, McpSubscription, McpSubscriptionEvent, McpToolResult,
};
