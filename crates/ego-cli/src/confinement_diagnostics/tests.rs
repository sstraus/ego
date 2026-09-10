use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::thread;

use super::*;

#[derive(Default)]
struct RecordingDiagnostics {
    notes: Mutex<Vec<String>>,
}

impl Diagnostics for RecordingDiagnostics {
    fn note(&self, message: &str) {
        self.notes
            .lock()
            .expect("diagnostic notes mutex")
            .push(message.to_owned());
    }
}

fn notes(diagnostics: &RecordingDiagnostics) -> Vec<String> {
    diagnostics
        .notes
        .lock()
        .expect("diagnostic notes mutex")
        .clone()
}

#[test]
fn empty_capabilities_warn_once_but_capable_measurements_do_not_consume_warning() {
    let diagnostics = Arc::new(RecordingDiagnostics::default());
    let reporter = Reporter::new(Arc::clone(&diagnostics) as Arc<dyn Diagnostics>);

    reporter.report(Ok(ExecCaps::READ_SCOPED));
    reporter.report(Ok(ExecCaps::empty()));
    reporter.report(Ok(ExecCaps::empty()));

    let notes = notes(&diagnostics);
    assert_eq!(
        notes.len(),
        1,
        "empty boot capabilities must be reported once"
    );
    assert!(notes[0].contains("no effective confinement"), "{notes:?}");
}

#[test]
fn diagnostic_errors_are_reported_without_consuming_the_empty_warning() {
    let diagnostics = Arc::new(RecordingDiagnostics::default());
    let reporter = Reporter::new(Arc::clone(&diagnostics) as Arc<dyn Diagnostics>);

    reporter.report(Err(ProcessError::Backend("probe failed".to_owned())));
    reporter.report(Ok(ExecCaps::empty()));

    let notes = notes(&diagnostics);
    assert_eq!(
        notes.len(),
        2,
        "errors must not mark the empty warning as emitted"
    );
    assert!(notes[0].contains("probe failed"), "{notes:?}");
    assert!(notes[1].contains("no effective confinement"), "{notes:?}");
}

#[test]
fn poisoned_warned_lock_is_recovered_and_panic_does_not_mark_warning() {
    struct PanicOnce {
        calls: Mutex<usize>,
    }

    impl Diagnostics for PanicOnce {
        fn note(&self, _message: &str) {
            let mut calls = self
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *calls += 1;
            if *calls == 1 {
                panic!("diagnostic sink failed");
            }
        }
    }

    let diagnostics = Arc::new(PanicOnce {
        calls: Mutex::new(0),
    });
    let reporter = Reporter::new(Arc::clone(&diagnostics) as Arc<dyn Diagnostics>);
    let first = catch_unwind(AssertUnwindSafe(|| reporter.report(Ok(ExecCaps::empty()))));
    assert!(
        first.is_err(),
        "the test sink must poison the reporter lock"
    );

    reporter.report(Ok(ExecCaps::empty()));
    assert_eq!(
        *diagnostics
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        2,
        "a panic before note returns must not suppress a later warning"
    );
}

#[test]
fn concurrent_empty_reports_are_serialized_by_the_shared_warned_lock() {
    struct Gate {
        state: Mutex<(usize, bool)>,
        entered: Condvar,
        release: Condvar,
    }

    impl Diagnostics for Gate {
        fn note(&self, _message: &str) {
            let mut state = self.state.lock().expect("gate mutex");
            state.0 += 1;
            if state.0 == 1 {
                self.entered.notify_all();
                while !state.1 {
                    state = self.release.wait(state).expect("gate release");
                }
            }
        }
    }

    let diagnostics = Arc::new(Gate {
        state: Mutex::new((0, false)),
        entered: Condvar::new(),
        release: Condvar::new(),
    });
    let reporter = Arc::new(Reporter::new(
        Arc::clone(&diagnostics) as Arc<dyn Diagnostics>
    ));
    let first = {
        let reporter = Arc::clone(&reporter);
        thread::spawn(move || reporter.report(Ok(ExecCaps::empty())))
    };

    let mut state = diagnostics.state.lock().expect("gate mutex");
    while state.0 != 1 {
        state = diagnostics.entered.wait(state).expect("gate entry");
    }
    drop(state);

    let second_started = Arc::new(Barrier::new(2));
    let second = {
        let reporter = Arc::clone(&reporter);
        let second_started = Arc::clone(&second_started);
        thread::spawn(move || {
            second_started.wait();
            reporter.report(Ok(ExecCaps::empty()));
        })
    };
    second_started.wait();

    let mut state = diagnostics.state.lock().expect("gate mutex");
    state.1 = true;
    diagnostics.release.notify_all();
    drop(state);
    first.join().expect("first reporter thread");
    second.join().expect("second reporter thread");
    assert_eq!(diagnostics.state.lock().expect("gate mutex").0, 1);
}
