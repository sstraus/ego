//! The system clock: the only thing in the workspace that reads wall time.
//!
//! `ego-core` declares the [`Clock`] port and never implements it, for the
//! same reason it never opens a file — see
//! `ARCHITECTURE.md` §2.

use std::time::{SystemTime, UNIX_EPOCH};

use ego_core::event::time::{Clock, Timestamp};

/// UTC milliseconds from the operating system.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    /// The current time, or the epoch if the system clock is set before it.
    ///
    /// Saturating rather than panicking or erroring. A clock behind 1970 is a
    /// misconfigured machine, not a corrupt session, and refusing to run on
    /// one would turn a wrong wall-clock reading into an unrunnable harness —
    /// while `Seq`, not `Timestamp`, is what actually orders the log
    /// (`event/time.rs`). The reading is wrong either way; only one of the two
    /// answers still lets the session be replayed.
    fn now(&self) -> Timestamp {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or(0);
        Timestamp::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
    }
}
