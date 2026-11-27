use super::{AdmissionAttempt, safe_mcp_error};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::advance;

/// Catches: evicting for an existing selection bypasses its backoff, and
/// keeping the oldest failure on overflow prevents that selection from starting.
#[tokio::test(start_paused = true)]
async fn failed_acp_admission_preserves_backoff_at_capacity_and_evicts_oldest_for_new_selection() {
    let state = Arc::new(Mutex::new(Default::default()));
    let begin = |selection| AdmissionAttempt::begin(state.clone(), [selection; 32], false);

    for selection in 0..64 {
        begin(selection)
            .await
            .expect("each new selection can start")
            .finish(Err(safe_mcp_error("discovery failed")));
        advance(Duration::from_millis(1)).await;
    }

    // The oldest failure is one millisecond short of its initial expiry.
    advance(Duration::from_millis(59_935)).await;
    let Err(refusal) = begin(0).await else {
        panic!("the oldest selection must still be in backoff at capacity");
    };
    assert!(refusal.to_string().contains("retry later"), "{refusal}");

    begin(64)
        .await
        .expect("a new selection can start at capacity")
        .finish(Err(safe_mcp_error("discovery failed")));
    assert!(
        begin(1).await.is_err(),
        "the next-oldest selection must retain its backoff through overflow"
    );
    begin(0)
        .await
        .expect("the oldest selection can start again immediately after eviction")
        .finish(Ok(()));

    // Re-admitting 0 at capacity can evict 1. Selection 2 must still cool down.
    advance(Duration::from_millis(2)).await;
    assert!(begin(2).await.is_err(), "backoff ends after 60 seconds");
    advance(Duration::from_millis(1)).await;
    begin(2)
        .await
        .expect("the retained selection can start at the 60-second boundary")
        .finish(Ok(()));
}
