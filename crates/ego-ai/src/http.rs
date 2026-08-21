//! The transport seam, and the egress policy that is a property of the seam
//! rather than a habit of its callers.
//!
//! `ARCHITECTURE.md` §3.1 gives `ego-ai` ownership of `Http`; `CLI.md` §3
//! promises `--offline` "assert no non-loopback egress (enforced at
//! transport)". This module is where that promise is kept, and — just as
//! importantly — where its exact size is written down.
//!
//! # Why the check cannot live in the providers
//!
//! The obvious implementation is for each wire format to look at its base URL
//! before calling out. That is the convention `plans/m0b-vertical-slice.md`
//! Step 8 forbids, and it is not pedantry: **a host string is not a socket.**
//! Three things sit between the two, and each of them is a way for a
//! host-string check to be true while the packets go somewhere else:
//!
//! 1. **A proxy.** `HTTP_PROXY=http://exfil.example` makes an HTTP client
//!    open its connection to the proxy, not to the URL's host. The request
//!    line still says `127.0.0.1`. The check passes; the bytes leave.
//! 2. **A redirect.** An admitted loopback server answering `302 Location:
//!    https://evil.example` gets followed by default — [`reqwest`] allows ten
//!    hops. The first hop is the only one a caller-side check ever sees.
//! 3. **Resolution.** Deciding whether a *name* is loopback means asking a
//!    resolver, and asking a resolver about `secrets.attacker.example` is
//!    itself non-loopback egress — subdomain-encoded DNS is a textbook
//!    exfiltration channel. A policy that leaks the question before it
//!    decides the answer has already broken the promise it is enforcing.
//!
//! So the policy lives here, at the one place that owns the socket, and it is
//! applied at all three points: to the initial target, to every redirect hop,
//! and to resolution itself.
//!
//! # What this seam does and does not guarantee
//!
//! `docs/08-codex-critique.md` R2 was accepted (`docs/10-triage.md`) on the
//! condition that the capability is **labelled honestly**, so:
//!
//! - It guarantees that this transport opens no connection to a non-loopback
//!   address, sends no DNS query, and consults no proxy, when the policy is
//!   [`EgressPolicy::LoopbackOnly`].
//! - It does **not** confine anything else. R2 names four exits — provider
//!   HTTP, MCP transports, hooks, and spawned commands. This is the first.
//!   The other three do not exist yet; when they do, each needs its own
//!   enforcement at its own boundary, because a spawned `curl` is not stopped
//!   by a type in this file.
//! - It does **not** guarantee that a prompt stays on the machine. An
//!   admitted loopback peer may forward upstream on its own account: Ollama's
//!   `cloudPassthroughMiddleware` proxies a `:cloud` model request from
//!   `localhost:11434` to `ollama.com`, and the caller cannot tell from the
//!   API shape. "No non-loopback socket *from ego*" is the true claim;
//!   "nothing reaches the network" is not, and this module does not make it.
//!
//! # Why delivery certainty is reported here too
//!
//! For the same reason, one level down. Whether the request bytes left is a
//! fact about the socket, and it survives only as long as the error keeps its
//! type: a failure flattened to a `String` cannot say whether the connector
//! never opened or the response died halfway, and those are opposite answers
//! to whether a retry double-bills. So [`HttpError`] carries a
//! [`DeliveryCertainty`] rather than making every wire format guess from
//! prose — see [`HttpError::delivery`].

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::types::ErrorKind;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use url::{Host, Url};

/// A response body, streamed. Deliberately a stream and not a `Vec<u8>`:
/// generation is streamed end to end, and buffering it here would make every
/// delta wait for the last one.
pub type ByteStream = BoxStream<'static, Result<bytes::Bytes, HttpError>>;

/// What egress this transport is allowed to perform.
///
/// [`LoopbackOnly`](Self::LoopbackOnly) is what `--offline` selects. The
/// variant is named for what it enforces rather than for the flag that
/// selects it, because "offline" overstates it — see the module doc's third
/// bullet on what an admitted loopback peer may still do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressPolicy {
    /// Any http(s) destination is admitted. The default posture.
    Unrestricted,
    /// Only loopback destinations are admitted, and nothing is resolved.
    LoopbackOnly,
}

