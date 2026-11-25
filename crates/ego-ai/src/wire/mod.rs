//! Wire formats: one module per API shape (`ARCHITECTURE.md` §3.1).
//!
//! A wire format turns a provider-neutral `GenRequest` into bytes and turns
//! the bytes back into [`AiEvent`](ego_core::generator::port::AiEvent)s. It
//! owns no socket, no credential and no policy — those belong to
//! [`crate::http`] and to the provider table — so a wire format is testable
//! against a transcript, which is what the goldens here do. A wire format also
//! decides nothing about a malformed answer: the shared rules in [`policy`]
//! and the generator's tool-name stage do, once, for every adapter.
//!
//! [`WireFormat`] is the seam, and it exists now because both conditions its
//! absence was waiting on have been met.
//! the design notes §1 retired the
//! original reason — a trait extracted from one implementation invents the
//! second one's requirements — when support expanded to six providers, and
//! §6 put the extraction behind stories 032 and 035 because those move
//! `GenRequest` and `AiEvent`, the types the signature is made of. Both have
//! landed, so the signature is written against types that have stopped moving.

pub mod anthropic_messages;
pub mod bedrock_converse;
mod cache;
pub mod chat_max_tokens;
pub mod deepseek;
mod failure_capture;
pub mod gemini_interactions;
pub mod github_copilot_completions;
pub mod kimi_anthropic;
pub mod kimi_api;
pub mod kimi_openai;
pub mod kimi_responses;
pub mod minimax_anthropic;
pub mod mistral_conversations;
pub mod nous_portal;
pub mod ollama;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_responses;
pub mod partner_messages;
mod policy;
#[cfg(test)]
mod policy_contract;
pub mod provider_cost;
mod responses_search;
pub mod sse;
pub mod xai_responses;
pub mod zai;

use std::fmt;
use std::sync::Arc;

use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ContentBlock, ErrorKind, GenRequest, StopReason};
use futures::stream::{self, BoxStream, StreamExt};
use serde::Deserialize;
use url::Url;

use crate::http::{HttpError, HttpRequest, HttpResponse, Method};
use provider_cost::ProviderCost;

pub(crate) fn request_has_images(req: &GenRequest) -> bool {
    req.messages
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Image { .. }))
}

pub(crate) fn request_has_audio(req: &GenRequest) -> bool {
    req.messages
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Audio { .. }))
}

