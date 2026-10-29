//! contract for one shared root budget authority across session owners.

use std::sync::Arc;

use ego_core::budget::{BudgetGate, Metering, SharedBudgetGate};
use ego_core::cost::CurrencyCode;
use ego_core::cost::MeteringRevision;
use ego_core::event::id::{GateId, GenerationId};
use ego_core::generator::attempt::{AttemptStream, GenerationAttempt};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::StopReason;
use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};
use futures::{Stream, StreamExt};

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
    let root = Arc::new(SharedBudgetGate::new(BudgetGate::unlimited(
        CurrencyCode::new("USD").expect("currency"),
    )));
    let (mut first, first_gate) = admitted_stream(&root, "first");
    let (mut second, second_gate) = admitted_stream(&root, "second");
    assert_eq!(first_gate, root.id());
    assert_eq!(second_gate, root.id());

    futures::executor::block_on(async {
        assert!(first.next().await.is_some());
        assert!(second.next().await.is_some());
    });
    root.settle(first).expect("first stream is consumed once");
    root.settle(second).expect("second stream is consumed once");
}

#[test]
fn provider_reservation_is_admitted_before_pending_io_without_holding_a_budget_guard() {
    let currency = CurrencyCode::new("USD").expect("currency");
    let root = Arc::new(SharedBudgetGate::new(BudgetGate::unlimited(currency)));
    let metering = Metering::Unmetered {
        revision: MeteringRevision("shared-runtime-ZVP".to_owned()),
    };
    let request = GenRequest {
        web_search: None,
        cache: None,
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
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
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
        default_reasoning_effort: None,
    };
    let (sender, receiver) = futures::channel::oneshot::channel();
    let reservation = root
        .admit(&metering, None, &request, &caps)
        .expect("root reservation");
    let first_gate = reservation.gate();
    let attempt = GenerationAttempt::new(
        GenerationId::new(),
        reservation,
        &metering,
        None,
        &request,
        &caps,
    )
    .expect("reservation authorises the pending provider request");
    let mut pending = AttemptStream::new(
        attempt,
        Box::pin(futures::stream::once(async move {
            let _ = receiver.await;
            AiEvent::Stop(StopReason::Stop)
        })),
    );
    let waker = futures::task::noop_waker_ref();
    let mut context = std::task::Context::from_waker(waker);
    assert!(matches!(
        std::pin::Pin::new(&mut pending).poll_next(&mut context),
        std::task::Poll::Pending
    ));

    let second = root
        .admit(&metering, None, &request, &caps)
        .expect("independent owner can reserve while provider is pending");
    assert_eq!(first_gate, root.id());
    assert_eq!(second.gate(), root.id());
    let second_gate = second.gate();
    let second_attempt = GenerationAttempt::new(
        GenerationId::new(),
        second,
        &metering,
        None,
        &request,
        &caps,
    )
    .expect("second reservation authorises independently");
    let mut second_stream = AttemptStream::new(
        second_attempt,
        Box::pin(futures::stream::iter([AiEvent::Stop(StopReason::Stop)])),
    );
    futures::executor::block_on(second_stream.next());
    let _ = sender.send(());
    futures::executor::block_on(pending.next());
    root.settle(pending)
        .expect("pending stream settles after provider completion");
    root.settle(second_stream)
        .expect("second stream settles exactly once");
    assert_eq!(second_gate, root.id());
}

fn admitted_stream(root: &SharedBudgetGate, model: &str) -> (AttemptStream, GateId) {
    let metering = Metering::Unmetered {
        revision: MeteringRevision("shared-runtime-ZVP".to_owned()),
    };
    let request = GenRequest {
        web_search: None,
        cache: None,
        model: model.to_owned(),
        messages: Conversation::new(vec![Message::text(Role::User, "pending".to_owned())])
            .expect("valid conversation"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(32),
        parallel_tool_calls: false,
    };
    let caps = ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
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
        default_reasoning_effort: None,
    };
    let reservation = root
        .admit(&metering, None, &request, &caps)
        .expect("reservation is admitted");
    let gate = reservation.gate();
    let attempt = GenerationAttempt::new(
        GenerationId::new(),
        reservation,
        &metering,
        None,
        &request,
        &caps,
    )
    .expect("reservation authorises stream");
    (
        AttemptStream::new(
            attempt,
            Box::pin(futures::stream::iter([AiEvent::Stop(StopReason::Stop)])),
        ),
        gate,
    )
}
