//! RED contract for one shared root budget authority across session owners.

use std::sync::Arc;

use ego_core::budget::{BudgetGate, Metering, SharedBudgetGate};
use ego_core::cost::{CostDelta, CurrencyCode, UnknownCost};
use ego_core::cost::MeteringRevision;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};

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
fn provider_reservation_is_admitted_before_pending_io_without_holding_a_budget_guard() {
    let currency = CurrencyCode::new("USD").expect("currency");
    let root = Arc::new(SharedBudgetGate::new(BudgetGate::unlimited(currency)));
    let metering = Metering::Unmetered {
        revision: MeteringRevision("regression_case".to_owned()),
    };
    let request = GenRequest {
        model: "provider/model".to_owned(),
        messages: Conversation::new(vec![Message::text(Role::User, "pending".to_owned())])
            .expect("valid conversation"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(32),
        parallel_tool_calls: false,
    };
    let caps = ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        max_context_tokens: Some(1024),
        max_output_tokens: Some(32),
        max_tools: Some(8),
        supports_parallel_tool_calls: false,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
    };
    let first = root
        .reserve(&metering, None, &request, &caps)
        .expect("root reservation");
    let second = root
        .reserve(&metering, None, &request, &caps)
        .expect("independent owner can reserve while provider is pending");
    assert_eq!(first.gate(), root.id());
    assert_eq!(second.gate(), root.id());
    root.settle(first, &CostDelta::Unknown(UnknownCost::NoPricingQuote))
        .expect("root settlement consumes its reservation");
    root.settle(second, &CostDelta::Unknown(UnknownCost::NoPricingQuote))
        .expect("second owner settlement consumes its reservation");
}