/// Names the destination that was refused and which fact refused it.
///
/// `ARCHITECTURE.md` §3.2: a denial must name what was denied. A bare
/// "blocked by --offline" would leave a user guessing between a typo, a LAN
/// address they thought was local, and a name that merely looks like one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressRefused {
    /// The URL's scheme is not one this transport speaks. Refused under
    /// every policy, including [`EgressPolicy::Unrestricted`]: `file:///etc`
    /// is not an HTTP request that happens to be local, it is not an HTTP
    /// request.
    NotHttp { scheme: String },
    /// The URL carries no host at all.
    NoHost { url: String },
    /// The host is an IP address, and it is not a loopback one.
    NotLoopbackAddress { host: String },
    /// The host is a name. Under [`EgressPolicy::LoopbackOnly`] the only
    /// name admitted is `localhost`, which RFC 6761 §6.3 reserves for
    /// loopback and which this module maps itself; deciding about any other
    /// name would mean asking a resolver, which is the egress being refused.
    NameNeedsResolution { host: String },
}

impl fmt::Display for EgressRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EgressRefused::NotHttp { scheme } => {
                write!(f, "egress refused: {scheme} is not an http(s) scheme")
            }
            EgressRefused::NoHost { url } => {
                write!(f, "egress refused: {url} names no host")
            }
            EgressRefused::NotLoopbackAddress { host } => {
                write!(f, "egress refused: {host} is not a loopback address")
            }
            EgressRefused::NameNeedsResolution { host } => write!(
                f,
                "egress refused: deciding whether {host} is loopback would \
                 require a DNS query, which is itself egress"
            ),
        }
    }
}

impl std::error::Error for EgressRefused {}

/// True for the one name RFC 6761 §6.3 reserves for loopback.
///
/// The `url` crate has already lowercased the host and applied IDNA, so
/// `LOCALHOST` and the fullwidth spelling both arrive here as `localhost`.
/// A single trailing dot is the DNS root and is stripped; everything else is
/// a different name. In particular `evil.localhost` is **not** admitted:
/// RFC 6761 does reserve the whole subtree, but resolvers vary on whether
/// they honour it, and mapping it here would mean inventing a resolution the
/// system does not perform. No local daemon needs it, so the tighter rule
/// wins.
fn is_localhost(host: &str) -> bool {
    host.strip_suffix('.').unwrap_or(host) == "localhost"
}

impl EgressPolicy {
    /// Decides whether one URL may be contacted.
    ///
    /// **Pure**: no DNS, no socket, no clock, no environment. That is not an
    /// incidental property — it is the guarantee. A refusal costs no packets
    /// because the decision needed none, which is why "no non-loopback
    /// egress" can be said about the refusal path at all.
    ///
    /// Classification reads [`Url::host`], never the raw authority string.
    /// The `url` crate normalises every IPv4 spelling — `2130706433`,
    /// `0177.0.0.1`, `0x7f000001`, `127.1` — to the same `Host::Ipv4`, and
    /// parses `http://127.0.0.1@evil.com/` with `evil.com` as the host and
    /// `127.0.0.1` as mere userinfo. A textual check would get both
    /// backwards.
    pub fn admit(&self, url: &Url) -> Result<(), EgressRefused> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(EgressRefused::NotHttp {
                scheme: url.scheme().to_string(),
            });
        }
        let host = url.host().ok_or_else(|| EgressRefused::NoHost {
            url: url.to_string(),
        })?;
        match self {
            EgressPolicy::Unrestricted => {}
            EgressPolicy::LoopbackOnly => match host {
                Host::Ipv4(addr) if addr.is_loopback() => {}
                // `to_canonical` folds `::ffff:127.0.0.1` down to the IPv4
                // loopback it actually addresses; `Ipv6Addr::is_loopback`
                // alone would refuse a genuinely local destination.
                Host::Ipv6(addr) if addr.to_canonical().is_loopback() => {}
                Host::Ipv4(addr) => {
                    return Err(EgressRefused::NotLoopbackAddress {
                        host: addr.to_string(),
                    });
                }
                Host::Ipv6(addr) => {
                    return Err(EgressRefused::NotLoopbackAddress {
                        host: addr.to_string(),
                    });
                }
                Host::Domain(name) if is_localhost(name) => {}
                Host::Domain(name) => {
                    return Err(EgressRefused::NameNeedsResolution {
                        host: name.to_string(),
                    });
                }
            },
        }
        Ok(())
    }
}

