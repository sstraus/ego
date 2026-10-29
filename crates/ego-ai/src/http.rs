//! Provider-owned HTTP admission over the run-owned one-hop transport.
//!
//! `ego-ai` owns logical calls, URLs, redirects and provider profiles. The
//! injected [`Http`] port owns no provider knowledge: one call handle spends
//! one freshly prepared capability at a time and keeps the run's absolute
//! deadline across every admitted redirect.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use ego_core::credentials::SecretMaterial;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::types::ErrorKind;
use ego_core::permit::{
    HttpHeader, HttpRequest as CoreHttpRequest, HttpRequestDigest, HttpRoute, HttpScheme,
    NetworkTarget, OneHopResponse, PreparedConnect, TransportError, TransportFailure,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use sha2::{Digest, Sha256};
use url::Url;

const HTTP_PROFILE_FINGERPRINT_DOMAIN: &[u8] = b"ego.http-profile";
const HTTP_PROFILE_POLICY_VERSION: &[u8] = b"1";

/// The identity of one exact, versioned set of admitted HTTP routes.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpProfileFingerprint {
    id: String,
    digest: [u8; 32],
}

impl HttpProfileFingerprint {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Debug for HttpProfileFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpProfileFingerprint")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// A reusable policy table private to one provider or auxiliary endpoint.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AdmittedHttpProfile {
    fingerprint: HttpProfileFingerprint,
    routes: Vec<HttpRoute>,
}

impl fmt::Debug for AdmittedHttpProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmittedHttpProfile")
            .field("fingerprint", &self.fingerprint)
            .field("routes", &self.routes)
            .finish()
    }
}

/// A single admitted HTTP hop, bound to its exact target and request bytes.
///
/// Deliberately non-`Clone`: the outer adapter consumes the authority.
pub struct PreparedHttpCall {
    profile: HttpProfileFingerprint,
    route: HttpRoute,
    connect: PreparedConnect,
    request: CoreHttpRequest,
}

impl PreparedHttpCall {
    pub fn profile(&self) -> &HttpProfileFingerprint {
        &self.profile
    }

    pub fn route(&self) -> &HttpRoute {
        &self.route
    }

    pub fn request_digest(&self) -> &HttpRequestDigest {
        self.connect.request_digest()
    }

    pub fn into_parts(
        self,
    ) -> (
        HttpProfileFingerprint,
        HttpRoute,
        PreparedConnect,
        CoreHttpRequest,
    ) {
        (self.profile, self.route, self.connect, self.request)
    }
}

/// Why a profile or one attempted use of it is invalid.
///
/// No variant carries URL, header, body or request-target material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpProfileError {
    EmptyId,
    EmptyRoutes,
    DuplicateRoute,
    UnsupportedScheme,
    MissingHost,
    MissingPort,
    UserInfo,
    Fragment,
    RouteNotAdmitted,
    RequestTargetMismatch,
    InvalidTarget,
    InvalidRequest,
    InvalidRedirect,
    RedirectCycle,
}

impl fmt::Display for HttpProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyId => "HTTP profile ID is empty",
            Self::EmptyRoutes => "HTTP profile has no admitted routes",
            Self::DuplicateRoute => "HTTP profile contains a duplicate route",
            Self::UnsupportedScheme => "HTTP profile call uses an unsupported scheme",
            Self::MissingHost => "HTTP profile call has no host",
            Self::MissingPort => "HTTP profile call has no effective port",
            Self::UserInfo => "HTTP profile call contains user information",
            Self::Fragment => "HTTP profile call contains a fragment",
            Self::RouteNotAdmitted => "HTTP profile does not admit the requested route",
            Self::RequestTargetMismatch => "HTTP profile URL and request target do not match",
            Self::InvalidTarget => "HTTP profile call contains an invalid target",
            Self::InvalidRequest => "HTTP profile call contains an invalid request",
            Self::InvalidRedirect => "HTTP profile received an invalid redirect location",
            Self::RedirectCycle => "HTTP profile detected a redirect cycle",
        })
    }
}

