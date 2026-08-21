//! Wall-clock observation time for the event envelope, and the port that
//! reads it.

use serde::{Deserialize, Serialize};

/// A wall-clock observation time: UTC milliseconds since the Unix epoch.
///
/// Deliberately a domain-owned newtype rather than `chrono::DateTime<Utc>` or
/// `time::OffsetDateTime`. `StoreRecord` checksums xxhash64 over canonical
/// payload bytes, so the durable spelling of a timestamp is part of ego's
/// versioned codec contract; delegating it to a dependency's serde
/// configuration (offset form, fractional-second precision, normalisation)
/// would let the checksummed bytes drift with a dependency bump. Integer
/// milliseconds have exactly one canonical spelling and no timezone aliases.
///
/// This is the time an event was *observed*, and it is not derivable from the
/// UUIDv7 `EventId`: the `uuid` crate's v7 context deliberately retains the
/// previous timestamp when the system clock is equal or earlier, so that IDs
/// stay monotonic. The millis embedded in an ID can therefore diverge from
/// the producer's wall clock.
///
/// A `Timestamp` may go backwards between consecutive events (NTP correction,
/// clock skew across a subagent process). `Seq` — not `Timestamp` — is
/// authoritative for ordering; the reducer must never reject, clamp or
/// reorder on wall time, nor derive durations from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    /// The Unix epoch.
    pub const EPOCH: Timestamp = Timestamp(0);

    /// Build a timestamp from UTC milliseconds since the Unix epoch.
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// UTC milliseconds since the Unix epoch.
    pub const fn as_millis(self) -> u64 {
        self.0
    }
}

/// Where an [`Envelope`](super::domain::Envelope)'s observation time comes
/// from (`docs/decisions/2026-08-21-the-turn-loop.md` §2).
///
/// A port rather than a `Timestamp::now()`, for the reason this whole type
/// exists: a timestamp that reads the system clock from inside the domain
/// makes every envelope non-deterministic and leaves a test nothing to
/// inject. `ego-core` does not read the system clock, the same way it does
/// not open files — `ego-tools` owns the implementation that does.
///
/// Returns a wall-clock `Timestamp`, not a `std::time::Instant`.
/// `ARCHITECTURE.md` §3.3 sketched `Env::clock() -> Instant`, which cannot
/// produce this value at all: an `Instant` is an opaque monotonic
/// measurement with no epoch, and this is UTC milliseconds since one.
/// `Instant` stays right for what it is good at — elapsed time and deadlines
/// — and that is a different method for a different caller.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millis_roundtrip() {
        let ts = Timestamp::from_millis(1_787_206_085_352);
        assert_eq!(ts.as_millis(), 1_787_206_085_352);
    }

    #[test]
    fn serialises_as_a_bare_integer() {
        let ts = Timestamp::from_millis(1_787_206_085_352);
        assert_eq!(serde_json::to_string(&ts).unwrap(), "1787206085352");
    }

    #[test]
    fn serde_roundtrip() {
        let ts = Timestamp::from_millis(1_787_206_085_352);
        let back: Timestamp = serde_json::from_str(&serde_json::to_string(&ts).unwrap()).unwrap();
        assert_eq!(ts, back);
    }

    #[test]
    fn epoch_is_zero_millis() {
        assert_eq!(Timestamp::EPOCH.as_millis(), 0);
    }

    #[test]
    fn orders_by_millis() {
        assert!(Timestamp::from_millis(1) < Timestamp::from_millis(2));
        assert!(Timestamp::EPOCH < Timestamp::from_millis(1));
    }

    #[test]
    fn a_later_event_may_carry_an_earlier_timestamp() {
        // Wall time is allowed to regress; Seq is what orders the log. This
        // pins the type's tolerance, not a desirable state of affairs.
        let earlier_observation = Timestamp::from_millis(1_787_206_085_352);
        let after_ntp_correction = Timestamp::from_millis(1_787_206_085_000);
        assert!(after_ntp_correction < earlier_observation);
    }
}
