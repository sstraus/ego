//! Critic cases for the suspension ledger. The ledger is process-global, so the
//! cases take a lock and run one at a time under `cargo test` as well.

use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use ego_tools::suspension::{ActiveClock, begin};

static SERIAL: Mutex<()> = Mutex::new(());

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Catches: subtracting the whole ledger instead of the part recorded after the
/// clock started, so a clock started after an old stop reads too little and an
/// exec timeout fires late by every earlier stop.
#[test]
fn a_clock_started_after_a_finished_suspension_is_not_reduced_by_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stop = begin();
    sleep(ms(150));
    drop(stop);
    let clock = ActiveClock::start();
    sleep(ms(80));
    assert!(clock.elapsed() >= ms(80), "elapsed {:?}", clock.elapsed());
}

/// Catches: a second stop overwriting or not adding to the first, so one of two
/// consecutive Ctrl+Z stops is charged to the exec timeout.
#[test]
fn two_consecutive_suspensions_are_both_discounted() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let clock = ActiveClock::start();
    for _ in 0..2 {
        let stop = begin();
        sleep(ms(120));
        drop(stop);
    }
    assert!(clock.elapsed() < ms(100), "elapsed {:?}", clock.elapsed());
}

/// Catches: a clock started while the process was already stopped (another
/// thread racing the stop) charging the part of the stop that preceded it, or
/// losing the active time that follows.
#[test]
fn a_clock_started_inside_a_suspension_counts_only_the_time_after_the_stop() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let stop = begin();
    sleep(ms(60));
    let clock = ActiveClock::start();
    sleep(ms(60));
    drop(stop);
    sleep(ms(80));
    let elapsed = clock.elapsed();
    assert!(elapsed >= ms(80), "lost active time: {elapsed:?}");
    assert!(elapsed < ms(140), "charged the stop: {elapsed:?}");
}

/// Catches: one shared offset for every clock, so a clock started after a stop
/// is discounted by a stop it never saw, or a clock started before it is not.
#[test]
fn clocks_either_side_of_a_suspension_each_see_only_their_own_share() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let before = ActiveClock::start();
    let stop = begin();
    sleep(ms(120));
    drop(stop);
    let after = ActiveClock::start();
    sleep(ms(70));
    assert!(before.elapsed() < ms(130), "before {:?}", before.elapsed());
    assert!(before.elapsed() >= ms(70), "before {:?}", before.elapsed());
    assert!(after.elapsed() >= ms(70), "after {:?}", after.elapsed());
}

/// Catches: elapsed time going backwards across the end of a stop (a reader
/// that compares it with a timeout could see a deadline un-expire, or a clock
/// that read "too much" mid-stop shrink after the ledger closes).
#[test]
fn elapsed_never_decreases_across_the_end_of_a_suspension() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let clock = ActiveClock::start();
    sleep(ms(20));
    let stop = begin();
    let mid = clock.elapsed();
    sleep(ms(60));
    let late_mid = clock.elapsed();
    drop(stop);
    let after = clock.elapsed();
    assert!(late_mid >= mid.saturating_sub(ms(2)), "{mid:?} -> {late_mid:?}");
    assert!(after >= late_mid.saturating_sub(ms(2)), "{late_mid:?} -> {after:?}");
}
