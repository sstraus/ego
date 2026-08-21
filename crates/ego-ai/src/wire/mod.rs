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

use crate::http::{HttpError, HttpRequest, HttpResponse};
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
    /// The complete HTTP request for `req` against `endpoint`.
    ///
    /// `headers` is the profile's contribution — the credential above all —
    /// because a wire format is not allowed to know one
    /// (`ARCHITECTURE.md` §3.1). A mapping adds the headers that are facts
    /// about *the body it is about to write*, and no others.
    fn build_request(
        &self,
        endpoint: Url,
        headers: Vec<(String, String)>,
        req: &GenRequest,
    ) -> Result<HttpRequest, Unexpressible>;

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

/// A [`GenRequest`] a wire format cannot express without lying about it.
///
/// One type at the seam, and a precise enum inside each mapping that converts
/// into it. The two are not the same thing and must not be fused: what a
/// format cannot say is a fact about *that* format —
/// `openai-completions` has nowhere to put reasoning state, and Anthropic's
/// Messages API has nowhere to put an absent `max_tokens` — so a shared enum
/// would give every mapping variants that mean nothing for it, and a per-
/// mapping enum at the seam would make the caller match on which mapping it
/// happened to be holding.
///
/// A rendered sentence and not a code, because there is exactly one caller —
/// `WireGenerator`, which turns it into
/// [`failed`] with [`ErrorKind::Invalid`] — and it wants the
/// sentence. A discriminant nothing reads is a field nothing can be wrong
/// about, so it arrives when a reader does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unexpressible(String);

impl Unexpressible {
    pub fn new(reason: impl Into<String>) -> Self {
        Unexpressible(reason.into())
    }
}

impl fmt::Display for Unexpressible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unexpressible {}

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
