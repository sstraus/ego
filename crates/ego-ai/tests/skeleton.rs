//! `SkeletonGenerator` drives a real attempt to a real settlement.
//!
//! See `plans/m0b-vertical-slice.md` Step 5. `ego-core`'s own tests cover the
//! attempt lifecycle against `FakeGenerator`; this one exists because the
//! other half of the claim can only be shown from *outside* `ego-core`.
//!
//! What it proves is about the crate DAG, not about the skeleton: an
//! implementor in a downstream crate can satisfy the `Generator` port, and
//! cannot route around the budget gate while doing so. `Reservation`'s
//! constructor is sealed inside `ego_core::budget` and `GenerationAttempt` is
//! the only thing that can hold one, so the fixture below has to go through
//! `BudgetGate::reserve` — there is no other door, and that is the point.

use ego_ai::SkeletonGenerator;
use ego_core::budget::{BudgetGate, PricingQuote};
use ego_core::cost::{Amount, CurrencyCode, Money, PricingRevision};
use ego_core::event::id::GenerationId;
use ego_core::generator::attempt::{DeliveryCertainty, GenerationAttempt, UsageReport};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, Transport,
};
use ego_core::generator::port::{AiEvent, Generator};
use ego_core::generator::types::{
    Conversation, GenRequest, Message, Role, StopReason, ThinkingLevel,
};
use futures::StreamExt;

fn usd(decimal: &str) -> Money {
    Money::new(
        decimal.parse::<Amount>().expect("a valid amount"),
        CurrencyCode::new("USD").expect("USD is valid"),
    )
}

fn quote() -> PricingQuote {
    PricingQuote {
        revision: PricingRevision("test-2026-08".to_string()),
        currency: CurrencyCode::new("USD").expect("USD is valid"),
        uncached_input: Some("0.000003".parse().unwrap()),
        cache_read: Some("0.0000003".parse().unwrap()),
        cache_write: Some("0.00000375".parse().unwrap()),
        output: Some("0.000015".parse().unwrap()),
    }
}

fn caps() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: 1,
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        max_tools: Some(128),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
    }
}

fn request() -> GenRequest {
    GenRequest {
        model: "claude-opus-5".to_string(),
        messages: Conversation::new(vec![Message::text(
            Role::User,
            "list the files".to_string(),
        )])
        .expect("one user message is a conversation"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(1_024),
        parallel_tool_calls: false,
        reasoning_state: None,
    }
}

/// The whole lifecycle across the crate boundary: reserve in `ego-core`,
/// generate in `ego-ai`, settle back in `ego-core`.
///
/// The skeleton streams nothing, which is the interesting case rather than a
/// degenerate one — it is exactly a provider that was called and reported
/// neither usage nor a terminal frame. That has to settle as an abort with a
/// missing usage report, charging the pessimistic bound, and it has to
/// release the hold: a provider that says nothing must not be able to strand
/// a gate's budget.
#[test]
fn the_skeleton_generator_drives_an_attempt_to_a_settlement() {
    let mut gate = BudgetGate::new(usd("10"));
    let reservation = gate
        .reserve(&quote(), &request(), &caps())
        .expect("a $10 budget admits an ordinary request");
    let bound = *reservation.upper_bound();
    let attempt = GenerationAttempt::new(
        GenerationId::new(),
        reservation,
        &quote(),
        &request(),
        &caps(),
    )
    .expect("built from the request that was priced");
    let generation = attempt.attempt();

    let mut stream = SkeletonGenerator.generate(request(), attempt);
    assert_eq!(
        stream.attempt(),
        generation,
        "ego-core stamps the identity; ego-ai carries it, it does not mint it"
    );

    let events: Vec<AiEvent> = futures::executor::block_on(stream.by_ref().collect());
    assert!(events.is_empty(), "the skeleton has no wire format yet");
    assert_eq!(
        *gate.outstanding(),
        bound,
        "the hold is still held until settlement"
    );

    let settled = stream
        .settle(&mut gate)
        .expect("the gate that minted the reservation settles it");

    assert_eq!(settled.attempt, generation);
    assert_eq!(settled.outcome.stop(), StopReason::Aborted);
    assert_eq!(settled.outcome.usage(), UsageReport::Missing);
    assert_eq!(
        settled.outcome.delivery(),
        DeliveryCertainty::DefinitelyUnsent,
        "nothing came back and nothing was reported sent, so a retry is safe"
    );
    assert!(settled.outcome.retry_is_safe());

    assert_eq!(
        settled.reconciled.charged, bound,
        "a provider that reported no usage is charged the pessimistic bound"
    );
    assert!(!settled.reconciled.exact);
    assert!(
        gate.outstanding().amount.is_zero(),
        "and the hold is released regardless: a silent provider must not \
         strand a gate's budget"
    );
}