/// One API shape: how a [`GenRequest`] becomes bytes, and how the bytes become
/// [`AiEvent`]s.
///
/// Two methods and no third, because the ADR's §2 split leaves a mapping with
/// exactly two jobs. Everything a request needs that is *not* a fact about the
/// shape — the address, the credential, whether this provider states a
/// monetary total — arrives as a parameter from the profile. Providers whose
/// current bytes agree reuse one value; a real request or event difference is
/// represented by a distinct value rather than a provider branch inside it.
///
/// `Debug` because a [`ProviderRow`](crate::provider::ProviderRow) is
/// `Debug` and a row worth logging must not lose which shape it speaks.
/// `Send + Sync` because a row is shared across a generation's tasks.
pub trait WireFormat: fmt::Debug + Send + Sync {
    /// Actual encoder/decoder identity, independent of provider aliases.
    fn name(&self) -> &'static str {
        std::any::type_name::<Self>()
            .rsplit("::")
            .next()
            .unwrap_or("unknown")
    }

    /// Lifetime selected by this mapping, not an invented response field.
    fn prompt_cache_policy(&self, _req: &GenRequest) -> ego_core::usage::PromptCachePolicy {
        ego_core::usage::PromptCachePolicy::default()
    }

    /// Whether this mapping encodes and decodes native hosted web search.
    /// Credential availability and permission admission remain separate.
    fn supports_web_search(&self) -> bool {
        false
    }

    /// Whether the resolved route offers native web search by default.
    /// A compatible encoder alone does not opt a custom endpoint in.
    fn supports_default_web_search(
        &self,
        _model: &str,
        _base: &Url,
        _credential: &crate::provider::ResolvedCredential,
        _strategy: &crate::provider::CredentialStrategy,
    ) -> bool {
        false
    }

    /// Only mappings with a proven bidirectional contract opt in.
    fn websocket(&self) -> bool {
        false
    }

    /// The complete HTTP request for `req` against `target`.
    ///
    /// A [`RequestTarget`] and not a URL, and that is the whole enforcement
    /// mechanism for "no per-provider branch in shared code": the target
    /// carries a base and a list of headers and **no provider identity**, so
    /// a mapping that wanted to branch on who it serves has nothing to branch
    /// on (the design notes
    /// §2). A provider that needs a different body is a different value of
    /// this trait — §4 allows a model fact, a transport fact or another
    /// mapping, and no fourth option and no flag.
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused>;

    /// Digest only a cache-marked prefix that this mapping can identify in
    /// its actual request bytes. Unsupported mappings report no continuity.
    fn cache_prefix(&self, _body: &[u8]) -> Option<String> {
        None
    }

    /// Durable continuity evidence from final request bytes. Only mappings
    /// with a proven input-array contract opt in; this is not `cache_prefix`.
    fn cache_request_identity(
        &self,
        _body: &[u8],
        _scope: String,
        _previous_input_items: Option<usize>,
    ) -> Option<ego_core::usage::CacheRequestIdentity> {
        None
    }

    /// One provider response, decoded into the generation's events.
    ///
    /// Takes the `Result` rather than the response so a request that never got
    /// one is the same kind of thing as one that failed halfway. `cost` is the
    /// profile's own declaration of where it states a charge, carried in
    /// rather than looked up here — the metering ADR §8 forbids a `match` on a
    /// provider id inside a mapping, and a parameter is how that stays true.
    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent>;

    /// Decode a live generation with any request-specific response policy.
    fn decode_generation(
        &self,
        response: Result<HttpResponse, HttpError>,
        cost: Option<Arc<dyn ProviderCost>>,
        _search: Option<ego_core::generator::types::HostedSearch>,
    ) -> BoxStream<'static, AiEvent> {
        self.decode(response, cost)
    }
}

/// Everything a mapping needs about where a request goes.
///
/// Deliberately carries no provider id: a mapping that cannot learn who it is
/// serving cannot branch on it, and "no per-provider branch in shared code"
/// stops being a rule anyone has to remember
/// (the design notes
/// §2).
///
/// The fields are private and the ADR's sketch has them public. The data and
/// its visibility to a mapping are identical either way; what the accessors
/// buy is that [`url`](Self::url) is the **only** way to reach a URL from
/// here, so every mapping joins its path to the base the same way. A mapping
/// holding the base directly would be a mapping that could `set_path` it, and
/// the difference between `join` and `set_path` on a base with a prefix is a
/// request sent somewhere unintended.
///
/// **A guardrail, not an enforcement**, and the distinction is worth stating
/// because the stronger claim is inviting and false. `join` itself accepts an
/// absolute path, a `../` that climbs out of the prefix, and a whole other
/// URL; and a mapping builds an [`HttpRequest`] itself, so it can put any URL
/// in one whatever this type offers. What privacy buys is that the *accident*
/// cannot happen — a mapping cannot reach for `set_path` on a base it never
/// holds. A mapping determined to send its request elsewhere is not what this
/// prevents, and nothing at this seam could.
pub struct RequestTarget<'a> {
    base: &'a Url,
    headers: &'a [ego_core::permit::HttpHeader],
}

impl<'a> RequestTarget<'a> {
    /// `base` is the row's, ending in `/` — see
    /// [`ProviderRow::base`](crate::provider::ProviderRow::base), which is
    /// where that is guaranteed. `headers` is the profile's contribution, the
    /// credential above all, and it is opaque here because a wire format is
    /// not allowed to know a credential (`ARCHITECTURE.md` §3.1).
    pub fn new(base: &'a Url, headers: &'a [ego_core::permit::HttpHeader]) -> Self {
        RequestTarget { base, headers }
    }

    /// The row's base with this mapping's own path joined onto it.
    ///
    /// `join` and not `set_path`, which is absolute and would discard any
    /// prefix the base carries — `https://gateway.example/openai/v1/` is a
    /// real deployment shape, and `set_path` would send its requests to
    /// `https://gateway.example/chat/completions`.
    pub fn url(&self, path: &str) -> Url {
        self.base
            .join(path)
            .expect("a relative path joined onto a base URL is a URL")
    }

