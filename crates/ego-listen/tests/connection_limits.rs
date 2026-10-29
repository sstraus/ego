//! Listener-owned connection limits and shutdown contracts.

use std::time::Duration;

use ego_listen::{ConnectionGate, ConnectionGateError, ConnectionLimits};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn defaults_are_nonzero_and_all_dimensions_are_validated() {
    let limits = ConnectionLimits::default();
    assert_eq!(limits.max_connections(), 32);
    assert_eq!(limits.connection_lifetime(), Duration::from_secs(86_400));
    assert_eq!(limits.max_ingress_bytes(), 134_217_728);

    assert!(ConnectionLimits::new(0, Duration::from_secs(1), 1).is_err());
    assert!(ConnectionLimits::new(1, Duration::ZERO, 1).is_err());
    assert!(ConnectionLimits::new(1, Duration::from_secs(1), 0).is_err());
}

#[tokio::test]
async fn shutdown_wakes_blocked_admission_and_refuses_new_admission() {
    let gate = ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 16).unwrap());
    let (_first_peer, first_stream) = tokio::io::duplex(32);
    let first = gate.guard(first_stream).await.unwrap();
    let (_second_peer, second_stream) = tokio::io::duplex(32);
    let waiting = tokio::spawn({
        let gate = gate.clone();
        async move { gate.guard(second_stream).await }
    });
    tokio::task::yield_now().await;
    gate.shutdown();
    assert!(matches!(
        waiting.await.unwrap(),
        Err(ConnectionGateError::ShuttingDown)
    ));
    let (_third_peer, third_stream) = tokio::io::duplex(32);
    assert!(matches!(
        gate.guard(third_stream).await,
        Err(ConnectionGateError::ShuttingDown)
    ));
    drop(first);
}

#[tokio::test]
async fn shutdown_wakes_pending_read_and_backpressured_write() {
    let gate =
        ConnectionGate::new(ConnectionLimits::new(2, Duration::from_secs(60), 1024).unwrap());
    let (_read_peer, read_stream) = tokio::io::duplex(32);
    let mut guarded_read = gate.guard(read_stream).await.unwrap();
    let pending_read = tokio::spawn(async move {
        guarded_read
            .read_exact(&mut [0; 1])
            .await
            .unwrap_err()
            .kind()
    });
    tokio::task::yield_now().await;
    gate.shutdown();
    assert_eq!(
        pending_read.await.unwrap(),
        std::io::ErrorKind::ConnectionAborted
    );

    let gate =
        ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 4096).unwrap());
    let (_write_peer, write_stream) = tokio::io::duplex(1);
    let mut guarded_write = gate.guard(write_stream).await.unwrap();
    let pending_write = tokio::spawn(async move {
        guarded_write
            .write_all(&[7; 4096])
            .await
            .unwrap_err()
            .kind()
    });
    tokio::task::yield_now().await;
    gate.shutdown();
    assert_eq!(
        pending_write.await.unwrap(),
        std::io::ErrorKind::ConnectionAborted
    );
}

#[tokio::test]
async fn split_halves_retain_the_permit_until_both_halves_are_dropped() {
    let gate = ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 16).unwrap());
    let (_peer, stream) = tokio::io::duplex(32);
    let guarded = gate.guard(stream).await.unwrap();
    let (read_half, write_half) = tokio::io::split(guarded);
    drop(read_half);

    let (_blocked_peer, blocked_stream) = tokio::io::duplex(32);
    let blocked = tokio::spawn({
        let gate = gate.clone();
        async move { gate.guard(blocked_stream).await }
    });
    tokio::task::yield_now().await;
    assert!(!blocked.is_finished());
    drop(write_half);
    assert!(blocked.await.unwrap().is_ok());
}

#[tokio::test]
async fn shutdown_and_drain_waits_for_guard_drop_and_completes_afterward() {
    let gate = ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 16).unwrap());
    let (_peer, stream) = tokio::io::duplex(32);
    let guarded = gate.guard(stream).await.unwrap();
    let draining = tokio::spawn({
        let gate = gate.clone();
        async move { gate.shutdown_and_drain().await }
    });
    tokio::task::yield_now().await;
    assert!(!draining.is_finished());
    drop(guarded);
    assert!(draining.await.unwrap().is_ok());
}

#[tokio::test]
async fn independent_gates_do_not_share_shutdown_or_occupancy() {
    let first_gate =
        ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 16).unwrap());
    let second_gate =
        ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 16).unwrap());
    let (_first_peer, first_stream) = tokio::io::duplex(32);
    let first = first_gate.guard(first_stream).await.unwrap();
    first_gate.shutdown();
    let (_second_peer, second_stream) = tokio::io::duplex(32);
    assert!(second_gate.guard(second_stream).await.is_ok());
    drop(first);
}

#[tokio::test]
async fn absolute_lifetime_wakes_a_pending_read_without_peer_data() {
    let gate =
        ConnectionGate::new(ConnectionLimits::new(1, Duration::from_millis(25), 4096).unwrap());
    let (_peer, stream) = tokio::io::duplex(32);
    let mut guarded = gate.guard(stream).await.unwrap();
    let result = guarded.read_exact(&mut [0; 1]).await;
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn cumulative_ingress_budget_refuses_after_the_budget_without_timeout_race() {
    let gate = ConnectionGate::new(ConnectionLimits::new(1, Duration::from_secs(60), 4).unwrap());
    let (mut peer, stream) = tokio::io::duplex(32);
    let mut guarded = gate.guard(stream).await.unwrap();

    peer.write_all(b"ab").await.unwrap();
    let mut first = [0; 2];
    guarded.read_exact(&mut first).await.unwrap();
    peer.write_all(b"cd").await.unwrap();
    let mut second = [0; 2];
    guarded.read_exact(&mut second).await.unwrap();
    peer.write_all(b"e").await.unwrap();
    assert_eq!(
        guarded.read_exact(&mut [0; 1]).await.unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}
