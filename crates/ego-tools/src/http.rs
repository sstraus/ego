//! Run-owned DNS and exactly-one-hop HTTP adapters.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use ego_core::permit::{
    DnsResolution, HttpHeader, HttpRequest, HttpScheme, NetworkTarget, OneHop, OneHopResponse,
    ResolvedConnect, Resolver, TransportError, TransportFailure,
};
use ego_core::turn::CancelToken;
use futures::future::BoxFuture;
use futures::{SinkExt as _, StreamExt as _};

use crate::exec::{TransportLimits, runtime_handle};

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
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        let task = runtime_handle().spawn(async move {
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
        });
        Box::pin(async move {
            task.await.map_err(|error| {
                TransportError::definitely_unsent(TransportFailure::Resolution(format!(
                    "resolver task failed: {error}"
                )))
            })?
        })
    }
}

/// One run's HTTP effect boundary. Construction is restricted to
/// [`crate::exec::RunEffectPorts`].
pub(crate) struct OneShotHttpTransport {
    resolver: Arc<dyn Resolver>,
    cancel: CancelToken,
    limits: TransportLimits,
    offline: bool,
}

impl OneShotHttpTransport {
    pub(crate) fn new(
        resolver: Arc<dyn Resolver>,
        cancel: CancelToken,
        limits: TransportLimits,
        offline: bool,
    ) -> Self {
        Self {
            resolver,
            cancel,
            limits,
            offline,
        }
    }
}

impl Resolver for OneShotHttpTransport {
    fn resolve(
        &self,
        target: NetworkTarget,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        if self.offline {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                TransportFailure::Offline,
            ))));
        }
        if self.cancel.is_cancelled() {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                TransportFailure::Cancelled,
            ))));
        }
        let resolver = Arc::clone(&self.resolver);
        let cancel = self.cancel.clone();
        let task = runtime_handle().spawn(async move {
            let mut resolution = Box::pin(resolver.resolve(target));
            loop {
                tokio::select! {
                    result = &mut resolution => return result,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(2)) => {
                        if cancel.is_cancelled() {
                            return Err(definitely_unsent(TransportFailure::Cancelled));
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
    fn execute(
        &self,
        connect: ResolvedConnect,
        request: HttpRequest,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        if self.offline {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                TransportFailure::Offline,
            ))));
        }
        if self.cancel.is_cancelled() {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                TransportFailure::Cancelled,
            ))));
        }
        if &ego_core::permit::HttpRequestDigest::of(&request) != connect.request_digest() {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))));
        }
        if request_bytes(&request).is_none_or(|bytes| bytes > self.limits.max_request_bytes()) {
            return Box::pin(futures::future::ready(Err(definitely_unsent(
                TransportFailure::RequestLimit {
                    limit: self.limits.max_request_bytes(),
                },
            ))));
        }

        let cancel = self.cancel.clone();
        let limits = self.limits;
        let task = runtime_handle().spawn(execute_one_hop(connect, request, cancel, limits));
        Box::pin(async move {
            task.await.map_err(|error| {
                possibly_sent(TransportFailure::Exchange(format!(
                    "HTTP task failed: {error}"
                )))
            })?
        })
    }
}

async fn execute_one_hop(
    connect: ResolvedConnect,
    request: HttpRequest,
    cancel: CancelToken,
    limits: TransportLimits,
) -> Result<OneHopResponse, TransportError> {
    let deadline = tokio::time::Instant::now() + limits.timeout();
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
            _ = tokio::time::sleep_until(deadline) => {
                return Err(possibly_sent(TransportFailure::TimedOut { limit: limits.timeout() }));
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(2)) => {
                if cancel.is_cancelled() {
                    return Err(possibly_sent(TransportFailure::Cancelled));
                }
            }
        }
    };

    let status = response.status().as_u16();
    let headers = response_headers(response.headers())?;
    let mut response_bytes = header_bytes(&headers).ok_or_else(|| response_limit(limits))?;
    if response_bytes > limits.max_response_bytes() {
        return Err(response_limit(limits));
    }

    if response.status().is_redirection()
        && let Some(location) = response.headers().get(reqwest::header::LOCATION)
    {
        let location = location.as_bytes().to_vec();
        response_bytes = checked_add_len(response_bytes, location.len())
            .ok_or_else(|| response_limit(limits))?;
        if response_bytes > limits.max_response_bytes() {
            return Err(response_limit(limits));
        }
        return Ok(OneHopResponse::redirect(status, headers, location));
    }

    let (mut sender, receiver) = futures::channel::mpsc::channel(1);
    runtime_handle().spawn(async move {
        let mut body = response.bytes_stream();
        let mut observed = response_bytes;
        loop {
            let next = tokio::select! {
                item = body.next() => item,
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = sender.send(Err(possibly_sent(TransportFailure::TimedOut {
                        limit: limits.timeout(),
                    }))).await;
                    return;
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(2)) => {
                    if cancel.is_cancelled() {
                        let _ = sender.send(Err(possibly_sent(TransportFailure::Cancelled))).await;
                        return;
                    }
                    continue;
                }
            };
            match next {
                Some(Ok(chunk)) => {
                    let Some(next_total) = checked_add_len(observed, chunk.len()) else {
                        let _ = sender.send(Err(response_limit(limits))).await;
                        return;
                    };
                    if next_total > limits.max_response_bytes() {
                        let _ = sender.send(Err(response_limit(limits))).await;
                        return;
                    }
                    observed = next_total;
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
    });
    Ok(OneHopResponse::streaming_response(
        status,
        headers,
        Box::pin(receiver),
    ))
}

fn request_bytes(request: &HttpRequest) -> Option<u64> {
    let mut total = 0_u64;
    for bytes in [
        request.method().as_bytes(),
        request.path().as_bytes(),
        request.body(),
    ] {
        total = checked_add_len(total, bytes.len())?;
    }
    for header in request.headers() {
        total = checked_add_len(total, header.name().len())?;
        total = checked_add_len(total, header.value().len())?;
    }
    Some(total)
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

fn header_bytes(headers: &[HttpHeader]) -> Option<u64> {
    headers.iter().try_fold(0_u64, |total, header| {
        checked_add_len(
            checked_add_len(total, header.name().len())?,
            header.value().len(),
        )
    })
}

fn checked_add_len(total: u64, len: usize) -> Option<u64> {
    total.checked_add(u64::try_from(len).ok()?)
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

fn response_limit(limits: TransportLimits) -> TransportError {
    possibly_sent(TransportFailure::ResponseLimit {
        limit: limits.max_response_bytes(),
    })
}

fn definitely_unsent(failure: TransportFailure) -> TransportError {
    TransportError::definitely_unsent(failure)
}

fn possibly_sent(failure: TransportFailure) -> TransportError {
    TransportError::possibly_sent(failure)
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