/// The HTTP verbs this transport speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// One request. The target is an ordinary [`Url`] and needs no prior
/// blessing, because the transport admits it itself — see [`Http`].
#[derive(Debug)]
pub struct HttpRequest {
    pub url: Url,
    pub method: Method,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// One response: the status and headers a wire format needs to tell a stream
/// of events from a stream of error text, plus the body it decodes.
///
/// No `Debug`: a [`ByteStream`] has none, and buffering the body to print it
/// is exactly what streaming exists to avoid.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ByteStream,
}

/// What went wrong at the transport.
#[derive(Debug)]
pub enum HttpError {
    /// The policy refused a destination — either the request's own, or a
    /// redirect hop's.
    ///
    /// Carries a [`DeliveryCertainty`] because those two are not the same
    /// event. The request's own destination is refused by a pure predicate
    /// before a socket exists; a *redirect* hop is refused after the original
    /// request was written, answered, and possibly billed. Reading "refused"
    /// as "nothing was sent" would authorise a retry of a request the provider
    /// already has.
    Refused {
        refused: EgressRefused,
        delivery: DeliveryCertainty,
    },
    /// Everything else the transport can fail at, carrying the only reading of
    /// [`DeliveryCertainty`] anyone is in a position to take.
    Transport {
        message: String,
        delivery: DeliveryCertainty,
    },
}

impl HttpError {
    /// A refusal decided before anything was sent.
    ///
    /// The only constructor that may assert [`DeliveryCertainty::DefinitelyUnsent`]
    /// by hand, and it is correct exactly because [`EgressPolicy::admit`] is
    /// pure: no DNS, no socket, no bytes. Every other refusal reads its
    /// certainty off the failure that produced it.
    pub fn refused_before_sending(refused: EgressRefused) -> Self {
        HttpError::Refused {
            refused,
            delivery: DeliveryCertainty::DefinitelyUnsent,
        }
    }

    /// Whether the request bytes may have reached the far side.
    ///
    /// This lives on the transport error and not on the wire format because
    /// the wire format cannot answer it. By the time a failure is prose, "the
    /// connector never opened a socket" and "the connection died halfway
    /// through the response" read identically — and they are opposite answers
    /// to whether a retry can be billed twice
    /// (`ego_core::generator::attempt::DeliveryCertainty`). The distinction
    /// exists exactly once, here, at the layer that watched it happen.
    pub fn delivery(&self) -> DeliveryCertainty {
        match self {
            HttpError::Refused { delivery, .. } | HttpError::Transport { delivery, .. } => {
                *delivery
            }
        }
    }

    /// The egress refusal inside this error, if that is what it is.
    pub fn refusal(&self) -> Option<&EgressRefused> {
        match self {
            HttpError::Refused { refused, .. } => Some(refused),
            HttpError::Transport { .. } => None,
        }
    }

