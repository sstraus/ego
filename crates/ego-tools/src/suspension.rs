//! Wall time this process spent stopped by job control (`Ctrl+Z`).
//!
//! A stopped process runs no timer, so a timeout measured on the monotonic
//! clock fires the moment the process is continued: the command finished, and
//! the report says it timed out. A deadline that must not charge that time
//! measures [`ActiveClock::elapsed`] instead of `Instant::elapsed`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Ledger {
    /// Suspensions that have ended.
    finished: Duration,
    /// The suspension in progress, from the moment the stop was requested.
    began: Option<Instant>,
}

static LEDGER: Mutex<Ledger> = Mutex::new(Ledger {
    finished: Duration::ZERO,
    began: None,
});

fn ledger() -> std::sync::MutexGuard<'static, Ledger> {
    LEDGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Total suspended time so far, counting a suspension still in progress up to
/// now. Counting it matters at the instant of `SIGCONT`: another thread can
/// look at the clock before the thread that stopped has recorded the end, and
/// it must read too little elapsed time rather than too much.
fn suspended() -> Duration {
    let ledger = ledger();
    ledger.finished + ledger.began.map_or(Duration::ZERO, |began| began.elapsed())
}

/// Mark the stop about to be requested. Returns the guard that ends it.
pub fn begin() -> Suspension {
    ledger().began = Some(Instant::now());
    Suspension(())
}

/// Ends the suspension begun by [`begin`] when it is dropped, so every exit
/// from the stopped region closes the ledger.
#[derive(Debug)]
pub struct Suspension(());

impl Drop for Suspension {
    fn drop(&mut self) {
        let mut ledger = ledger();
        if let Some(began) = ledger.began.take() {
            ledger.finished += began.elapsed();
        }
    }
}

/// A stopwatch that does not run while the process is suspended.
#[derive(Debug, Clone, Copy)]
pub struct ActiveClock {
    started: Instant,
    suspended_at_start: Duration,
}

impl ActiveClock {
    pub fn start() -> Self {
        Self {
            started: Instant::now(),
            suspended_at_start: suspended(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        let stopped = suspended().saturating_sub(self.suspended_at_start);
        self.started.elapsed().saturating_sub(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ledger is process-global: `cargo test` runs these on threads of one
    /// process, so a suspension in one case would be charged to the other.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// A command that finished while the process was stopped must not read as
    /// having run for the whole stop: that was the report `timed out after 20
    /// seconds` for a command that printed its result.
    #[test]
    fn a_suspension_is_not_charged_to_a_clock_that_started_before_it() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let clock = ActiveClock::start();
        let suspension = begin();
        std::thread::sleep(Duration::from_millis(120));
        // Mid-stop, as another thread sees it the moment `SIGCONT` arrives.
        assert!(clock.elapsed() < Duration::from_millis(60));
        drop(suspension);
        assert!(clock.elapsed() < Duration::from_millis(60));
    }

    #[test]
    fn time_outside_a_suspension_still_counts() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let clock = ActiveClock::start();
        std::thread::sleep(Duration::from_millis(80));
        assert!(clock.elapsed() >= Duration::from_millis(80));
    }
}