impl std::error::Error for HttpProfileError {}

impl AdmittedHttpProfile {
    pub(crate) fn try_new(id: &str, mut routes: Vec<HttpRoute>) -> Result<Self, HttpProfileError> {
        if id.is_empty() {
            return Err(HttpProfileError::EmptyId);
        }
        if routes.is_empty() {
            return Err(HttpProfileError::EmptyRoutes);
        }
        routes.sort_by(compare_http_routes);
        if routes.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(HttpProfileError::DuplicateRoute);
        }
        Ok(Self {
            fingerprint: fingerprint_http_profile(id, &routes, &[]),
            routes,
        })
    }

    pub(crate) fn from_urls(id: &str, urls: &[Url]) -> Result<Self, HttpProfileError> {
        let mut bindings = urls
            .iter()
            .map(|url| url.as_str().as_bytes().to_vec())
            .collect::<Vec<_>>();
        bindings.sort();
        let mut profile = urls
            .iter()
            .map(route_from_url)
            .collect::<Result<Vec<_>, _>>()
            .and_then(|routes| Self::try_new(id, routes))?;
        profile.fingerprint = fingerprint_http_profile(id, &profile.routes, &bindings);
        Ok(profile)
    }

    pub(crate) fn prepare(
        &self,
        url: Url,
        request: CoreHttpRequest,
    ) -> Result<PreparedHttpCall, HttpProfileError> {
        let route = route_from_url(&url)?;
        if self
            .routes
            .binary_search_by(|item| compare_http_routes(item, &route))
            .is_err()
        {
            return Err(HttpProfileError::RouteNotAdmitted);
        }

        let mut url_target = url.path().to_owned();
        if let Some(query) = url.query() {
            url_target.push('?');
            url_target.push_str(query);
        }
        if url_target != request.path() {
            return Err(HttpProfileError::RequestTargetMismatch);
        }

        let connect = PreparedConnect::new(route.target().clone(), &request);
        Ok(PreparedHttpCall {
            profile: self.fingerprint.clone(),
            route,
            connect,
            request,
        })
    }

    /// Execute one logical request, retaining one handle and fresh authority
    /// across every admitted redirect.
    pub(crate) async fn execute(
        &self,
        http: &dyn Http,
        request: HttpRequest,
    ) -> Result<HttpResponse, HttpError> {
        let mut call = http.begin_call();
        let mut request = request;
        let mut identities = HashSet::new();
        let mut prior_effect = false;

        loop {
            let core = request
                .to_core()
                .map_err(|failure| HttpError::profile(failure, delivery_after(prior_effect)))?;
            let prepared = self
                .prepare(request.url.clone(), core)
                .map_err(|failure| HttpError::profile(failure, delivery_after(prior_effect)))?;
            let identity = (prepared.route().clone(), prepared.request_digest().clone());
            if !identities.insert(identity) {
                return Err(HttpError::profile(
                    HttpProfileError::RedirectCycle,
                    delivery_after(prior_effect),
                ));
            }

            let host = request.url.host_str().unwrap_or_default().to_owned();
            match call.execute(prepared).await {
                Err(error) => {
                    return Err(HttpError::transport(error)
                        .at_host(host)
                        .promote(prior_effect));
                }
                Ok(OneHopResponse::Response {
                    status,
                    headers,
                    body,
                    ..
                }) => {
                    let body = body.map(move |chunk| {
                        chunk.map(bytes::Bytes::from).map_err(|error| {
                            HttpError::transport(error)
                                .at_host(host.clone())
                                .promote(true)
                        })
                    });
                    return Ok(HttpResponse {
                        status,
                        headers,
                        body: Box::pin(body),
                    });
                }
                Ok(OneHopResponse::Redirect { location, .. }) => {
                    prior_effect = true;
                    let location = std::str::from_utf8(&location).map_err(|_| {
                        HttpError::profile(
                            HttpProfileError::InvalidRedirect,
                            DeliveryCertainty::PossiblySent,
                        )
                    })?;
                    request.url = request.url.join(location).map_err(|_| {
                        HttpError::profile(
                            HttpProfileError::InvalidRedirect,
                            DeliveryCertainty::PossiblySent,
                        )
                    })?;
                }
            }
        }
    }
}

