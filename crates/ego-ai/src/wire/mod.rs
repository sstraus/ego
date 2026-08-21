//! Wire formats: one module per API shape (`ARCHITECTURE.md` §3.1).
//!
//! A wire format turns a provider-neutral `GenRequest` into bytes and turns
//! the bytes back into [`AiEvent`](ego_core::generator::port::AiEvent)s. It
//! owns no socket, no credential and no policy — those belong to
//! [`crate::http`] and to the provider table — so a wire format is testable
//! against a transcript, which is what the goldens here do.
//!
//! There is deliberately no `WireFormat` trait yet. `ARCHITECTURE.md` sketches
//! one taking a `ProviderEntry`, and it will exist; extracting it from a
//! single implementation would mean inventing the second implementation's
//! requirements from imagination.

pub mod openai_completions;
pub mod provider_cost;
pub mod sse;

use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::StopReason;

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
pub fn failed(message: String, delivery: DeliveryCertainty) -> [AiEvent; 3] {
    [
        AiEvent::DeliveryObserved(delivery),
        AiEvent::Error { message },
        AiEvent::Stop(StopReason::Error),
    ]
}