    /// The profile's headers, verbatim and opaque.
    ///
    /// For a mapping whose request is not one JSON body and which therefore
    /// cannot use [`post_json`](Self::post_json). Reading them is allowed;
    /// interpreting them is not — a mapping that looked for a credential in
    /// here would be a mapping that knows what a credential is.
    pub fn headers(&self) -> &[ego_core::permit::HttpHeader] {
        self.headers
    }

    /// A JSON body posted to this target's `path`, asking for an event stream
    /// back.
    ///
    /// The two headers it adds are facts about *what is being sent and what is
    /// wanted back*, which is the only kind of header a mapping is allowed to
    /// add (`ARCHITECTURE.md` §3.1). It lives here rather than in each mapping
    /// because it is the same two headers for every streaming JSON API, and a
    /// mapping that had to remember them is a mapping that can forget one.
    pub fn post_json(&self, path: &str, body: Vec<u8>) -> HttpRequest {
        // Explicit opt-in only: request bodies can contain private prompt text.
        // Never include headers, which carry credentials.
        tracing::debug!(target: "ego::body", path, body = %String::from_utf8_lossy(&body), "provider request");
        let mut headers = Vec::with_capacity(self.headers.len() + 2);
        headers.push(
            ego_core::permit::HttpHeader::new("content-type", b"application/json".to_vec())
                .expect("a static wire header is valid"),
        );
        headers.push(
            ego_core::permit::HttpHeader::new("accept", b"text/event-stream".to_vec())
                .expect("a static wire header is valid"),
        );
        headers.extend(self.headers.iter().cloned());
        HttpRequest {
            url: self.url(path),
            method: Method::Post,
            headers,
            body: Some(body),
        }
    }
}

/// A [`GenRequest`] a wire format cannot express without lying about it.
///
/// One type at the seam, and a precise enum inside each mapping that converts
/// into it. The two are not the same thing and must not be fused: what a
/// format cannot say is a fact about *that* format —
/// `openai-completions` has nowhere to put reasoning state, and Anthropic's
/// Messages API has nowhere to put an absent `max_tokens` — so a shared enum
/// would give every mapping variants that mean nothing for it, and a per-
/// mapping enum at the seam would make the caller match on which mapping it
/// happened to be holding. The mapping-local enums are named `Inexpressible`
/// for that reason: this type says a request was *refused*, and theirs
/// enumerate what their own format cannot *say*.
///
/// A rendered sentence and not a code, because there is exactly one caller —
/// `WireGenerator`, which turns it into
/// [`failed`] with [`ErrorKind::Invalid`] — and it wants the
/// sentence. A discriminant nothing reads is a field nothing can be wrong
/// about, so it arrives when a reader does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRefused(String);

impl RequestRefused {
    pub fn new(reason: impl Into<String>) -> Self {
        RequestRefused(reason.into())
    }
}

impl fmt::Display for RequestRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RequestRefused {}

/// The events a generation that produced no terminal frame still owes.
///
/// Three of them, always in this order, and exactly once: what the transport
/// read about delivery, what went wrong, and the fact that it is over. An
/// implementation that returned nothing here would settle as
/// [`StopReason::Aborted`] with a missing usage report — which reads as "the
/// provider was called and said nothing", and is a different event from "the
/// request was refused before a socket existed".
///
/// It lives here rather than inside one wire format because three call sites
/// need it — a transport error, a non-200 status, and a request the format
/// cannot express at all — and three hand-written copies of a three-event
/// shape is three chances for them to drift apart.
///
/// `kind` is a parameter and not a constant because each of the three call
/// sites knows something this function cannot see: the transport knows
/// whether a socket died or a policy refused, and only the status line knows
/// what the provider thought of the request. A default here would be one of
/// those three answers imposed on the other two.
pub fn failed(message: String, delivery: DeliveryCertainty, kind: ErrorKind) -> [AiEvent; 3] {
    [
        AiEvent::DeliveryObserved(delivery),
        AiEvent::Error { message, kind },
        AiEvent::Stop(StopReason::Error),
    ]
}

// --- what every streaming mapping does with a response, once ---