fn route_from_url(url: &Url) -> Result<HttpRoute, HttpProfileError> {
    if has_userinfo(url) {
        return Err(HttpProfileError::UserInfo);
    }
    if url.fragment().is_some() {
        return Err(HttpProfileError::Fragment);
    }
    let scheme = match url.scheme() {
        "http" => HttpScheme::Http,
        "https" => HttpScheme::Https,
        _ => return Err(HttpProfileError::UnsupportedScheme),
    };
    let host = url.host_str().ok_or(HttpProfileError::MissingHost)?;
    let port = url
        .port_or_known_default()
        .ok_or(HttpProfileError::MissingPort)?;
    let target = NetworkTarget::new(host, port).map_err(|_| HttpProfileError::InvalidTarget)?;
    HttpRoute::new(scheme, target).map_err(|_| HttpProfileError::InvalidTarget)
}

fn compare_http_routes(left: &HttpRoute, right: &HttpRoute) -> std::cmp::Ordering {
    scheme_tag(left.scheme())
        .cmp(&scheme_tag(right.scheme()))
        .then_with(|| left.target().host().cmp(right.target().host()))
        .then_with(|| left.target().port().cmp(&right.target().port()))
}

fn scheme_tag(scheme: HttpScheme) -> u8 {
    match scheme {
        HttpScheme::Http => 0,
        HttpScheme::Https => 1,
    }
}

fn fingerprint_http_profile(
    id: &str,
    routes: &[HttpRoute],
    bindings: &[Vec<u8>],
) -> HttpProfileFingerprint {
    let mut hasher = Sha256::new();
    hash_profile_frame(&mut hasher, HTTP_PROFILE_FINGERPRINT_DOMAIN);
    hash_profile_frame(&mut hasher, HTTP_PROFILE_POLICY_VERSION);
    hash_profile_frame(&mut hasher, id.as_bytes());
    hasher.update((routes.len() as u128).to_be_bytes());
    for route in routes {
        hash_profile_frame(&mut hasher, &[scheme_tag(route.scheme())]);
        hash_profile_frame(&mut hasher, route.target().host().as_bytes());
        hash_profile_frame(&mut hasher, &route.target().port().to_be_bytes());
    }
    hasher.update((bindings.len() as u128).to_be_bytes());
    for binding in bindings {
        hash_profile_frame(&mut hasher, binding);
    }
    HttpProfileFingerprint {
        id: id.to_owned(),
        digest: hasher.finalize().into(),
    }
}

fn hash_profile_frame(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u128).to_be_bytes());
    hasher.update(bytes);
}

fn has_userinfo(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return true;
    }
    url.as_str()
        .strip_prefix(url.scheme())
        .and_then(|rest| rest.strip_prefix("://"))
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

/// The HTTP verbs currently used by provider and auxiliary mappings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// One provider-owned request before its profile prepares one-hop authority.
pub struct HttpRequest {
    pub url: Url,
    pub method: Method,
    pub headers: Vec<HttpHeader>,
    pub body: Option<Vec<u8>>,
}

pub(crate) fn public_header(name: &str, value: impl Into<Vec<u8>>) -> HttpHeader {
    HttpHeader::new(name, value.into()).expect("a source-controlled HTTP header is valid")
}

pub(crate) fn bearer_header(value: &[u8]) -> HttpHeader {
    let mut bearer = SecretMaterial::new(b"Bearer ".to_vec());
    bearer.extend_from_slice(value);
    HttpHeader::secret("authorization", bearer).expect("the authorization header name is valid")
}

