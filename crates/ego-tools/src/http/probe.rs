//! Layered reachability probe for `ego doctor`.
//!
//! Establishes how far a connection to one endpoint gets: DNS, TCP, TLS, then
//! one HTTP exchange. The request is a bare `GET` with no credential, no proxy,
//! no redirect and no body, so it cannot log in, refresh a token or spend
//! money. Any HTTP status, even 401 or 404, proves the endpoint answered.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use url::Url;

use crate::exec::runtime_handle;

/// The longest any one layer may take.
const LAYER_TIMEOUT: Duration = Duration::from_secs(2);

/// The layer a connection attempt stopped at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Dns,
    Tcp,
    Tls,
    Http,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dns => "DNS",
            Self::Tcp => "TCP",
            Self::Tls => "TLS",
            Self::Http => "HTTP",
        }
    }
}

/// The endpoint answered an HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reached {
    pub status: u16,
}

/// The first layer that failed, with a credential-free reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreached {
    pub layer: Layer,
    pub detail: String,
}

fn stop(layer: Layer, detail: impl std::fmt::Display) -> Unreached {
    Unreached {
        layer,
        detail: detail.to_string(),
    }
}

/// Probe `url`, never exceeding `budget` in total.
///
/// Blocking: call it from a plain thread, not from inside an async task.
pub fn probe(url: &Url, budget: Duration) -> Result<Reached, Unreached> {
    let deadline = Instant::now() + budget;
    runtime_handle().block_on(layered(url, deadline))
}

/// Time left before `deadline`, capped at one layer.
fn allowance(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .min(LAYER_TIMEOUT)
}

async fn layered(url: &Url, deadline: Instant) -> Result<Reached, Unreached> {
    let host = url
        .host_str()
        .ok_or_else(|| stop(Layer::Dns, "the endpoint names no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| stop(Layer::Dns, "the endpoint names no port"))?;

    let lookup = tokio::time::timeout(allowance(deadline), tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| stop(Layer::Dns, "timed out resolving the host"))?
        .map_err(|error| stop(Layer::Dns, error))?;
    let addresses: Vec<SocketAddr> = lookup.collect();
    if addresses.is_empty() {
        return Err(stop(Layer::Dns, "the host resolved to no address"));
    }

    let mut connected = None;
    let mut last = String::from("no address was tried");
    for address in &addresses {
        let remaining = allowance(deadline);
        if remaining.is_zero() {
            last = "timed out connecting".to_owned();
            break;
        }
        match tokio::time::timeout(remaining, tokio::net::TcpStream::connect(address)).await {
            Ok(Ok(_stream)) => {
                connected = Some(*address);
                break;
            }
            Ok(Err(error)) => last = error.to_string(),
            Err(_) => last = "timed out connecting".to_owned(),
        }
    }
    let Some(address) = connected else {
        return Err(stop(Layer::Tcp, last));
    };

    // TCP is proven, so a connect failure inside the client is the TLS
    // handshake; any later failure is the HTTP exchange.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .pool_max_idle_per_host(0)
        .connect_timeout(allowance(deadline))
        .timeout(deadline.saturating_duration_since(Instant::now()))
        .resolve_to_addrs(host, &[address])
        .build()
        .map_err(|error| stop(Layer::Http, error.without_url()))?;
    match client.get(url.clone()).send().await {
        Ok(response) => Ok(Reached {
            status: response.status().as_u16(),
        }),
        Err(error) => {
            let layer = if error.is_connect() && url.scheme() == "https" {
                Layer::Tls
            } else {
                Layer::Http
            };
            Err(stop(layer, chain(error.without_url())))
        }
    }
}

/// The error and its causes. The URL is already stripped: it can hold secrets.
fn chain(error: reqwest::Error) -> String {
    let mut detail = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    detail
}

#[cfg(test)]
mod tests;
