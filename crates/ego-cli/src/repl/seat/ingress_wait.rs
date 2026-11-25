//! Keep the terminal's bounded printer moving while core acknowledges an intent.

use super::{Reedline, SharedStatus, report_terminal_failure, write_printer_message};
use crossbeam::channel::{Receiver, Sender, bounded};
use ego_core::turn::ControlError;
use futures::task::{ArcWake, waker};
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll};

struct Wake(Sender<()>);

impl ArcWake for Wake {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        let _ = arc_self.0.try_send(());
    }
}

pub(super) fn wait<T>(
    editor: &mut Reedline,
    status: &SharedStatus,
    future: impl Future<Output = Result<T, ControlError>>,
) -> Result<T, ControlError> {
    let output = status
        .lock()
        .expect("surface status lock")
        .printer_output
        .clone();
    let Some(output) = output else {
        return futures::executor::block_on(future);
    };
    let mut wrote = false;
    let result = with_output(future, &output, |row| {
        if !wrote {
            // A late cursor report must not abandon an already-sent intent.
            let _ = editor.erase_last_prompt();
            wrote = true;
        }
        #[cfg(test)]
        if let Some(output) = &status
            .lock()
            .expect("surface status lock")
            .printer_test_output
        {
            let mut output = output.lock().expect("test printer lock");
            if output.fail_writes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected writer failure",
                ));
            }
            write_printer_message(&mut output.bytes, row)?;
            if let Some(delivered) = &output.delivered {
                let _ = delivered.send(());
            }
            return Ok(());
        }
        write_printer_message(&mut std::io::stdout().lock(), row)
    });
    if wrote {
        editor.invalidate_prompt_position();
    }
    match result {
        Ok(result) => result,
        Err(error) => {
            let current = status.lock().expect("surface status lock");
            #[cfg(test)]
            let captured = if let Some(output) = &current.printer_test_output {
                output
                    .lock()
                    .expect("test printer lock")
                    .errors
                    .push(error.to_string());
                true
            } else {
                false
            };
            #[cfg(not(test))]
            let captured = false;
            if !captured {
                report_terminal_failure("output", error);
            }
            if let Some(cancel) = &current.printer_cancel {
                cancel.cancel(ego_core::event::domain::CancelCause::Host);
            }
            // Closed means admission ended and permits submit callers to
            // hand the same text to the next turn. This intent may already
            // be queued, so a terminal failure must never take that fallback.
            Err(ControlError::Unavailable(
                "terminal output failed".to_owned(),
            ))
        }
    }
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct TestOutput {
    pub(super) bytes: Vec<u8>,
    pub(super) fail_writes: bool,
    pub(super) errors: Vec<String>,
    pub(super) delivered: Option<Sender<()>>,
}