impl HttpRequest {
    pub(crate) fn to_core(&self) -> Result<CoreHttpRequest, HttpProfileError> {
        let mut path = self.url.path().to_owned();
        if let Some(query) = self.url.query() {
            path.push('?');
            path.push_str(query);
        }
        CoreHttpRequest::new(
            self.method.as_str(),
            &path,
            self.headers.clone(),
            self.body.clone().unwrap_or_default(),
        )
        .map_err(|_| HttpProfileError::InvalidRequest)
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("url", &self.url)
            .field("method", &self.method)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(HttpHeader::name)
                    .collect::<Vec<_>>(),
            )
            .field("body", &self.body.as_ref().map(|body| Elided(body.len())))
            .finish()
    }
}

struct Elided(usize);

impl fmt::Debug for Elided {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} bytes>", self.0)
    }
}

pub type ByteStream = BoxStream<'static, Result<bytes::Bytes, HttpError>>;

/// One response with ordered raw headers and a streamed body.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: ByteStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpFailure {
    Profile(HttpProfileError),
    Authority,
    Offline,
    Resolution,
    InvalidRequest,
    Connect,
    Exchange,
    ResponseBody,
    Cancelled,
    TimedOut,
}

/// A secret-free protocol view of a typed transport or profile failure.
#[derive(Debug)]
pub struct HttpError {
    failure: HttpFailure,
    delivery: DeliveryCertainty,
    host: Option<String>,
    connect_source: Option<String>,
}

impl HttpError {
    fn profile(failure: HttpProfileError, delivery: DeliveryCertainty) -> Self {
        Self {
            failure: HttpFailure::Profile(failure),
            delivery,
            host: None,
            connect_source: None,
        }
    }

    pub(crate) fn transport(error: TransportError) -> Self {
        let failure = match error.failure() {
            TransportFailure::AuthoritySpent
            | TransportFailure::AuthorityMismatch
            | TransportFailure::ExecutableDenied(_) => HttpFailure::Authority,
            TransportFailure::Offline => HttpFailure::Offline,
            TransportFailure::Resolution(_) => HttpFailure::Resolution,
            TransportFailure::InvalidRequest(_) => HttpFailure::InvalidRequest,
            TransportFailure::Connect(_) => HttpFailure::Connect,
            TransportFailure::Exchange(_) => HttpFailure::Exchange,
            TransportFailure::ResponseBody(_) => HttpFailure::ResponseBody,
            TransportFailure::Cancelled => HttpFailure::Cancelled,
            TransportFailure::TimedOut => HttpFailure::TimedOut,
        };
        Self {
            failure,
            delivery: error.delivery(),
            host: None,
            connect_source: match error.failure() {
                TransportFailure::Connect(source) => Some(source.clone()),
                _ => None,
            },
        }
    }

    fn at_host(mut self, host: String) -> Self {
        self.host = Some(host);
        self
    }

    fn promote(mut self, prior_effect: bool) -> Self {
        if prior_effect {
            self.delivery = DeliveryCertainty::PossiblySent;
        }
        self
    }

    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }

    pub fn kind(&self) -> ErrorKind {
        match self.failure {
            HttpFailure::Profile(_)
            | HttpFailure::Authority
            | HttpFailure::Offline
            | HttpFailure::InvalidRequest => ErrorKind::Invalid,
            HttpFailure::Resolution
            | HttpFailure::Connect
            | HttpFailure::Exchange
            | HttpFailure::ResponseBody
            | HttpFailure::Cancelled
            | HttpFailure::TimedOut => ErrorKind::Transient,
        }
    }
}

