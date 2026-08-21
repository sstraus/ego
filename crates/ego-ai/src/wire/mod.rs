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
pub mod sse;