fn with_output<F: Future>(
    future: F,
    output: &Receiver<String>,
    mut render: impl FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<F::Output> {
    let mut future = std::pin::pin!(future);
    let (wake, ready) = bounded(1);
    let waker = waker(Arc::new(Wake(wake)));
    let mut context = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
            return Ok(result);
        }
        crossbeam::channel::select! {
            recv(ready) -> _ => {},
            recv(output) -> row => {
                let row = row.map_err(|_| std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe, "terminal printer closed"
                ))?;
                render(&row)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches: request_interrupt bypasses the printer-aware wait and starves finish.
    #[test]
    fn interrupt_does_not_starve_finish_printer() {
        use super::super::{SurfaceOutput, SurfaceStatus, request_interrupt};
        use ego_core::turn::{CancelToken, interactive_control};
        use std::sync::{Mutex, atomic::AtomicBool};
        use std::time::Duration;

        let (printer, rows) = bounded(1);
        printer.send("first row".to_owned()).unwrap();
        // Reedline retains its printer sender for the lifetime of the owner.
        let _editor_printer = printer.clone();
        let cancel = CancelToken::new();
        let (mut output, pump) =
            SurfaceOutput::pumped(printer, cancel.clone(), Arc::new(AtomicBool::new(false)));
        let (ingress, control) = interactive_control();
        ingress.begin_turn();
        let (delivered, delivery) = bounded(2);
        let written = Arc::new(Mutex::new(TestOutput {
            delivered: Some(delivered),
            ..TestOutput::default()
        }));
        let status = Arc::new(Mutex::new(SurfaceStatus {
            printer_output: Some(rows.clone()),
            printer_cancel: Some(cancel),
            printer_test_output: Some(Arc::clone(&written)),
            ..SurfaceStatus::default()
        }));
        let (done, completed) = bounded(2);
        let owner_done = done.clone();
        let owner = std::thread::spawn(move || {
            let accepted = request_interrupt(&mut Reedline::create(), &status, &ingress);
            owner_done.send("interrupt").unwrap();
            accepted
        });
        let driver = std::thread::spawn(move || {
            output.write("final row\n").unwrap();
            let result = output.finish();
            drop(output);
            // Close after observing both rows at the terminal boundary, not
            // merely after the pump enqueued them. This makes byte assertions
            // independent of whether the control or printer wake wins a race.
            for _ in 0..2 {
                if delivery.recv_timeout(Duration::from_secs(2)).is_err() {
                    break;
                }
            }
            drop(control);
            done.send("finish").unwrap();
            result
        });
        let first = completed.recv_timeout(Duration::from_secs(2));
        if first.is_err() {
            // Release the original deadlock before joining, including on RED.
            while completed.len() < 2 {
                let _ = rows.recv_timeout(Duration::from_millis(100));
            }
        }
        let result = driver.join().unwrap();
        assert!(!owner.join().unwrap(), "the real control receiver closed");
        pump.join().unwrap();
        assert!(first.is_ok(), "interrupt starved the final output barrier");
        assert!(result.is_ok(), "final output was not delivered");
        let written = written.lock().unwrap();
        assert_eq!(written.bytes, b"first row\r\nfinal row\r\n");
        assert!(written.errors.is_empty());
    }

    // Catches: a writer failure drops an intent without ending its owning host.
    #[test]
    fn interrupt_writer_failure_reports_and_cancels_host_without_waiting_for_reply() {
        use super::super::{SurfaceStatus, request_interrupt};
        use ego_core::event::domain::CancelCause;
        use ego_core::turn::{CancelToken, interactive_control};
        use std::sync::Mutex;
        use std::time::Duration;

        let (printer, rows) = bounded(1);
        printer.send("queued output".to_owned()).unwrap();
        let cancel = CancelToken::new();
        let written = Arc::new(Mutex::new(TestOutput {
            fail_writes: true,
            ..TestOutput::default()
        }));
        let status = Arc::new(Mutex::new(SurfaceStatus {
            printer_output: Some(rows),
            printer_cancel: Some(cancel.clone()),
            printer_test_output: Some(Arc::clone(&written)),
            ..SurfaceStatus::default()
        }));
        let (ingress, control) = interactive_control();
        ingress.begin_turn();
        let (done, completed) = bounded(1);
        let owner = std::thread::spawn(move || {
            done.send(request_interrupt(
                &mut Reedline::create(),
                &status,
                &ingress,
            ))
            .unwrap();
        });
        let result = completed.recv_timeout(Duration::from_secs(2));
        // A regressed wait can still exit cleanly before the assertion fails.
        drop(control);
        owner.join().unwrap();
        assert_eq!(
            result,
            Ok(false),
            "terminal failure must not wait for an acknowledgement"
        );
        assert_eq!(cancel.cause(), Some(CancelCause::Host));
        let written = written.lock().unwrap();
        assert!(written.bytes.is_empty());
        assert_eq!(written.errors, ["injected writer failure"]);
    }

    // Catches: the printer-aware wait never wakes when core replies without printing a row.
    #[test]
    fn acknowledgement_without_output_wakes_the_terminal_owner() {
        use std::time::Duration;
        let (_printer, output) = bounded(1);
        let (reply, answer) = futures::channel::oneshot::channel();
        let (polled, first_poll) = bounded(1);
        let coordinator = std::thread::spawn(move || {
            first_poll.recv_timeout(Duration::from_secs(2)).unwrap();
            reply.send("acknowledged").unwrap();
        });
        let mut answer = std::pin::pin!(answer);
        let waiting = futures::future::poll_fn(|context| {
            let result = answer.as_mut().poll(context);
            let _ = polled.try_send(());
            result
        });
        let result = with_output(waiting, &output, |_| panic!("no output expected")).unwrap();
        coordinator.join().unwrap();
        assert_eq!(result, Ok("acknowledged"));
    }

    // Catches: waiting for core acknowledgement starves the printer needed by core's finish barrier.
    #[test]
    fn control_acknowledgement_drains_final_output_before_reply() {
        use crate::repl::seat::SurfaceOutput;
        use ego_core::turn::CancelToken;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let (printer, output) = bounded(1);
        printer.send("held".to_owned()).unwrap();
        let (mut surface, pump) = SurfaceOutput::pumped(
            printer,
            CancelToken::new(),
            Arc::new(AtomicBool::new(false)),
        );
        surface.write("final output\n").unwrap();
        let (reply, answer) = futures::channel::oneshot::channel();
        let coordinator = std::thread::spawn(move || {
            surface.finish().unwrap();
            let _ = reply.send("acknowledged");
        });
        let (deadline, expired) = futures::channel::oneshot::channel();
        let (stop, stopped) = bounded(1);
        let watchdog = std::thread::spawn(move || {
            if stopped.recv_timeout(Duration::from_secs(2)).is_err() {
                let _ = deadline.send(());
            }
        });
        let mut rows = Vec::new();
        let result = with_output(futures::future::select(answer, expired), &output, |row| {
            rows.push(row.to_owned());
            Ok(())
        })
        .unwrap();
        let acknowledged = matches!(
            result,
            futures::future::Either::Left((Ok("acknowledged"), _))
        );
        // Release the producer even if the regression fails; never leave blocked test threads.
        drop(output);
        let _ = stop.send(());
        watchdog.join().unwrap();
        coordinator.join().unwrap();
        pump.join().unwrap();
        assert!(
            acknowledged,
            "control acknowledgement waited for its own printer consumer"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| !row.is_empty())
                .cloned()
                .collect::<Vec<_>>(),
            ["held", "final output\n"]
        );
    }
}