#[cfg(test)]
pub(crate) fn test_http_error(error: TransportError) -> HttpError {
    HttpError::transport(error)
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.failure {
            HttpFailure::Profile(failure) => return write!(f, "{failure}"),
            HttpFailure::Authority => "HTTP authority was invalid",
            HttpFailure::Offline => "HTTP is unavailable under offline confinement",
            HttpFailure::Resolution => "HTTP name resolution failed",
            HttpFailure::InvalidRequest => "HTTP request construction failed",
            HttpFailure::Connect => "HTTP connection failed",
            HttpFailure::Exchange => "HTTP exchange failed",
            HttpFailure::ResponseBody => "HTTP response body failed",
            HttpFailure::Cancelled => "HTTP call was cancelled",
            HttpFailure::TimedOut => "HTTP call timed out",
        };
        f.write_str(message)?;
        if let Some(host) = &self.host {
            write!(f, " ({host})")?;
        }
        if let Some(source) = &self.connect_source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpError {}

fn delivery_after(prior_effect: bool) -> DeliveryCertainty {
    if prior_effect {
        DeliveryCertainty::PossiblySent
    } else {
        DeliveryCertainty::DefinitelyUnsent
    }
}

/// One logical HTTP call retaining its absolute deadline across all hops.
pub trait HttpCall: Send {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>>;
}

/// Consumer-owned HTTP port. Beginning once creates one logical call scope.
pub trait Http: Send + Sync {
    fn begin_call(&self) -> Box<dyn HttpCall>;

    fn open_websocket(
        &self,
        _call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<Box<dyn HttpWebSocket>, TransportError>> {
        Box::pin(futures::future::ready(Err(
            TransportError::definitely_unsent(TransportFailure::Connect(
                "WebSocket is unavailable".into(),
            )),
        )))
    }
}

/// Provider admission is checked for every message, not only at the handshake.
pub trait HttpWebSocket: Send {
    fn send(&mut self, call: PreparedHttpCall) -> BoxFuture<'_, Result<(), TransportError>>;
    fn receive(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>>;
}

/// One buffered auxiliary exchange. Generation bodies remain streamed.
pub(crate) struct Exchange {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub bytes: Vec<u8>,
}

impl Exchange {
    /// The first UTF-8 value of `name`, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name().eq_ignore_ascii_case(name))
            .and_then(|header| std::str::from_utf8(header.value()).ok())
    }
}

pub(crate) async fn fetch(
    http: &Arc<dyn Http>,
    profile: &AdmittedHttpProfile,
    request: HttpRequest,
) -> Result<Vec<u8>, String> {
    let answer = exchange(http, profile, request).await?;
    if !(200..300).contains(&answer.status) {
        return Err(refusal(&answer));
    }
    Ok(answer.bytes)
}

/// What a non-2xx catalogue answer is allowed to say: its status, and no bytes.
///
/// A catalogue request carries the operator's credential, and a failing
/// endpoint may echo it back inside its own error document — as may a proxy
/// that reflects request headers into an interception page. That string is
/// rendered onto a terminal, and since the ACP surface landed it also travels
/// to a host in `error.data`, so the body cannot be part of it.
///
/// The body is not lost: it is logged here at `warn`, in a field of its own so
/// a structured subscriber can drop it without losing the status beside it.
/// This is the same rule `oauth::BuiltInFlow::classify` applies to the login
/// half and `RunTermination::after_turn` applies to the mid-turn half.
///
/// `ego::body` is disabled in CLI filters unless the operator explicitly names
/// that target, so `EGO_LOG=info` does not print request or response bodies.
pub(crate) fn refusal(answer: &Exchange) -> String {
    tracing::warn!(
        target: "ego::body",
        status = answer.status,
        body = %String::from_utf8_lossy(&answer.bytes),
        "an upstream endpoint refused a pre-turn request"
    );
    format!("it answered {}", answer.status)
}

pub(crate) async fn exchange(
    http: &Arc<dyn Http>,
    profile: &AdmittedHttpProfile,
    request: HttpRequest,
) -> Result<Exchange, String> {
    let HttpResponse {
        status,
        headers,
        mut body,
    } = profile
        .execute(http.as_ref(), request)
        .await
        .map_err(|error| error.to_string())?;

    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        bytes.extend_from_slice(&chunk.map_err(|error| error.to_string())?);
    }
    Ok(Exchange {
        status,
        headers,
        bytes,
    })
}

#[cfg(test)]
mod tests;
