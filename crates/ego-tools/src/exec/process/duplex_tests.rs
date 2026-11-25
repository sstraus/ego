//! Task wakes are observable only at the supervisor boundary; public transport
//! tests separately exercise settlement and process-tree ownership.

use super::*;
use std::io::{Read as _, Write as _};
use std::sync::atomic::AtomicUsize;

use futures::task::{ArcWake, waker};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{ChildStdin, Command};

const GRACE: Duration = Duration::from_millis(100);

// libtest runs unrelated child processes and global suspension tests on other
// threads. Keep the wake oracle and suspension ledger in an owned process.
fn run_isolated(name: &str) -> bool {
    const MARKER: &str = "EGO_DUPLEX_REGRESSION_CHILD";
    if std::env::var(MARKER).as_deref() == Ok(name) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            &format!("exec::process::duplex_tests::{name}"),
            "--nocapture",
        ])
        .env(MARKER, name)
        .output()
        .expect("isolated regression");
    assert!(
        output.status.success(),
        "isolated regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed;"),
        "isolated regression must execute exactly one test"
    );
    true
}

#[test]
#[ignore = "owned child fixture; launched by supervisor tests"]
fn idle_child_fixture() {
    println!("EGO_SUPERVISOR_READY");
    std::io::stdout().flush().expect("ready marker");
    std::io::stdin().read_exact(&mut [0]).expect("exit byte");
    std::process::exit(0);
}

async fn parked_child() -> (ManagedChild, ChildStdin) {
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args([
            "--ignored",
            "--exact",
            "exec::process::duplex_tests::idle_child_fixture",
            "--nocapture",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ManagedChild::spawn(command).expect("owned child");
    let stdin = child.take_stdin().expect("stdin");
    let mut stdout = BufReader::new(child.take_stdout().expect("stdout")).lines();
    loop {
        let line = stdout
            .next_line()
            .await
            .expect("ready read")
            .expect("ready line");
        if line.ends_with("EGO_SUPERVISOR_READY") {
            return (child, stdin);
        }
    }
}

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl ArcWake for WakeCount {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// Catches: a short periodic exit probe wakes an otherwise idle supervisor.
#[tokio::test]
async fn idle_duplex_supervisor_waits_for_an_event() {
    if run_isolated("idle_duplex_supervisor_waits_for_an_event") {
        return;
    }
    let (child, _stdin) = parked_child().await;
    let (_control, receiver) = mpsc::unbounded_channel();
    let cancel = CancelToken::new();
    let mut supervisor = Box::pin(supervise_duplex_child(
        child,
        receiver,
        cancel.clone(),
        None,
        ActiveClock::start(),
        GRACE,
    ));
    let wakes = Arc::new(WakeCount::default());
    let task_waker = waker(wakes.clone());
    assert!(
        supervisor
            .as_mut()
            .poll(&mut Context::from_waker(&task_waker))
            .is_pending()
    );
    wakes.0.store(0, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let idle_wakes = wakes.0.load(Ordering::SeqCst);
    // Always settle the real child, including when the regression is present.
    cancel.cancel(CancelCause::User);
    let (_, cleanup) = supervisor.await;
    assert!(matches!(cleanup, CleanupCertainty::Complete));
    assert_eq!(idle_wakes, 0, "an idle supervisor scheduled a wake");
}

// Catches: an already-ready child exit bypasses an expired deadline, or an
// expired deadline bypasses cancellation. These were the original priorities.
#[tokio::test]
async fn ready_duplex_exit_keeps_cancellation_and_deadline_priority() {
    if run_isolated("ready_duplex_exit_keeps_cancellation_and_deadline_priority") {
        return;
    }
    for cancelled in [false, true] {
        let (mut child, mut stdin) = parked_child().await;
        stdin.write_all(b"x").await.expect("exit byte");
        assert!(child.wait().await.expect("child exit").success());
        let (_control, receiver) = mpsc::unbounded_channel();
        let cancel = CancelToken::new();
        if cancelled {
            cancel.cancel(CancelCause::User);
        }
        let (end, cleanup) = supervise_duplex_child(
            child,
            receiver,
            cancel,
            Some(Duration::ZERO),
            ActiveClock::start(),
            GRACE,
        )
        .await;
        assert!(matches!(cleanup, CleanupCertainty::Complete));
        if cancelled {
            assert!(matches!(end, DuplexEnd::Cancelled));
        } else {
            assert!(matches!(end, DuplexEnd::TimedOut { timeout } if timeout == Duration::ZERO));
        }
    }
}

// Catches: using the wall-clock deadline after suspension terminates a child
// whose active-time budget has not expired.
#[tokio::test]
async fn duplex_deadline_does_not_charge_suspended_time() {
    if run_isolated("duplex_deadline_does_not_charge_suspended_time") {
        return;
    }
    let (child, mut stdin) = parked_child().await;
    let (_control, receiver) = mpsc::unbounded_channel();
    let suspension = crate::suspension::begin();
    let supervisor = tokio::spawn(supervise_duplex_child(
        child,
        receiver,
        CancelToken::new(),
        Some(Duration::from_millis(50)),
        ActiveClock::start(),
        GRACE,
    ));
    tokio::time::sleep(Duration::from_millis(120)).await;
    // The task remains pending despite its original wall timer expiring.
    let pending_while_suspended = !supervisor.is_finished();
    stdin.write_all(b"x").await.expect("exit byte");
    let (end, cleanup) = supervisor.await.expect("supervisor");
    drop(suspension);
    assert!(pending_while_suspended);
    assert!(matches!(end, DuplexEnd::Exited { code: 0 }));
    assert!(matches!(cleanup, CleanupCertainty::Complete));
}
