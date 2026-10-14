//! Run-owned DNS and exactly-one-hop HTTP adapters.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use ego_core::permit::{
    DnsResolution, HttpHeader, HttpRequest, HttpScheme, NetworkTarget, OneHop, OneHopResponse,
    ResolvedConnect, Resolver, RunConfinement, TransportError, TransportFailure,
};
use ego_core::turn::CancelToken;
use futures::future::BoxFuture;
use futures::{SinkExt as _, Stream, StreamExt as _};

use crate::exec::{HttpCallTimeout, runtime_handle};

/// The host resolver used by production composition.
#[derive(Default)]
pub struct SystemResolver;

impl SystemResolver {
    pub const fn new() -> Self {
        Self
    }
}

impl Resolver for SystemResolver {
    fn resolve(
        &self,
        target: NetworkTarget,
        deadline: Instant,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        let mut task = AbortOnDrop(runtime_handle().spawn(async move {
            let addresses = tokio::net::lookup_host((target.host().to_owned(), target.port()))
                .await
                .map_err(|error| {
                    TransportError::definitely_unsent(TransportFailure::Resolution(
                        error.to_string(),
                    ))
                })?
                .map(|address| address.ip())
                .collect::<Vec<_>>();
            DnsResolution::new(target, addresses).map_err(|error| {
                TransportError::definitely_unsent(TransportFailure::Resolution(error.to_string()))
            })
        }));
        Box::pin(async move {
            tokio::select! {
                result = &mut task.0 => result.map_err(|error| {
                    TransportError::definitely_unsent(TransportFailure::Resolution(format!(
                        "resolver task failed: {error}"
                    )))
                })?,
                _ = deadline_elapsed(deadline) => {
                    Err(definitely_unsent(TransportFailure::TimedOut))
                }
            }
        })
    }
}

/// In-memory, HTTP-only authority for explicitly selected setup inference.
/// Construction neither measures a process backend nor opens an evidence store.
/// This exposes no process, filesystem, MCP, or session-storage handle.
pub struct InferenceHttpPorts {
    transport: Arc<OneShotHttpTransport>,
}

impl InferenceHttpPorts {
    pub fn new(confinement: RunConfinement, cancel: CancelToken, timeout: HttpCallTimeout) -> Self {
        Self {
            transport: Arc::new(OneShotHttpTransport::new(
                Arc::new(SystemResolver::new()),
                cancel,
                timeout,
                None,
                confinement,
            )),
        }
    }

    pub fn resolver_handle(&self) -> Arc<dyn Resolver> {
        self.transport.clone()
    }
    pub fn one_hop_handle(&self) -> Arc<dyn OneHop> {
        self.transport.clone()
    }
    pub fn begin_call(&self) -> Instant {
        self.transport.begin_call()
    }
}

/// One run's HTTP effect boundary. Construction is restricted to
/// [`crate::exec::RunEffectPorts`] or [`InferenceHttpPorts`].
pub(crate) struct OneShotHttpTransport {
    resolver: Arc<dyn Resolver>,
    cancel: CancelToken,
    timeout: HttpCallTimeout,
    run_deadline: Option<Instant>,
    confinement: RunConfinement,
}

impl OneShotHttpTransport {
    pub(crate) fn new(
        resolver: Arc<dyn Resolver>,
        cancel: CancelToken,
        timeout: HttpCallTimeout,
        run_deadline: Option<Instant>,
        confinement: RunConfinement,
    ) -> Self {
        Self {
            resolver,
            cancel,
            timeout,
            run_deadline,
            confinement,
        }
    }
}

impl Resolver for OneShotHttpTransport {
    fn resolve(
        &self,
        target: NetworkTarget,
        deadline: Instant,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        if self.confinement.offline() {
            return Box::pin(futures::future::ready(offline_resolution(target)));
        }
        if self.cancel.is_cancelled() {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                cancellation_failure(&self.cancel),
            ))));
        }
        if deadline <= Instant::now() {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                TransportFailure::TimedOut,
            ))));
        }
        let resolver = Arc::clone(&self.resolver);
        let cancel = self.cancel.clone();
        let task = runtime_handle().spawn(async move {
            let mut resolution = Box::pin(resolver.resolve(target, deadline));
            loop {
                tokio::select! {
                    result = &mut resolution => return result,
                    _ = deadline_elapsed(deadline) => {
                        return Err(definitely_unsent(TransportFailure::TimedOut));
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(2)) => {
                        if cancel.is_cancelled() {
                            return Err(definitely_unsent(cancellation_failure(&cancel)));
                        }
                    }
                }
            }
        });
        Box::pin(async move {
            task.await.map_err(|error| {
                definitely_unsent(TransportFailure::Resolution(format!(
                    "resolver task failed: {error}"
                )))
            })?
        })
    }
}

