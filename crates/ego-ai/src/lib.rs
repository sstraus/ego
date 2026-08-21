//! Provider layer: wire formats, provider table, usage, delivery certainty,
//! model catalogues.
//!
//! See `ARCHITECTURE.md` §3.1. Implements `ego-core`'s `Generator` port and
//! owns `Http`, credentials and the provider tables.

pub mod http;
pub mod wire;

use ego_core::generator::attempt::{AttemptStream, GenerationAttempt};
use ego_core::generator::port::Generator;
use ego_core::generator::types::GenRequest;
use futures::stream;

/// Proves the crate DAG direction (`ego-ai -> ego-core`) compiles: a bare
/// `Generator` impl with no wire format behind it. Real providers —
/// `WireFormat`, `Http`, the provider table (ARCH §3.1) — land in a later
/// step; this story's scope is only the port and the fact that `ego-ai` can
/// implement it.
pub struct SkeletonGenerator;

impl Generator for SkeletonGenerator {
    fn generate(&self, _req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
        AttemptStream::new(attempt, Box::pin(stream::empty()))
    }
}