/// One mapping's stream decoder, as the two operations [`decode_response`]
/// performs on it.
///
/// A trait rather than a third copy of the same eight-line state machine. The
/// mappings differ in what a *frame* means — `openai-completions` has a
/// `[DONE]` sentinel and `anthropic-messages` has a terminal event — and agree
/// completely about what a *response* means: a transport error is three
/// events, a non-200 is those three plus its host-safe cause, and a 200 is
/// bytes fed to a decoder until they run out. That agreement is not a coincidence to be re-derived
/// per mapping; it is `AttemptStream`'s contract, and one implementation of it
/// is one place to get it wrong.
///
/// `finish` takes `Box<Self>` because a decoder is consumed by ending — a
/// decoder that could be pushed to after its terminal frame is a decoder that
/// could emit two `Stop`s.
pub(crate) trait StreamSink: Send + 'static {
    fn push(&mut self, chunk: &[u8]) -> Vec<AiEvent>;
    fn finish(self: Box<Self>, transport: Option<HttpError>) -> Vec<AiEvent>;
    /// True if `push` emitted a terminal stop and the body must be dropped.
    fn stop_reading(&self) -> bool {
        false
    }
    /// Whether the wire's usage report did not partition its own input total.
    /// The adapter states the fact; [`policy`] decides what it means.
    fn usage_incoherent(&self) -> bool {
        false
    }
}

/// End the decoder, with the shared notice when its usage was incoherent.
fn finish(decoder: Box<dyn StreamSink>, transport: Option<HttpError>) -> Vec<AiEvent> {
    let incoherent = decoder.usage_incoherent();
    let mut events = decoder.finish(transport);
    if incoherent {
        events.insert(0, policy::incoherent_usage());
    }
    events
}

enum State {
    Head {
        response: Result<HttpResponse, HttpError>,
        decoder: Box<dyn StreamSink>,
    },
    Body {
        body: crate::http::ByteStream,
        decoder: Box<dyn StreamSink>,
        capture: Option<failure_capture::FailureCapture>,
    },
    Done,
}

