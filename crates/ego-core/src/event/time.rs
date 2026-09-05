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

    /// This instant as UTC calendar fields.
    ///
    /// The durable spelling of a timestamp stays integer milliseconds — see
    /// the type's doc comment for why that is a codec obligation. This is the
    /// other direction, for the surfaces that are not the log: a terminal
    /// listing and a wire format both need a date, and neither should own its
    /// own calendar arithmetic. Pure and total, so it belongs here rather
    /// than in an adapter.
    pub fn civil(self) -> Civil {
        let millis = self.0 as i64;
        let (days, rest) = (millis.div_euclid(86_400_000), millis.rem_euclid(86_400_000));
        let (year, month, day) = civil_from_days(days);
        Civil {
            year,
            month,
            day,
            hour: (rest / 3_600_000) as u32,
            minute: ((rest % 3_600_000) / 60_000) as u32,
            second: ((rest % 60_000) / 1_000) as u32,
            milli: (rest % 1_000) as u32,
        }
    }

    /// This instant as RFC 3339, which is the ISO 8601 profile every JSON
    /// wire ego speaks asks for by name.
    ///
    /// Always UTC and always `Z`: the domain holds no offset, so any local
    /// rendering here would be an invention. Milliseconds are always printed,
    /// including `.000`, because a fixed-width spelling sorts lexicographically
    /// in the same order it sorts chronologically and a variable one does not.
    pub fn to_rfc3339(self) -> String {
        let c = self.civil();
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            c.year, c.month, c.day, c.hour, c.minute, c.second, c.milli
        )
    }
}

/// A [`Timestamp`] decomposed into UTC calendar fields.
///
/// A plain struct of parts, not a second time type: it has no ordering, no
/// arithmetic and no way back. `Timestamp` remains the only thing an event
/// carries, and this is only how a renderer reads one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    /// Proleptic Gregorian year. Signed, because the arithmetic is defined
    /// before 1970 and refusing to say so would be a lie about a clock that
    /// went backwards.
    pub year: i64,
    /// 1-12.
    pub month: u32,
    /// 1-31.
    pub day: u32,
    /// 0-23.
    pub hour: u32,
    /// 0-59.
    pub minute: u32,
    /// 0-59. Never 60: the domain counts Unix milliseconds, which have no
    /// leap second to report.
    pub second: u32,
    /// 0-999.
    pub milli: u32,
}

/// Howard Hinnant's `civil_from_days`: a day count since the Unix epoch to a
/// proleptic Gregorian date. Exact for every day this program can be handed.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
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
    fn rfc3339_is_utc_fixed_width_and_millisecond_precise() {
        assert_eq!(
            Timestamp::from_millis(1_787_206_085_352).to_rfc3339(),
            "2026-08-20T06:08:05.352Z"
        );
        assert_eq!(Timestamp::EPOCH.to_rfc3339(), "1970-01-01T00:00:00.000Z");
        // A sub-second value still prints three digits, so the spelling sorts
        // lexicographically in the order it sorts chronologically.
        assert_eq!(
            Timestamp::from_millis(7).to_rfc3339(),
            "1970-01-01T00:00:00.007Z"
        );
    }

    #[test]
    fn the_calendar_handles_leap_days_and_century_rules() {
        // 2024 is a leap year, 2100 will not be, and 2000 was: the three cases
        // a naive `year % 4` gets wrong.
        let civil = |iso: &str, millis: u64| {
            assert_eq!(Timestamp::from_millis(millis).to_rfc3339(), iso);
        };
        civil("2024-02-29T00:00:00.000Z", 1_709_164_800_000);
        civil("2000-02-29T00:00:00.000Z", 951_782_400_000);
        civil("2100-03-01T00:00:00.000Z", 4_107_542_400_000);
    }

    #[test]
    fn civil_reports_every_field_of_one_instant() {
        let c = Timestamp::from_millis(1_787_206_085_352).civil();
        assert_eq!(
            (c.year, c.month, c.day, c.hour, c.minute, c.second, c.milli),
            (2026, 8, 20, 6, 8, 5, 352)
        );
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
