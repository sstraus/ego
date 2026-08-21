//! Wire formats: one module per API shape (`ARCHITECTURE.md` §3.1).
//!
//! A wire format turns a provider-neutral `GenRequest` into bytes and turns
//! the bytes back into [`AiEvent`](ego_core::generator::port::AiEvent)s. It
//! owns no socket, no credential and no policy — those belong to
//! [`crate::http`] and to the provider table — so a wire format is testable
//! against a transcript, which is what the goldens here do.
//!
//! There is no `WireFormat` trait here yet, and the reason has changed. It used
//! to be that extracting one from a single implementation would mean inventing
//! the second implementation's requirements from imagination. That is no longer
//! true — maintainer set the target at six providers, so Anthropic's Messages API and
//! Gemini's `generateContent` are certain, and
//! `docs/decisions/2026-08-21-a-profile-is-not-a-protocol.md` records what the
//! seam separates and where the credential goes.
//!
//! What the extraction now waits on is its own inputs: regression case moves
//! `GenRequest` and `AiEvent`, which are the types the signature is made of. A
//! trait written today would be rewritten before it had a second implementer.
//! The ADR's §6 says why that is waiting rather than deferring.

pub mod openai_completions;
pub mod provider_cost;
pub mod sse;

use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ErrorKind, StopReason};

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