impl OneHop for OneShotHttpTransport {
    fn admit_mcp(&self) -> Result<(), TransportError> {
        if self.confinement.admits_mcp_http() {
            return Ok(());
        }
        let failure = if self.confinement.offline() {
            TransportFailure::Offline
        } else {
            TransportFailure::AuthorityMismatch
        };
        Err(definitely_unsent(failure))
    }

    fn begin_call(&self) -> Instant {
        let now = Instant::now();
        let configured = now.checked_add(self.timeout.duration()).unwrap_or(now);
        self.run_deadline.map_or(configured, |run_deadline| {
            std::cmp::min(configured, run_deadline)
        })
    }

    fn execute(
        &self,
        connect: ResolvedConnect,
        request: HttpRequest,
        deadline: Instant,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        if self.confinement.offline() && !offline_connect_allowed(&connect) {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                TransportFailure::Offline,
            ))));
        }
        if self.cancel.is_cancelled() {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                cancellation_failure(&self.cancel),
            ))));
        }
        if &ego_core::permit::HttpRequestDigest::of(&request) != connect.request_digest() {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))));
        }
        if deadline <= Instant::now() {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                TransportFailure::TimedOut,
            ))));
        }

        let cancel = self.cancel.clone();
        let mut task = AbortOnDrop(
            runtime_handle().spawn(execute_one_hop(connect, request, cancel, deadline)),
        );
        Box::pin(async move {
            (&mut task.0).await.map_err(|error| {
                possibly_sent(TransportFailure::Exchange(format!(
                    "HTTP task failed: {error}"
                )))
            })?
        })
    }
}

fn offline_resolution(target: NetworkTarget) -> Result<DnsResolution, TransportError> {
    if target.host() != "localhost" {
        return Err(definitely_unsent(TransportFailure::Offline));
    }
    DnsResolution::new(target, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        .map_err(|error| definitely_unsent(TransportFailure::Resolution(error.to_string())))
}

fn offline_connect_allowed(connect: &ResolvedConnect) -> bool {
    let target = connect.target();
    let logical_loopback =
        target.host() == "localhost" || target.ip().is_some_and(canonical_is_loopback);
    logical_loopback && canonical_is_loopback(connect.selected())
}

fn canonical_is_loopback(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback(),
        IpAddr::V6(address) => address.to_canonical().is_loopback(),
    }
}

