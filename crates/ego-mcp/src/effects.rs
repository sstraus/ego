use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ego_core::config::McpServerName;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{
    DuplexHandle, HttpHeader, HttpRequest, HttpRoute, PreparedConnect, SpawnSpec,
};

use crate::{McpError, McpFailure, McpHttpResponse};

pub type McpFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpHttpPurpose {
    Protocol,
    ProtectedResourceDiscovery,
    AuthorizationServerDiscovery,
    ClientMetadata,
    Authorization,
    Token,
    TokenRefresh,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct McpRequestId(u64);

impl std::fmt::Debug for McpRequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mcp-request-{}", self.0)
    }
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpHttpEnd {
    Completed,
    Cancelled,
    TimedOut,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpHttpSettlement {
    request_id: McpRequestId,
    end: McpHttpEnd,
    delivery: DeliveryCertainty,
}

impl McpHttpSettlement {
    pub fn new(
        request_id: McpRequestId,
        end: McpHttpEnd,
        delivery: DeliveryCertainty,
    ) -> Result<Self, McpError> {
        if end == McpHttpEnd::Completed && delivery == DeliveryCertainty::DefinitelyUnsent {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "a completed MCP HTTP request cannot be definitely unsent",
            ));
        }
        Ok(Self {
            request_id,
            end,
            delivery,
        })
    }

    pub fn request_id(&self) -> &McpRequestId {
        &self.request_id
    }

    pub fn end(&self) -> McpHttpEnd {
        self.end
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
}

pub struct PreparedMcpHttpCall {
    request_id: McpRequestId,
    profile_fingerprint: [u8; 32],
    server: McpServerName,
    purpose: McpHttpPurpose,
    route: HttpRoute,
    connect: PreparedConnect,
    request: HttpRequest,
    deadline: Instant,
}

impl PreparedMcpHttpCall {
    pub(crate) fn prepared(
        profile_fingerprint: [u8; 32],
        server: McpServerName,
        purpose: McpHttpPurpose,
        route: HttpRoute,
        request: HttpRequest,
        deadline: Instant,
    ) -> Self {
        let connect = PreparedConnect::new(route.target().clone(), &request);
        Self {
            request_id: McpRequestId(NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)),
            profile_fingerprint,
            server,
            purpose,
            route,
            connect,
            request,
            deadline,
        }
    }

    pub fn request_id(&self) -> &McpRequestId {
        &self.request_id
    }

    pub fn server(&self) -> &McpServerName {
        &self.server
    }

    pub fn purpose(&self) -> McpHttpPurpose {
        self.purpose
    }

    pub fn route(&self) -> &HttpRoute {
        &self.route
    }

    pub fn request(&self) -> &HttpRequest {
        &self.request
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) fn with_headers(mut self, headers: &[HttpHeader]) -> Result<Self, McpError> {
        if headers.is_empty() {
            return Ok(self);
        }
        let mut combined = self.request.headers().to_vec();
        combined.extend_from_slice(headers);
        self.request = HttpRequest::new(
            self.request.method(),
            self.request.path(),
            combined,
            self.request.body().to_vec(),
        )
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
        self.connect = PreparedConnect::new(self.route.target().clone(), &self.request);
        Ok(self)
    }

    pub fn into_parts(self) -> McpPreparedHttpParts {
        McpPreparedHttpParts {
            request_id: self.request_id,
            profile_fingerprint: self.profile_fingerprint,
            server: self.server,
            purpose: self.purpose,
            route: self.route,
            connect: self.connect,
            request: self.request,
            deadline: self.deadline,
        }
    }
}

pub struct McpPreparedHttpParts {
    pub request_id: McpRequestId,
    pub profile_fingerprint: [u8; 32],
    pub server: McpServerName,
    pub purpose: McpHttpPurpose,
    pub route: HttpRoute,
    pub connect: PreparedConnect,
    pub request: HttpRequest,
    pub deadline: Instant,
}

pub trait McpHttpEffects: Send + Sync {
    fn execute(
        &self,
        call: PreparedMcpHttpCall,
    ) -> McpFuture<'static, Result<McpHttpResponse, McpError>>;

    fn cancel(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>>;

    fn timeout(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>>;

    fn settle(
        &self,
        request_id: McpRequestId,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>>;
}

pub trait McpDuplexEffects: Send {
    fn open(
        self: Box<Self>,
        server: McpServerName,
    ) -> McpFuture<'static, Result<DuplexHandle, McpError>>;
}

/// Consumer-owned factory for one fresh management/discovery duplex.
pub trait McpDuplexEffectsFactory: Send + Sync {
    fn create(
        &self,
        server: McpServerName,
        spec: SpawnSpec,
    ) -> Result<Box<dyn McpDuplexEffects>, McpError>;
}
