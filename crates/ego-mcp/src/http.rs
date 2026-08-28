use std::collections::BTreeSet;
use std::time::Instant;

use ego_core::config::McpServerName;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{HttpBody, HttpHeader, HttpRequest, HttpRoute, HttpScheme, NetworkTarget};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{McpError, McpFailure, McpHttpPurpose, PreparedMcpHttpCall};

pub type McpHttpBody = HttpBody;

pub enum McpHttpResponse {
    Response {
        status: u16,
        headers: Vec<HttpHeader>,
        body: McpHttpBody,
        delivery: DeliveryCertainty,
    },
    Redirect {
        status: u16,
        headers: Vec<HttpHeader>,
        location: Vec<u8>,
        delivery: DeliveryCertainty,
    },
}

impl McpHttpResponse {
    pub fn response(
        status: u16,
        headers: Vec<HttpHeader>,
        body: McpHttpBody,
        delivery: DeliveryCertainty,
    ) -> Result<Self, McpError> {
        if !(100..=599).contains(&status) {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid HTTP status",
            ));
        }
        Ok(Self::Response {
            status,
            headers,
            body,
            delivery,
        })
    }

    pub fn json(status: u16, body: impl AsRef<[u8]>) -> Self {
        let headers = vec![
            HttpHeader::new("content-type", b"application/json".to_vec())
                .expect("constant header is valid"),
        ];
        Self::Response {
            status,
            headers,
            body: Box::pin(futures::stream::once(futures::future::ready(Ok(body
                .as_ref()
                .to_vec())))),
            delivery: DeliveryCertainty::PossiblySent,
        }
    }

    pub fn redirect(status: u16, headers: Vec<HttpHeader>, location: Vec<u8>) -> Self {
        Self::Redirect {
            status,
            headers,
            location,
            delivery: DeliveryCertainty::PossiblySent,
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Response { status, .. } | Self::Redirect { status, .. } => *status,
        }
    }

    pub fn headers(&self) -> &[HttpHeader] {
        match self {
            Self::Response { headers, .. } | Self::Redirect { headers, .. } => headers,
        }
    }

    pub(crate) fn header_value(&self, name: &str) -> Option<&[u8]> {
        self.headers()
            .iter()
            .find(|header| header.name().eq_ignore_ascii_case(name))
            .map(HttpHeader::value)
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        match self {
            Self::Response { delivery, .. } | Self::Redirect { delivery, .. } => *delivery,
        }
    }

    pub fn location(&self) -> Option<&[u8]> {
        match self {
            Self::Redirect { location, .. } => Some(location),
            Self::Response { .. } => None,
        }
    }

    pub fn into_body(self) -> Option<McpHttpBody> {
        match self {
            Self::Response { body, .. } => Some(body),
            Self::Redirect { .. } => None,
        }
    }

    pub(crate) async fn into_bytes(self, limit: usize) -> Result<Vec<u8>, McpError> {
        let mut body = self
            .into_body()
            .ok_or_else(|| McpError::new(McpFailure::InvalidResponse, "redirect has no body"))?;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(McpError::new(
                    McpFailure::InvalidResponse,
                    "HTTP response exceeded the configured bound",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

#[derive(Clone)]
pub(crate) struct AdmittedMcpHttpProfile {
    fingerprint: [u8; 32],
    routes: BTreeSet<String>,
}

impl AdmittedMcpHttpProfile {
    pub(crate) fn initial(server: &McpServerName, url: &Url) -> Result<Self, McpError> {
        let mut routes = BTreeSet::new();
        routes.insert(origin(url)?);
        Self::from_routes(server, routes)
    }

    pub(crate) fn admitting(&self, server: &McpServerName, url: &Url) -> Result<Self, McpError> {
        let mut routes = self.routes.clone();
        routes.insert(origin(url)?);
        Self::from_routes(server, routes)
    }

    fn from_routes(server: &McpServerName, routes: BTreeSet<String>) -> Result<Self, McpError> {
        let mut digest = Sha256::new();
        digest.update(b"ego:mcp:http-profile:v1");
        digest.update(server.as_str().as_bytes());
        for route in &routes {
            digest.update((route.len() as u64).to_be_bytes());
            digest.update(route.as_bytes());
        }
        Ok(Self {
            fingerprint: digest.finalize().into(),
            routes,
        })
    }

    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub(crate) fn route(&self, url: &Url) -> Result<HttpRoute, McpError> {
        if !self.routes.contains(&origin(url)?) {
            return Err(McpError::new(
                McpFailure::Authority,
                "HTTP route is not admitted",
            ));
        }
        route(url)
    }

    pub(crate) fn prepare(
        &self,
        server: &McpServerName,
        purpose: McpHttpPurpose,
        url: &Url,
        request: HttpRequest,
        deadline: Instant,
    ) -> Result<PreparedMcpHttpCall, McpError> {
        self.route(url)?;
        if request.path() != path_and_query(url) {
            return Err(McpError::new(
                McpFailure::Authority,
                "HTTP request path does not match its admitted URL",
            ));
        }
        let route = self.route(url)?;
        Ok(PreparedMcpHttpCall::prepared(
            self.fingerprint,
            server.clone(),
            purpose,
            route,
            request,
            deadline,
        ))
    }
}

fn route(url: &Url) -> Result<HttpRoute, McpError> {
    let scheme = match url.scheme() {
        "https" => HttpScheme::Https,
        "http" => HttpScheme::Http,
        _ => {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "unsupported HTTP scheme",
            ));
        }
    };
    let host = url
        .host_str()
        .ok_or_else(|| McpError::new(McpFailure::InvalidValue, "HTTP URL has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| McpError::new(McpFailure::InvalidValue, "HTTP URL has no port"))?;
    let target = NetworkTarget::new(host, port)
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))?;
    HttpRoute::new(scheme, target)
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))
}

fn origin(url: &Url) -> Result<String, McpError> {
    let route = route(url)?;
    Ok(format!("{:?}://{}", route.scheme(), route.target()))
}

pub(crate) fn path_and_query(url: &Url) -> String {
    let mut path = url.path().to_owned();
    if path.is_empty() {
        path.push('/');
    }
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    path
}

pub(crate) fn header(name: &str, value: impl Into<Vec<u8>>) -> Result<HttpHeader, McpError> {
    HttpHeader::new(name, value.into())
        .map_err(|error| McpError::new(McpFailure::InvalidValue, error.to_string()))
}
