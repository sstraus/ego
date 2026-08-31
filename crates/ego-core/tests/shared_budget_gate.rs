//! RED contract for one shared root budget authority across session owners.

use std::sync::Arc;

use ego_core::budget::{SharedBudgetGate, BudgetGate};
use ego_core::cost::CurrencyCode;

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn root_prompt_and_child_clones_share_one_gate_id_and_ledger() {
    let currency = CurrencyCode::new("USD").expect("currency");
    let root = Arc::new(SharedBudgetGate::new(BudgetGate::unlimited(currency)));
    let prompt = Arc::clone(&root);
    let child = Arc::clone(&root);

    assert_eq!(root.id(), prompt.id());
    assert_eq!(root.id(), child.id());
    assert_eq!(root.snapshot(), prompt.snapshot());
    assert_eq!(root.snapshot(), child.snapshot());
    assert_send_sync::<SharedBudgetGate>();
}

#[test]
fn shared_gate_reservation_and_attempt_stream_are_consumed_once() {
    // This is deliberately compile-focused until the final shared reserve
    // port exists. The by-value signatures below must remain non-replayable:
    // a provider may settle one attempt, never clone its reservation or stream.
    fn consume_reservation(
        reservation: ego_core::budget::Reservation,
    ) -> ego_core::budget::Reservation {
        reservation
    }
    fn consume_stream(stream: ego_core::generator::attempt::AttemptStream) {
        drop(stream);
    }

    let _ = consume_reservation;
    let _ = consume_stream;
}

#[test]
fn provider_pending_work_can_progress_while_another_owner_appends() {
    // The shared gate API must return a bounded reservation before provider
    // I/O and release its synchronization guard before awaiting that I/O.
    // Keeping this assertion at the contract boundary prevents an async
    // adapter from accepting a MutexGuard-shaped implementation.
    fn append_while_provider_pending<'a>(
        gate: &'a SharedBudgetGate,
    ) -> impl std::future::Future<Output = Result<(), String>> + 'a {
        gate.append_probe_while_pending()
    }

    let _ = append_while_provider_pending;
}