/// Decode one provider response into the generation's events.
///
/// Takes the `Result` rather than the response so that a request that never
/// got a response is the same kind of thing as one that failed halfway: both
/// end in exactly one `Stop`, and both report their [`DeliveryCertainty`] from
/// the transport that watched them.
///
/// The decoder is built by the caller and handed over already configured,
/// because what a mapping needs to construct one differs — the family's takes
/// the row's cost decoder, and Anthropic's takes nothing — and that difference
/// has no business in here.
pub(crate) fn decode_response(
    response: Result<HttpResponse, HttpError>,
    decoder: Box<dyn StreamSink>,
) -> BoxStream<'static, AiEvent> {
    let stream_retry_after = response
        .as_ref()
        .ok()
        .filter(|response| response.status == 200)
        .and_then(|response| retry_after(&response.headers));
    stream::unfold(State::Head { response, decoder }, |state| async move {
        match state {
            State::Head {
                response: Err(error),
                ..
            } if error.is_protocol_filter() => Some((local_filter_failure(&error), State::Done)),
            State::Head {
                response: Err(error),
                ..
            } => Some((
                // Both readings come off the transport error, and neither is
                // recoverable from the prose it renders to — see
                // `HttpError::kind`.
                failed(error.to_string(), error.delivery(), error.kind()).to_vec(),
                State::Done,
            )),
            State::Head {
                response: Ok(response),
                decoder,
            } => {
                // Header names only, never values: they identify a provider's
                // quota family without exposing account data.
                tracing::debug!(
                    target: "ego_ai::provider_status",
                    status = response.status,
                    header_names = ?response.headers.iter().map(|header| header.name()).collect::<Vec<_>>(),
                    "provider HTTP response"
                );
                // A status at all proves the request arrived, whatever the
                // status says. This is the earliest honest reading, and it is
                // the one that matters: it is taken before anything can cut
                // the body and leave nobody to report it.
                let arrived = AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent);
                if response.status == 200 {
                    let capture =
                        failure_capture::FailureCapture::start(response.status, &response.headers);
                    return Some((
                        vec![arrived],
                        State::Body {
                            body: response.body,
                            decoder,
                            capture,
                        },
                    ));
                }
                let retry_after = retry_after(&response.headers);
                let diagnostic = match read_error_body(response.status, response.body).await {
                    Ok(diagnostic) => diagnostic,
                    Err(error) => return Some((local_filter_failure(&error), State::Done)),
                };
                // A documented quota refusal overrides the status class: it
                // shares 429 (or 400) with conditions that clear, and it never
                // does. Anything else keeps the status-only class.
                let (kind, capacity) = if diagnostic.quota {
                    (ErrorKind::Refused, false)
                } else {
                    (
                        classify_status(response.status),
                        matches!(response.status, 429 | 503 | 529),
                    )
                };
                let (message, cause) = (diagnostic.cause.clone(), diagnostic.cause);
                // The same three events, with the certainty already read off
                // the status above rather than off a failure: a response at
                // all proves the request arrived. The provider answered, so
                // its host-safe cause goes before the terminal `Stop`.
                let mut events = failed(message, DeliveryCertainty::PossiblySent, kind).to_vec();
                events.insert(2, AiEvent::ProviderFailure {
                        cause,
                        retry_after,
                        capacity,
                    },
                );
                Some((events, State::Done))
            }
            State::Body {
                mut body,
                mut decoder,
                mut capture,
            } => match body.next().await {
                Some(Ok(bytes)) => {
                    let events = decoder.push(&bytes);
                    if let Some(capture) = capture.as_mut() {
                        capture.observe(&bytes);
                    }
                    let capture = capture.and_then(|capture| capture.settle(&events));
                    if decoder.stop_reading() {
                        return Some((events, State::Done));
                    }
                    Some((
                        events,
                        State::Body {
                            body,
                            decoder,
                            capture,
                        },
                    ))
                }
                Some(Err(error)) if error.is_protocol_filter() => {
                    let mut events = finish(decoder, None);
                    // The decoder's own stream-ended error and provider failure
                    // describe a fault the provider did not have.
                    events.retain(|event| {
                        !matches!(
                            event,
                            AiEvent::Stop(_) | AiEvent::Error { .. } | AiEvent::ProviderFailure { .. }
                        )
                    });
                    events.extend(local_filter_failure(&error));
                    Some((events, State::Done))
                }
                Some(Err(error)) => {
                    let events = finish(decoder, Some(error));
                    if let Some(capture) = capture {
                        capture.settle(&events);
                    }
                    Some((events, State::Done))
                }
                None => {
                    let events = finish(decoder, None);
                    if let Some(capture) = capture {
                        capture.settle(&events);
                    }
                    Some((events, State::Done))
                }
            },
            State::Done => None,
        }
    })
    .flat_map(stream::iter)
    .map(move |mut event| {
        if let AiEvent::ProviderFailure { retry_after, .. } = &mut event
            && retry_after.is_none()
        {
            *retry_after = stream_retry_after;
        }
        event
    })
    .boxed()
}

fn local_filter_failure(error: &HttpError) -> Vec<AiEvent> {
    let diagnostic = error.to_string();
    vec![
        AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent),
        AiEvent::Error {
            message: diagnostic.clone(),
            kind: error.kind(),
        },
        AiEvent::LocalFailure { cause: diagnostic },
        AiEvent::Stop(StopReason::Error),
    ]
}