    /// Which class of failure this is, in the caller's vocabulary.
    ///
    /// Beside [`delivery`](Self::delivery) and for the same reason: by the
    /// time a failure is prose, a refused destination and a dead socket read
    /// identically, and they are opposite answers to whether a retry can
    /// succeed. The distinction exists exactly once, here.
    ///
    /// A [`Refused`](HttpError::Refused) is [`ErrorKind::Invalid`] and not
    /// [`ErrorKind::Refused`], whose name is the near miss to watch for:
    /// `ErrorKind::Refused` means *the provider* refused the account, and
    /// nothing here reached a provider. An egress refusal is ego's own policy
    /// answering a question about the request's destination — which is a
    /// property of the request, will be the same on every retry, and belongs
    /// in front of the operator as a usage error rather than as an outage.
    pub fn kind(&self) -> ErrorKind {
        match self {
            HttpError::Refused { .. } => ErrorKind::Invalid,
            HttpError::Transport { .. } => ErrorKind::Transient,
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Refused { refused, .. } => write!(f, "{refused}"),
            HttpError::Transport { message, .. } => write!(f, "transport error: {message}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// The transport seam (`ARCHITECTURE.md` §3.1).
///
/// An implementation that opens sockets **must** admit its own target under
/// its own policy, and must do so again for every redirect hop. That
/// obligation sits here rather than on callers deliberately: an earlier draft
/// of this module made the check unforgeable by handing `execute` a sealed
/// `Destination` token that only [`EgressPolicy::admit`] could mint. It was
/// dropped because it moved the check back to the caller while looking like
/// it had moved it to the seam — and because a token minted under one policy
/// could be presented to a transport built with another, which is a hole a
/// self-check does not have. `--offline` is a property of the transport, so
/// the transport is what checks.
pub trait Http: Send + Sync {
    fn execute(&self, req: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>>;
}

/// A resolver that answers only for `localhost`, from a constant.
///
/// Installed under [`EgressPolicy::LoopbackOnly`] so that the offline path
/// performs no DNS query at all — not even for `localhost`, whose system
/// resolution can be redirected through `/etc/hosts` or a search domain.
/// Every other name is refused here as well as at admission: this is the
/// layer that makes "sends no DNS query" true rather than merely intended.
///
/// Its *installation* has no test, and that is not an oversight. Deleting
/// the `dns_resolver` call below changes nothing observable: admission has
/// already refused every name except `localhost`, and `localhost` resolves to
/// loopback through the system too, so the two paths are indistinguishable
/// from outside while the first layer holds. That is what a second layer of
/// defence *is* — a test that could see it would prove it was not redundant.
/// The risk worth guarding is the two layers disagreeing, and they cannot:
/// both ask [`is_localhost`], and a change there fails tests on both sides.
#[derive(Debug)]
struct LoopbackResolver;

impl reqwest::dns::Resolve for LoopbackResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        // The port is filled in by the connector from the URL; hyper's own
        // resolvers return zero here for the same reason.
        let answer: Result<Vec<SocketAddr>, EgressRefused> = if is_localhost(name.as_str()) {
            Ok(vec![SocketAddr::new(IpAddr::from([127, 0, 0, 1]), 0)])
        } else {
            Err(EgressRefused::NameNeedsResolution {
                host: name.as_str().to_string(),
            })
        };
        Box::pin(async move {
            match answer {
                Ok(addrs) => {
                    let iter: reqwest::dns::Addrs = Box::new(addrs.into_iter());
                    Ok(iter)
                }
                Err(refused) => Err(Box::new(refused) as Box<dyn std::error::Error + Send + Sync>),
            }
        })
    }
}

/// The real transport.
///
/// Construction takes the policy because two of the three ways a host-string
/// check gets defeated are decided when the client is built, not when a
/// request is made: whether a proxy is consulted, and what happens on a
/// redirect. A policy handed to `execute` instead of to `new` would arrive
/// too late for both.
#[derive(Debug)]
pub struct ReqwestHttp {
    policy: EgressPolicy,
    client: reqwest::Client,
}

impl ReqwestHttp {
    /// Builds a transport that enforces `policy`.
    ///
    /// Under [`EgressPolicy::LoopbackOnly`] this: refuses every proxy, so
    /// `HTTP_PROXY` cannot redirect an admitted loopback request (the
    /// `system-proxy` feature is also off in `Cargo.toml`, so the
    /// environment is not read at all — `tests/http.rs` pins that); resolves
    /// through [`LoopbackResolver`], so no DNS query leaves; and re-runs
    /// `admit` on every redirect hop, so an admitted server cannot forward
    /// the connection off-box with a `Location` header.
    pub fn new(policy: EgressPolicy) -> Result<Self, HttpError> {
        let mut builder = reqwest::Client::builder();
        builder = match policy {
            EgressPolicy::Unrestricted => builder,
            EgressPolicy::LoopbackOnly => {
                builder.no_proxy().dns_resolver(Arc::new(LoopbackResolver))
            }
        };
        let client = builder
            .redirect(reqwest::redirect::Policy::custom(
                move |attempt| match policy.admit(attempt.url()) {
                    Ok(_) => attempt.follow(),
                    Err(refused) => attempt.error(refused),
                },
            ))
            .build()
            .map_err(|e| HttpError::Transport {
                delivery: delivery_of(&e),
                message: e.to_string(),
            })?;
        Ok(Self { policy, client })
    }
}

/// Recovers the typed [`EgressRefused`] a redirect refusal was built from.
///
/// `reqwest` boxes the error a redirect policy returns, so without this the
/// refusal would come back as prose and criterion "a refusal names which
/// destination and why" would hold for the first hop but not the second.
fn refusal_in(err: &reqwest::Error) -> Option<EgressRefused> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(err);
    while let Some(cause) = source {
        if let Some(refused) = cause.downcast_ref::<EgressRefused>() {
            return Some(refused.clone());
        }
        source = cause.source();
    }
    None
}

/// Reads whether `err` happened before the request bytes went out.
///
/// Two kinds are unsent with certainty. `is_builder` never touched the
/// network at all. `is_connect` is `reqwest` walking the source chain for
/// `hyper_util`'s `Kind::Connect`, which is raised by the *connector* — no
/// TCP handshake completed, so no request line was written. Everything else
/// — a timeout, a reset, a body that stopped arriving, a redirect loop — is
/// [`DeliveryCertainty::PossiblySent`], because the local side genuinely
/// cannot tell an error before the bytes left from one after.
///
/// One known way `is_connect` understates: `hyper_util`'s pooled client
/// retries a request whose connection was closed before any response
/// arrived, and if that *second* attempt fails to connect the error is
/// `Kind::Connect` even though the first attempt's bytes may have been
/// processed. Removing it means disabling connection reuse
/// (`pool_max_idle_per_host(0)`), which costs a TLS handshake on every
/// generation of an agent loop — too much for a race this narrow. Recorded
/// rather than papered over; see `POTENTIALS.md`.
fn delivery_of(err: &reqwest::Error) -> DeliveryCertainty {
    if err.is_builder() || err.is_connect() {
        DeliveryCertainty::DefinitelyUnsent
    } else {
        DeliveryCertainty::PossiblySent
    }
}

fn to_http_error(err: reqwest::Error) -> HttpError {
    // `delivery_of` is asked in both arms, and that is the point. A refusal
    // that surfaces here is not the pre-flight one: it came either from the
    // resolver, which fails the *connector* and so reads as `is_connect`, or
    // from the redirect policy, which fires only after the original request
    // was written and answered. One of those is unsent and the other is not,
    // and the word "refused" does not distinguish them.
    let delivery = delivery_of(&err);
    match refusal_in(&err) {
        Some(refused) => HttpError::Refused { refused, delivery },
        None => HttpError::Transport {
            delivery,
            message: err.to_string(),
        },
    }
}

impl Http for ReqwestHttp {
    fn execute(&self, req: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        // The first of the three enforcement points. It runs before anything
        // is built, so a refusal here costs no DNS query and no socket.
        if let Err(refused) = self.policy.admit(&req.url) {
            return Box::pin(async move { Err(HttpError::refused_before_sending(refused)) });
        }
        let method = match req.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
        };
        let mut builder = self.client.request(method, req.url);
        for (name, value) in req.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = req.body {
            builder = builder.body(body);
        }
        Box::pin(async move {
            let response = builder.send().await.map_err(to_http_error)?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_string(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let body = futures::StreamExt::map(response.bytes_stream(), |chunk| {
                chunk.map_err(to_http_error)
            });
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(body),
            })
        })
    }
}

