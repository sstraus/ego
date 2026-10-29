//! `SystemClock`: the only wall-clock reader in the workspace.
//!
//! See `docs/architecture.md` §2.

use ego_core::event::time::{Clock, Timestamp};
use ego_tools::clock::SystemClock;

mod clock {
    use super::*;

    #[test]
    fn clock_reads_a_time_after_the_epoch_and_before_the_far_future() {
        let now = SystemClock.now();
        // 2020-01-01 and 2100-01-01, in UTC milliseconds. A range and not an
        // exact value, because the thing under test is precisely the one
        // number a test cannot pin — what it can pin is that the conversion
        // produces milliseconds rather than seconds or nanoseconds, which a
        // wrong unit would fail by three orders of magnitude.
        assert!(
            now > Timestamp::from_millis(1_577_836_800_000),
            "the clock read a time before 2020: {now:?}"
        );
        assert!(
            now < Timestamp::from_millis(4_102_444_800_000),
            "the clock read a time after 2100: {now:?}"
        );
    }

    #[test]
    fn clock_does_not_go_backwards_between_two_reads() {
        // Not "goes forwards": two reads inside one millisecond are equal,
        // and that is fine — `Seq`, not `Timestamp`, orders the log.
        let first = SystemClock.now();
        let second = SystemClock.now();
        assert!(second >= first, "{first:?} then {second:?}");
    }
}