/// What a non-200 status says about whether the request can ever succeed.
///
/// **The status and not the body.** A body is not one schema — OpenAI's
/// envelope, xAI's flatter one, Anthropic's typed one, and a proxy's HTML
/// `502` that parses as none of them — while the status line is the one field
/// every provider sets, set by something that cannot have been truncated.
/// [`read_error_body`] keeps the body for the operator to read; this decides
/// what the harness does.
///
/// Every arm is a status these APIs actually return, and the fall-through is
/// [`ErrorKind::Unclassified`] on the fail-closed rule: a status nobody has
/// mapped is not a status that said "retryable". The cost of being wrong here
/// is one attempt, and the cost of guessing "transient" is `--retry N`
/// attempts and an exit code that tells a script to try again.
///
/// **The known gap: `429`.** A rate limit and an exhausted quota can share it,
/// and they are opposite answers — the first clears on its own, the second
/// never does. Only the body distinguishes them, and only on the rows that
/// publish a discriminator. `Transient` is the right reading of the status
/// alone, and the wrong reading of an empty wallet. `POTENTIALS.md` P26, whose
/// trigger is an OpenAI credential existing.
///
/// **What this does not do.** It classifies the non-200 path and only that. A
/// row can answer `200` to a request that was wrong — Ollama accepts `role:
/// "wizard"`, a negative `max_tokens` and a tool with no function body, and
/// generates a normal completion for each — and none of those reaches this
/// function, because nothing about them is a failure as far as the transport
/// is concerned. No status table closes that gap.
///
/// Shared by every mapping rather than copied into each: the four classes are
/// `ego-core`'s, the statuses are HTTP's, and neither is a fact about a wire
/// shape. Anthropic's own list lands on it exactly — `402 billing_error` is
/// `Refused` and `529 overloaded_error` is `Transient` — which is evidence
/// that the table is about statuses and not about OpenAI.
pub(crate) fn classify_status(status: u16) -> ErrorKind {
    match status {
        // The provider is momentarily unable. Nothing about the request is
        // wrong, so the same bytes may work on the next attempt.
        408 | 409 | 429 | 500..=599 => ErrorKind::Transient,
        // The account or the credential, never the request: unauthenticated,
        // forbidden, out of credit.
        401..=403 => ErrorKind::Refused,
        // The request is wrong and will be wrong again — a malformed body, a
        // model that does not exist, a prompt over the limit, a parameter this
        // row does not accept.
        400 | 404 | 413 | 422 => ErrorKind::Invalid,
        _ => ErrorKind::Unclassified,
    }
}

/// Shared provider retry timing: milliseconds, delta seconds, or HTTP-date.
/// The turn owner enforces its wait ceiling and never shortens this value.
fn retry_after(headers: &[ego_core::permit::HttpHeader]) -> Option<std::time::Duration> {
    retry_after_at(headers, std::time::SystemTime::now())
}

fn retry_after_at(
    headers: &[ego_core::permit::HttpHeader],
    now: std::time::SystemTime,
) -> Option<std::time::Duration> {
    let value = |name: &str| {
        headers
            .iter()
            .find(|header| header.name().eq_ignore_ascii_case(name))
            .and_then(|header| std::str::from_utf8(header.value()).ok())
            .map(str::trim)
    };
    let seconds = |text: &str| {
        text.parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n > 0.0)
            .and_then(|n| std::time::Duration::try_from_secs_f64(n).ok())
    };
    if let Some(delay) = value("retry-after-ms")
        .and_then(seconds)
        .map(|delay| delay.div_f64(1_000.0))
    {
        return Some(delay);
    }
    let text = value("retry-after")?;
    seconds(text).or_else(|| {
        httpdate::parse_http_date(text)
            .ok()?
            .duration_since(now)
            .ok()
            .filter(|delay| !delay.is_zero())
    })
}

/// What a streamed Responses error `code` says about retrying.
///
/// Shared by the three Responses mappings, which publish one code vocabulary.
/// Only the codes OpenAI documents for a momentary condition are `Transient`;
/// any other code, and a missing one, stays `Unclassified` on the same
/// fail-closed rule as [`classify_status`]. A documented quota code
/// ([`QUOTA_CODES`]) is `Refused`: it shares a status with a rate limit and
/// never clears by waiting.
///
/// Only an overload and a rate limit are a capacity refusal; a generic
/// `server_error` retries on the seconds-long backoff.
pub(crate) fn classify_stream_code(code: Option<&str>) -> FailureClass {
    match code {
        Some(code) if QUOTA_CODES.contains(&code) => FailureClass {
            kind: ErrorKind::Refused,
            capacity: false,
        },
        Some("server_is_overloaded" | "rate_limit_exceeded") => FailureClass {
            kind: ErrorKind::Transient,
            capacity: true,
        },
        Some("server_error") => FailureClass {
            kind: ErrorKind::Transient,
            capacity: false,
        },
        _ => FailureClass {
            kind: ErrorKind::Unclassified,
            capacity: false,
        },
    }
}

