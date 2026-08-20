//! `FakeGenerator`: a scriptable `Generator` for tests and for the
//! eval/replay seam (U3, `docs/10-triage.md`) — record a real session's
//! `AiEvent` stream once, then replay it deterministically without a
//! network call.

use std::collections::VecDeque;
use std::sync::Mutex;

use futures::stream;

use super::attempt::{AttemptStream, GenerationAttempt};
use super::port::{AiEvent, Generator};
use super::types::GenRequest;

/// A `Generator` driven entirely by pre-scripted output.
///
/// Holds a queue of scripts, one `Vec<AiEvent>` per expected call to
/// `generate`: the first call plays the first script, the second call the
/// second, and so on, so a multi-turn conversation can be replayed call by
/// call. Every `GenRequest` it receives is recorded so a test or eval
/// harness can assert on what was actually sent, not just what came back.
///
/// Interior mutability via `std::sync::Mutex`: `generate` only ever holds
/// the lock across a synchronous pop/push, never across an `.await`, so a
/// blocking mutex is the simplest correct choice here (no need for
/// `tokio::sync::Mutex`).
pub struct FakeGenerator {
    scripts: Mutex<VecDeque<Vec<AiEvent>>>,
    calls: Mutex<Vec<GenRequest>>,
}

impl FakeGenerator {
    /// Build a generator that plays `scripts` in order, one per call to
    /// `generate`. A call made after the scripts run out gets an empty
    /// stream rather than panicking — a test can still assert on the
    /// recorded `GenRequest` for that call.
    pub fn new(scripts: impl IntoIterator<Item = Vec<AiEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Every `GenRequest` passed to `generate`, in call order.
    pub fn calls(&self) -> Vec<GenRequest> {
        self.calls
            .lock()
            .expect("FakeGenerator::calls poisoned")
            .clone()
    }
}

impl Generator for FakeGenerator {
    /// Records the request and plays the next script.
    ///
    /// Passes the attempt straight through into the returned stream. A
    /// fake generator spends no money, so it has nothing to charge — but the
    /// attempt still has to be *obtained* to reach this call, and it still
    /// comes back out for the caller to settle, which is the whole point:
    /// the gate is enforced by the type, so even the replay seam cannot
    /// route around it.
    fn generate(&self, req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
        self.calls
            .lock()
            .expect("FakeGenerator::calls poisoned")
            .push(req);
        let script = self
            .scripts
            .lock()
            .expect("FakeGenerator::scripts poisoned")
            .pop_front()
            .unwrap_or_default();
        AttemptStream::new(attempt, Box::pin(stream::iter(script)))
    }
}
