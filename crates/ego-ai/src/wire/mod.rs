//! Wire formats: one module per API shape (`ARCHITECTURE.md` §3.1).
//!
//! A wire format turns a provider-neutral `GenRequest` into bytes and turns
//! the bytes back into [`AiEvent`](ego_core::generator::port::AiEvent)s. It
//! owns no socket, no credential and no policy — those belong to
//! [`crate::http`] and to the provider table — so a wire format is testable
//! against a transcript, which is what the goldens here do.
//!
//! [`WireFormat`] is the seam, and it exists now because both conditions its
//! absence was waiting on have been met.
//! `docs/decisions/2026-08-21-a-profile-is-not-a-protocol.md` §1 retired the
//! original reason — a trait extracted from one implementation invents the
//! second one's requirements — when maintainer set the target at six providers, and
//! §6 put the extraction behind regression case and 035 because those move
//! `GenRequest` and `AiEvent`, the types the signature is made of. Both have
//! landed, so the signature is written against types that have stopped moving.

pub mod ollama;
pub mod openai_completions;
pub mod provider_cost;
pub mod sse;

use std::fmt;
use std::sync::Arc;

use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ErrorKind, GenRequest, StopReason};
use futures::stream::BoxStream;
use url::Url;

use crate::http::{HttpError, HttpRequest, HttpResponse, Method};
use provider_cost::ProviderCost;

/// One API shape: how a [`GenRequest`] becomes bytes, and how the bytes become
/// [`AiEvent`]s.
///
/// Two methods and no third, because the ADR's §2 split leaves a mapping with
/// exactly two jobs. Everything a request needs that is *not* a fact about the
/// shape — the address, the credential, whether this provider states a
/// monetary total — arrives as a parameter from the profile. That is what
/// makes OpenAI, OpenRouter, xAI and Ollama four profiles over one value of
/// this trait rather than four branches inside it.
///
/// `Debug` because a [`ProviderRow`](crate::provider::ProviderRow) is
/// `Debug` and a row worth logging must not lose which shape it speaks.
/// `Send + Sync` because a row is shared across a generation's tasks.
pub trait WireFormat: fmt::Debug + Send + Sync {
    /// The complete HTTP request for `req` against `target`.
    ///
    /// A [`RequestTarget`] and not a URL, and that is the whole enforcement
    /// mechanism for "no per-provider branch in shared code": the target
    /// carries a base and a list of headers and **no provider identity**, so
    /// a mapping that wanted to branch on who it serves has nothing to branch
    /// on (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
    /// §2). A provider that needs a different body is a different value of
    /// this trait — §4 allows a model fact, a transport fact or another
    /// mapping, and no fourth option and no flag.
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused>;

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
}

/// Everything a mapping needs about where a request goes.
///
/// Deliberately carries no provider id: a mapping that cannot learn who it is
/// serving cannot branch on it, and "no per-provider branch in shared code"
/// stops being a rule anyone has to remember
/// (`docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md`
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
    headers: &'a [(String, String)],
}

impl<'a> RequestTarget<'a> {
    /// `base` is the row's, ending in `/` — see
    /// [`ProviderRow::base`](crate::provider::ProviderRow::base), which is
    /// where that is guaranteed. `headers` is the profile's contribution, the
    /// credential above all, and it is opaque here because a wire format is
    /// not allowed to know a credential (`ARCHITECTURE.md` §3.1).
    pub fn new(base: &'a Url, headers: &'a [(String, String)]) -> Self {
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
    pub fn headers(&self) -> &[(String, String)] {
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
        let mut headers = Vec::with_capacity(self.headers.len() + 2);
        headers.push(("content-type".to_string(), "application/json".to_string()));
        headers.push(("accept".to_string(), "text/event-stream".to_string()));
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
