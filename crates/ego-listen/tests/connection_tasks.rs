//! Contracts for the listener-owned accept task lifecycle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use ego_core::endpoint::{PublicBind, Requested, admit};
use ego_listen::{ConnectionGate, ConnectionLimits, GuardedAccepted, bind, serve_connections};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::{Notify, oneshot};

async fn bound_loopback() -> ego_listen::Bound {
    let admitted = admit(
        Requested::Tcp("127.0.0.1:0".parse().unwrap()),
        PublicBind::Refused,
    )
    .unwrap();
    bind(&admitted).await.unwrap()
}

async fn wait_for_count(count: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while count.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("handler count never reached {expected}"));
}

#[tokio::test]
async fn ordinary_handler_completion_permits_a_new_accept() {
    let bound = bound_loopback().await;
    let address = bound.advertised().unwrap();
    let gate =
        ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 1024).unwrap());
    let handled = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&handled);
    let (stop, shutdown) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_connections(
        bound,
        gate,
        move |accepted: GuardedAccepted| {
            let observed = Arc::clone(&observed);
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                drop(accepted);
            }
        },
        async move {
            let _ = shutdown.await;
        },
    ));

    let first = TcpStream::connect(&address).await.unwrap();
    wait_for_count(&handled, 1).await;
    let second = TcpStream::connect(&address).await.unwrap();
    wait_for_count(&handled, 2).await;
    drop(first);
    drop(second);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn max_one_blocks_the_second_handler_until_the_first_finishes() {
    let bound = bound_loopback().await;
    let address = bound.advertised().unwrap();
    let gate =
        ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 1024).unwrap());
    let handled = Arc::new(AtomicUsize::new(0));
    let first_started = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());
    let (stop, shutdown) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_connections(
        bound,
        gate,
        {
            let handled = Arc::clone(&handled);
            let first_started = Arc::clone(&first_started);
            let release_first = Arc::clone(&release_first);
            move |accepted: GuardedAccepted| {
                let ordinal = handled.fetch_add(1, Ordering::SeqCst) + 1;
                let first_started = Arc::clone(&first_started);
                let release_first = Arc::clone(&release_first);
                async move {
                    if ordinal == 1 {
                        first_started.notify_one();
                        release_first.notified().await;
                    }
                    drop(accepted);
                }
            }
        },
        async move {
            let _ = shutdown.await;
        },
    ));

    let first = TcpStream::connect(&address).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), first_started.notified())
        .await
        .unwrap();
    let second = TcpStream::connect(&address).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), async {
            while handled.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "the second handler ran while the first connection still held the only permit"
    );
    release_first.notify_one();
    wait_for_count(&handled, 2).await;

    drop(first);
    drop(second);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn explicit_shutdown_joins_while_accept_is_pending() {
    let bound = bound_loopback().await;
    let gate = ConnectionGate::new(ConnectionLimits::default());
    let (stop, shutdown) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_connections(
        bound,
        gate,
        |_accepted: GuardedAccepted| async {},
        async move {
            let _ = shutdown.await;
        },
    ));

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("explicit shutdown joins an idle accept loop")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn external_gate_cancellation_wakes_a_guard_pending_connection() {
    let bound = bound_loopback().await;
    let address = bound.advertised().unwrap();
    let gate =
        ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 1024).unwrap());
    let handled = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let gate_for_test = gate.clone();
    let (_stop, shutdown) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_connections(
        bound,
        gate,
        {
            let handled = Arc::clone(&handled);
            let started = Arc::clone(&started);
            move |accepted: GuardedAccepted| {
                let handled = Arc::clone(&handled);
                let started = Arc::clone(&started);
                async move {
                    handled.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    let _accepted = accepted;
                    std::future::pending::<()>().await;
                }
            }
        },
        async move {
            let _ = shutdown.await;
        },
    ));

    let first = TcpStream::connect(&address).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .unwrap();
    let mut second = TcpStream::connect(&address).await.unwrap();
    tokio::task::yield_now().await;
    gate_for_test.shutdown();
    assert_eq!(handled.load(Ordering::SeqCst), 1);
    let mut closed = Vec::new();
    let read_result = tokio::time::timeout(Duration::from_secs(1), second.read_to_end(&mut closed))
        .await
        .expect("gate cancellation closes the guard-pending stream");
    let valid_close = match &read_result {
        // A clean close is EOF and therefore cannot carry bytes. On some
        // Unix runs the peer observes the listener's cancellation as a reset;
        // both are valid, but arbitrary successful reads are not closure.
        Ok(_) => closed.is_empty(),
        Err(error) => error.kind() == std::io::ErrorKind::ConnectionReset,
    };
    assert!(
        valid_close,
        "gate cancellation should close the stream with EOF or reset, got {read_result:?}"
    );
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("gate cancellation terminates the listener")
        .unwrap()
        .unwrap();
    drop(first);
}

struct DropSentinel(Arc<AtomicBool>);

impl Drop for DropSentinel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn shutdown_cancels_and_joins_a_handler_waiting_on_a_non_io_future() {
    let bound = bound_loopback().await;
    let address = bound.advertised().unwrap();
    let gate =
        ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 1024).unwrap());
    let dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(Notify::new());
    let (stop, shutdown) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_connections(
        bound,
        gate,
        {
            let dropped = Arc::clone(&dropped);
            let started = Arc::clone(&started);
            move |accepted: GuardedAccepted| {
                let dropped = Arc::clone(&dropped);
                let started = Arc::clone(&started);
                async move {
                    let _sentinel = DropSentinel(dropped);
                    let _accepted = accepted;
                    started.notify_one();
                    std::future::pending::<()>().await;
                }
            }
        },
        async move {
            let _ = shutdown.await;
        },
    ));

    let mut peer = TcpStream::connect(&address).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .unwrap();
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("shutdown cancels and joins the pending handler")
        .unwrap()
        .unwrap();
    assert!(
        dropped.load(Ordering::SeqCst),
        "the handler future was not dropped"
    );

    let mut eof = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut eof))
        .await
        .expect("the accepted stream closes after handler cancellation")
        .unwrap();
}

#[tokio::test]
async fn handler_panic_is_fatal_and_cleans_up_the_listener() {
    let bound = bound_loopback().await;
    let address = bound.advertised().unwrap();
    let gate =
        ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 4096).unwrap());
    let started = Arc::new(Notify::new());
    let (stop, shutdown) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_connections(
        bound,
        gate,
        {
            let started = Arc::clone(&started);
            move |_accepted: GuardedAccepted| {
                let started = Arc::clone(&started);
                async move {
                    started.notify_one();
                    panic!("handler panic is fatal to the listener");
                }
            }
        },
        async move {
            let _ = shutdown.await;
        },
    ));

    let first = TcpStream::connect(&address).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("a handler panic terminates the listener")
        .unwrap();
    assert!(result.is_err(), "a handler panic was reported as success");
    drop(first);
    assert!(
        TcpStream::connect(&address).await.is_err(),
        "the listener accepted another peer after a fatal handler panic"
    );
    let _ = stop.send(());
}