/// [`LoopbackResolver`] is private, and it is the second of the three
/// enforcement points, so it needs a test that can see it. From
/// `tests/http.rs` it is invisible — and worse, indistinguishable: a request
/// to `localhost` succeeds whether this resolver answered or the system one
/// did, so an integration test could only claim the resolver was used, never
/// show it.
#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::dns::{Name, Resolve};
    use std::str::FromStr;

    async fn resolve(host: &str) -> Result<Vec<SocketAddr>, String> {
        LoopbackResolver
            .resolve(Name::from_str(host).expect("a valid DNS name"))
            .await
            .map(|addrs| addrs.collect())
            .map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn the_offline_resolver_answers_for_localhost_from_a_constant() {
        for spelling in ["localhost", "localhost."] {
            let addrs = resolve(spelling)
                .await
                .unwrap_or_else(|e| panic!("{spelling} should resolve: {e}"));
            assert_eq!(
                addrs.iter().map(|a| a.ip()).collect::<Vec<_>>(),
                vec![IpAddr::from([127, 0, 0, 1])],
                "{spelling} must map to the loopback address and nothing else"
            );
        }
    }

    /// The name the admission table already refuses. It is refused here too,
    /// because a single check is a single place to get it wrong — and because
    /// this is the layer that makes "no DNS query leaves" a fact about the
    /// program rather than a claim about its callers.
    #[tokio::test]
    async fn the_offline_resolver_refuses_every_other_name_rather_than_asking() {
        for host in ["api.openai.com", "evil.localhost", "localhost.evil.com"] {
            let err = resolve(host)
                .await
                .expect_err("{host} must not be resolved offline");
            assert!(
                err.contains(host),
                "the refusal must name {host}, got: {err}"
            );
        }
    }
}