/// What a streamed provider error says about retrying: its class, and whether
/// it is a capacity refusal ([`AiEvent::ProviderFailure`]'s `capacity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FailureClass {
    pub(crate) kind: ErrorKind,
    pub(crate) capacity: bool,
}

/// The two events a streamed provider error emits: the log's `Error`, and the
/// host-safe [`AiEvent::ProviderFailure`]. `message` is the provider's typed
/// `message` field (or a sentence naming its code), never a raw body, so it
/// serves as both.
pub(crate) fn streamed_failure(message: String, class: FailureClass) -> [AiEvent; 2] {
    [
        AiEvent::Error {
            message: message.clone(),
            kind: class.kind,
        },
        AiEvent::ProviderFailure {
            cause: message,
            retry_after: None,
            capacity: class.capacity,
        },
    ]
}

/// How much of a non-200 body is read before giving up on it.
///
/// An error body is a small JSON object. A cap is here because the far side
/// having gone wrong is exactly when "it will be small" stops being a
/// guarantee.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// `{"error": {"message": …}}` — the envelope OpenAI defined, which
/// OpenRouter copied and which Anthropic's typed error body also satisfies.
///
/// Only typed diagnostic fields cross this boundary. An unrecognized body
/// stays private: gateways can return request bodies or credentials in it.
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

/// xAI and Ollama also publish a flat, typed error string. Recognize that
/// message without falling back to copying its surrounding response body.
#[derive(Debug, Deserialize)]
struct FlatErrorEnvelope {
    error: String,
    #[serde(default)]
    code: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    message: String,
    // Optional fields are inspected separately so an unknown field shape cannot
    // discard an otherwise useful typed message.
    #[serde(default)]
    code: serde_json::Value,
    #[serde(default, rename = "type")]
    kind: serde_json::Value,
    #[serde(default)]
    details: serde_json::Value,
    #[serde(default)]
    metadata: serde_json::Value,
}

/// Provider codes that document an exhausted account quota, credit or spend
/// limit. Each shares a status with a transient failure (usually 429) and
/// none clears by waiting a few seconds. Sources, read 2026-11-08: OpenAI
/// error codes guide; Anthropic spend cap (`error.details.error_code`);
/// Gemini API errors; Anthropic 402 type; the ChatGPT subscription limit in
/// OpenAI's Codex client. `budget_reached` is the ChatGPT backend's session
/// budget, terminal in OpenAI's Codex client (`parse_terminal_provider_error`,
/// codex-rs/codex-api/src/error.rs at rust-v0.168.0, read 2026-11-13).
/// `tests/fixtures/quota/` holds one case per code.
pub(crate) const QUOTA_CODES: [&str; 10] = [
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
    "enforced_spend_limit_reached",
    "quota_exceeded",
    "payment_required",
    "usage_limit_reached",
    "budget_reached",
];

/// Anthropic answers an operator-set spend limit with HTTP 400 and this
/// message prefix (rate-limits guide, "Setting your own spend limit").
const OPERATOR_SPEND_LIMIT: &str = "You have reached your specified";

/// The sentence a quota refusal adds to the host cause.
const QUOTA_HINT: &str = "the account's quota, credit or spend limit is \
     exhausted; retrying will not help until it is raised or resets";

impl ErrorBody {
    /// Whether the typed body documents an exhausted account rather than a
    /// momentary condition. Only exact documented codes and one documented
    /// message prefix count; anything else keeps the status class.
    fn is_quota_refusal(&self, status: u16) -> bool {
        let details = self
            .details
            .get("error_code")
            .unwrap_or(&serde_json::Value::Null);
        [&self.code, &self.kind, details]
            .into_iter()
            .filter_map(serde_json::Value::as_str)
            .any(|code| QUOTA_CODES.contains(&code))
            || (status == 400 && self.message.starts_with(OPERATOR_SPEND_LIMIT))
    }
}

/// Provider-controlled prose is bounded and cannot inject terminal controls.
/// Reuse the existing narrative redactor before truncating, so a bearer token
/// at the boundary is not cut into an unrecognizable credential.
fn error_text(text: &str, limit: usize) -> String {
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    ego_core::permit::shell::redact_credentials(&text)
        .chars()
        .take(limit)
        .collect()
}

