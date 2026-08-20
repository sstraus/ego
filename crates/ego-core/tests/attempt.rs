//! Generation attempts: the identity a retry keeps, the identity it does not,
//! and the delivery certainty that decides whether a retry is safe at all.
//!
//! See `docs/decisions/2026-08-20-generation-attempt-contract.md` and
//! `plans/m0b-vertical-slice.md` Steps 1-2.
//!
//! What these tests defend is one claim: **a generation and a wire attempt are
//! not the same thing, and confusing them double-bills**. Prior history is
//! re-sent on every retry, so a provider that already charged for attempt 1
//! charges again for attempt 2 — the only defence is knowing which is which
//! and refusing to retry when the first attempt may have landed.

use ego_core::event::id::{AttemptId, GenerationId};
use ego_core::generator::attempt::{AttemptRef, DeliveryCertainty};

// Wrapped in a module named `attempt` so `cargo test -p ego-core attempt`
// matches by substring: cargo filters on the full test path, not the file
// name — the same trick `tests/reducer.rs` and `tests/cost.rs` use.
mod attempt {
    use super::*;

    // --- identity ---------------------------------------------------------

    #[test]
    fn generation_and_attempt_ids_sort_by_creation() {
        let (a, b) = (GenerationId::new(), GenerationId::new());
        assert!(a < b, "UUIDv7 generations must order by creation");
        let (a, b) = (AttemptId::new(), AttemptId::new());
        assert!(a < b, "UUIDv7 attempts must order by creation");
    }

    #[test]
    fn ids_round_trip_through_serde() {
        let generation = GenerationId::new();
        let json = serde_json::to_string(&generation).unwrap();
        assert_eq!(
            serde_json::from_str::<GenerationId>(&json).unwrap(),
            generation
        );

        let attempt = AttemptId::new();
        let json = serde_json::to_string(&attempt).unwrap();
        assert_eq!(serde_json::from_str::<AttemptId>(&json).unwrap(), attempt);
    }

    /// The whole point of two identifiers. A retry is the *same* logical
    /// generation — the budget settles once for it — reaching the wire a
    /// second time, and usage figures key to the attempt so the two are not
    /// summed.
    #[test]
    fn a_retry_keeps_the_generation_and_mints_a_new_attempt() {
        let first = AttemptRef::first(GenerationId::new());
        let second = first.retry();
        let third = second.retry();

        assert_eq!(second.generation(), first.generation());
        assert_eq!(third.generation(), first.generation());

        assert_ne!(second.attempt(), first.attempt());
        assert_ne!(third.attempt(), second.attempt());
        assert_ne!(third.attempt(), first.attempt());
    }

    #[test]
    fn two_generations_never_share_an_identity() {
        let a = AttemptRef::first(GenerationId::new());
        let b = AttemptRef::first(GenerationId::new());
        assert_ne!(a.generation(), b.generation());
        assert_ne!(a.attempt(), b.attempt());
    }

    // --- delivery certainty ------------------------------------------------

    /// A fresh attempt has not touched the wire, so it is safe to retry: that
    /// is the only state in which a retry cannot double-bill.
    #[test]
    fn a_fresh_attempt_is_definitely_unsent_and_safe_to_retry() {
        let certainty = DeliveryCertainty::default();
        assert_eq!(certainty, DeliveryCertainty::DefinitelyUnsent);
        assert!(certainty.retry_is_safe());
    }

    #[test]
    fn once_the_request_may_have_landed_a_retry_is_not_safe() {
        let mut certainty = DeliveryCertainty::default();
        certainty.observe(DeliveryCertainty::PossiblySent);
        assert!(!certainty.retry_is_safe());
    }

    /// The invariant. Certainty is monotonic — it only ever gets worse — and
    /// this is the test that matters: a downgrade back to `DefinitelyUnsent`
    /// would licence exactly the retry that bills twice. A connection reset
    /// *after* the bytes went out looks locally like "nothing happened", and
    /// that is precisely when the lie is most tempting.
    #[test]
    fn certainty_never_improves_back_to_definitely_unsent() {
        let mut certainty = DeliveryCertainty::default();
        certainty.observe(DeliveryCertainty::PossiblySent);

        for _ in 0..3 {
            certainty.observe(DeliveryCertainty::DefinitelyUnsent);
            assert_eq!(
                certainty,
                DeliveryCertainty::PossiblySent,
                "a later observation must not restore certainty we lost"
            );
        }
    }

    #[test]
    fn observing_the_same_certainty_twice_changes_nothing() {
        let mut certainty = DeliveryCertainty::default();
        certainty.observe(DeliveryCertainty::DefinitelyUnsent);
        assert_eq!(certainty, DeliveryCertainty::DefinitelyUnsent);

        certainty.observe(DeliveryCertainty::PossiblySent);
        certainty.observe(DeliveryCertainty::PossiblySent);
        assert_eq!(certainty, DeliveryCertainty::PossiblySent);
    }

    /// It travels in a durable event (`AttemptSettled`), so it has to survive
    /// the round trip with its meaning intact.
    #[test]
    fn certainty_round_trips_through_serde() {
        for certainty in [
            DeliveryCertainty::DefinitelyUnsent,
            DeliveryCertainty::PossiblySent,
        ] {
            let json = serde_json::to_string(&certainty).unwrap();
            assert_eq!(
                serde_json::from_str::<DeliveryCertainty>(&json).unwrap(),
                certainty
            );
        }
    }
}
