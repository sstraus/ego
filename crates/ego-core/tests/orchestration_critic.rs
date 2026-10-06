use ego_core::orchestration_events::{Cursor, Observation, Salience};

fn result(peer: &str, at_ms: u64) -> Observation {
    Observation {
        text: "RESULT: targeted tests passed".into(),
        at_ms,
        result: None,
        peer: Some(peer.into()),
        sender: None,
    }
}

// Catches: content-only deduplication discards a different child's completion.
#[test]
fn identical_results_from_distinct_children_both_produce_cards() {
    let mut cursor = Cursor::default();
    cursor.observe(result("worker-a", 0), 0);
    cursor.observe(result("worker-b", 1), 1);
    assert_eq!(cursor.ready(20_000).len(), 2);
}

// Catches: sorting source timestamps reverses actual observation arrival order.
#[test]
fn delayed_event_keeps_arrival_order_within_card_batch() {
    let mut cursor = Cursor::default();
    let first = Observation {
        text: "BLOCKED: first arrival".into(),
        at_ms: 5_000,
        result: None,
        peer: Some("worker-a".into()),
        sender: None,
    };
    let second = Observation {
        text: "BLOCKED: delayed second arrival".into(),
        at_ms: 1_000,
        result: None,
        peer: Some("worker-b".into()),
        sender: None,
    };
    assert_eq!(cursor.observe(first, 10_000), Salience::Card);
    assert_eq!(cursor.observe(second, 10_001), Salience::Card);
    let texts: Vec<_> = cursor
        .ready(30_000)
        .iter()
        .map(|card| card.text.as_str())
        .collect();
    assert_eq!(
        texts,
        ["BLOCKED: first arrival", "BLOCKED: delayed second arrival"]
    );
}