fn error_code(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(error_text(text, 64)),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

impl ErrorBody {
    fn diagnostic(&self, status: u16) -> String {
        let mut details = Vec::new();
        if let Some(code) =
            error_code(&self.code).or_else(|| self.details.get("error_code").and_then(error_code))
        {
            details.push(format!("code {code}"));
        }
        for (key, label) in [
            ("provider_name", "upstream"),
            ("error_type", "type"),
            ("provider_code", "provider code"),
        ] {
            if let Some(text) = self.metadata.get(key).and_then(error_code) {
                details.push(format!("{label} {text}"));
            }
        }
        // OpenRouter's raw payload is NOT an excerpt. Parse only a typed
        // upstream error.message, ignoring headers, prompts, request bodies,
        // and every other sibling. Opaque text and malformed JSON stay private.
        let raw = self.metadata.get("raw");
        let parsed = raw
            .and_then(serde_json::Value::as_str)
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
        if let Some(message) = parsed
            .as_ref()
            .or(raw)
            .and_then(|raw| raw.get("error"))
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
        {
            details.push(format!("upstream message: {}", error_text(message, 512)));
        }
        let base = format!(
            "the provider returned HTTP {status}: {}",
            error_text(&self.message, 512)
        );
        if details.is_empty() {
            base
        } else {
            format!("{base} ({})", details.join("; "))
        }
    }
}

/// What a `401` or `403` is told to consider, once ego stopped impersonating.
///
/// ego sends its own identity and never a vendor client's, so "this client is
/// not one of ours" is now a refusal an operator can actually receive — and it
/// arrives as the same status as a wrong key, with a body that rarely says
/// which. Appending the recipe costs one sentence and saves the operator
/// guessing; withholding it would leave the amended ADR's opt-in override
/// discoverable only by reading the source.
///
/// **Appended, never substituted.** What the vendor said comes first and whole:
/// a hint that displaced the body would hide the case where the vendor *did*
/// explain itself.
const IDENTITY_HINT: &str = "if this provider gates on a client identity ego \
     does not ship, see the configuration reference for non-secret identity \
     overrides under `auth.<provider>.headers`";

async fn read_error_body(
    status: u16,
    mut body: crate::http::ByteStream,
) -> Result<ErrorDiagnostic, HttpError> {
    let mut raw = Vec::new();
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(bytes) => {
                raw.extend_from_slice(&bytes);
                if raw.len() >= MAX_ERROR_BODY {
                    raw.truncate(MAX_ERROR_BODY);
                    break;
                }
            }
            // The status is the news; a body that also failed to arrive does
            // not change it.
            Err(error) if error.is_protocol_filter() => return Err(error),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw);
    // The second string is what a host may be shown: the status and the
    // envelope's typed message, never a body that parsed as no envelope.
    let typed = serde_json::from_str::<ErrorEnvelope>(&text)
        .map(|envelope| envelope.error)
        .or_else(|_| {
            serde_json::from_str::<FlatErrorEnvelope>(&text).map(|flat| ErrorBody {
                message: flat.error,
                code: flat.code,
                kind: serde_json::Value::Null,
                details: serde_json::Value::Null,
                metadata: serde_json::Value::Null,
            })
        });
    let quota = typed
        .as_ref()
        .is_ok_and(|body| body.is_quota_refusal(status));
    let cause = match &typed {
        Ok(body) => body.diagnostic(status),
        Err(_) if text.trim().is_empty() => {
            format!("the provider returned HTTP {status} with an empty body")
        }
        Err(_) => format!("the provider returned HTTP {status}"),
    };
    // `402` is deliberately not in the identity hint. It is the third member
    // of the `Refused` class and it means an empty wallet, which no header
    // fixes; a documented quota refusal says so instead.
    let cause = match status {
        401 | 403 => format!("{cause} — {IDENTITY_HINT}"),
        _ if quota => format!("{cause} — {QUOTA_HINT}"),
        _ => cause,
    };
    Ok(ErrorDiagnostic { cause, quota })
}

/// What a non-200 body says: the host-safe cause, and whether it documents an
/// exhausted account quota rather than the status's usual condition.
struct ErrorDiagnostic {
    cause: String,
    quota: bool,
}