async fn execute_one_hop(
    connect: ResolvedConnect,
    request: HttpRequest,
    cancel: CancelToken,
    deadline: Instant,
) -> Result<OneHopResponse, TransportError> {
    let route = connect.route();
    let scheme = match route.scheme() {
        HttpScheme::Http => "http",
        HttpScheme::Https => "https",
    };
    let target = route.target();
    let authority = if target.host().contains(':') {
        format!("[{}]:{}", target.host(), target.port())
    } else {
        format!("{}:{}", target.host(), target.port())
    };
    let url = format!("{scheme}://{authority}{}", request.path());
    let selected = SocketAddr::new(connect.selected(), target.port());
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .pool_max_idle_per_host(0)
        .dns_resolver(DenyFallbackResolver)
        .resolve_to_addrs(target.host(), &[selected])
        .build()
        .map_err(|error| definitely_unsent(TransportFailure::InvalidRequest(error.to_string())))?;
    let method = reqwest::Method::from_bytes(request.method().as_bytes())
        .map_err(|error| definitely_unsent(TransportFailure::InvalidRequest(error.to_string())))?;
    let mut outgoing = client.request(method, url);
    for header in request.headers() {
        let name =
            reqwest::header::HeaderName::from_bytes(header.name().as_bytes()).map_err(|error| {
                definitely_unsent(TransportFailure::InvalidRequest(error.to_string()))
            })?;
        let value = reqwest::header::HeaderValue::from_bytes(header.value()).map_err(|error| {
            definitely_unsent(TransportFailure::InvalidRequest(error.to_string()))
        })?;
        outgoing = outgoing.header(name, value);
    }
    outgoing = outgoing.body(request.body().to_vec());

    let mut send = Box::pin(outgoing.send());
    let response = loop {
        tokio::select! {
            result = &mut send => break result.map_err(exchange_error)?,
            _ = deadline_elapsed(deadline) => {
                return Err(possibly_sent(TransportFailure::TimedOut));
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(2)) => {
                if cancel.is_cancelled() {
                    return Err(possibly_sent(cancellation_failure(&cancel)));
                }
            }
        }
    };

    let status = response.status().as_u16();
    let headers = response_headers(response.headers())?;

    if response.status().is_redirection()
        && let Some(location) = response.headers().get(reqwest::header::LOCATION)
    {
        let location = location.as_bytes().to_vec();
        return Ok(OneHopResponse::redirect(status, headers, location));
    }

    let (mut sender, receiver) = futures::channel::mpsc::channel(1);
    let task = AbortOnDrop(runtime_handle().spawn(async move {
        let mut body = response.bytes_stream();
        loop {
            let next = tokio::select! {
                item = body.next() => item,
                _ = deadline_elapsed(deadline) => {
                    let _ = sender.send(Err(possibly_sent(TransportFailure::TimedOut))).await;
                    return;
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(2)) => {
                    if cancel.is_cancelled() {
                        let _ = sender
                            .send(Err(possibly_sent(cancellation_failure(&cancel))))
                            .await;
                        return;
                    }
                    continue;
                }
            };
            match next {
                Some(Ok(chunk)) => {
                    if sender.send(Ok(chunk.to_vec())).await.is_err() {
                        return;
                    }
                }
                Some(Err(error)) => {
                    let _ = sender
                        .send(Err(possibly_sent(TransportFailure::ResponseBody(
                            error.to_string(),
                        ))))
                        .await;
                    return;
                }
                None => return,
            }
        }
    }));
    Ok(OneHopResponse::streaming_response(
        status,
        headers,
        Box::pin(OwnedResponseBody {
            receiver,
            _task: task,
        }),
    ))
}

struct OwnedResponseBody {
    receiver: futures::channel::mpsc::Receiver<Result<Vec<u8>, TransportError>>,
    _task: AbortOnDrop<()>,
}

impl Stream for OwnedResponseBody {
    type Item = Result<Vec<u8>, TransportError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_next(context)
    }
}

fn response_headers(
    headers: &reqwest::header::HeaderMap,
) -> Result<Vec<HttpHeader>, TransportError> {
    headers
        .iter()
        .map(|(name, value)| {
            HttpHeader::new(name.as_str(), value.as_bytes().to_vec()).map_err(|error| {
                possibly_sent(TransportFailure::Exchange(format!(
                    "invalid response header: {error}"
                )))
            })
        })
        .collect()
}

fn exchange_error(error: reqwest::Error) -> TransportError {
    let failure = if error.is_connect() {
        TransportFailure::Connect(error.to_string())
    } else {
        TransportFailure::Exchange(error.to_string())
    };
    if error.is_builder() || error.is_connect() {
        definitely_unsent(failure)
    } else {
        possibly_sent(failure)
    }
}

fn definitely_unsent(failure: TransportFailure) -> TransportError {
    TransportError::definitely_unsent(failure)
}

fn possibly_sent(failure: TransportFailure) -> TransportError {
    TransportError::possibly_sent(failure)
}

fn cancellation_failure(cancel: &CancelToken) -> TransportFailure {
    match cancel.cause() {
        Some(ego_core::event::domain::CancelCause::Timeout) => TransportFailure::TimedOut,
        _ => TransportFailure::Cancelled,
    }
}

async fn deadline_elapsed(deadline: Instant) {
    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug)]
struct DenyFallbackResolver;

impl reqwest::dns::Resolve for DenyFallbackResolver {
    fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async {
            Err(Box::new(io::Error::other(
                "DNS fallback is outside one-shot connect authority",
            )) as Box<dyn std::error::Error + Send + Sync>)
        })
    }
}

#[cfg(test)]
mod tests;
