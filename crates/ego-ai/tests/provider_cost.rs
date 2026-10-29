//! Two providers that agree about nothing on the wire, settling through one
//! `ego-core` path.
//!
//! See `docs/guides/providers.md` §8, whose rule
//! this file exists to make falsifiable:
//!
//! > Adding a second provider-reported decoder must require no change to
//! > `budget.rs`, `cost.rs` or `port.rs`.
//!
//! A single decoder cannot demonstrate that. One implementation always fits
//! the mechanism built for it, and a `match` on the provider id hidden inside
//! the wire format would look identical from outside. So two ship, chosen
//! because they share no wire convention at all:
//!
//! - **xAI** states `usage.cost_in_usd_ticks`, a JSON *integer* counting
//!   `1e-10` USD — `12345678` ticks is $0.0012345678.
//! - **OpenRouter** states `usage.cost`, a JSON *decimal* already in USD.
//!
//! Different field names, different JSON types, different units, a factor of
//! `1e10` between the numbers on the wire. Everything downstream of
//! `ProviderCost::read` — the event, the gate, the attribution, the settled
//! `Money` — is byte-for-byte the same code, and one test here asserts that by
//! running both through it and comparing the *shape* of what came out, not
//! just the figures.
//!
//! Two shipped decoders still leave the rule half-proved, though, because both
//! were written by whoever built the mechanism. So a **third** lives at the
//! bottom of this file rather than in `src/`: `TollgateStated`, a convention
//! that agrees with neither, written where it can reach nothing but public
//! API. `ego-ai` depends on `ego-core` and never the reverse, so a decoder
//! that needed something added to `budget.rs`, `cost.rs` or `port.rs` could
//! not be written from here at all — which turns "requires no change" from a
//! promise into something the compiler checks.

use std::sync::{Arc, Mutex};

use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
use ego_ai::provider::{ProviderRow, ResolvedCredential};
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_ai::wire::provider_cost::{
    OPENROUTER_COST_FIELD, OpenRouterUsd, ProviderCost, XAI_COST_FIELD, XaiUsdTicks,
};
use ego_ai::{catalogue, generator::WireGenerator};
use ego_core::budget::{BudgetGate, Metering, PricingQuote, SettlementSource};
use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CurrencyCode, Money, PricingRevision, ProviderCostRevision,
    UnknownCost,
};
use ego_core::event::id::GenerationId;
use ego_core::generator::attempt::{GenerationAttempt, Settlement, UsageReport, settled_cost};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::port::{AiEvent, Generator};
use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};
use ego_core::permit::{HttpHeader, OneHopResponse, TransportError, TransportFailure};
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::value::RawValue;
use url::Url;

mod support;
use support::FakeCredentials;

// --- the two wire conventions ---

/// One `usage` object as each provider sends it, and nothing else. The
/// decoders read only this, so the rest of the response is not the subject.
fn usage_object(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_string()).expect("valid JSON")
}

/// xAI: an integer tick count. 12_345_678 ticks = $0.0012345678.
const XAI_USAGE: &str = r#"{"prompt_tokens":1000,"completion_tokens":250,
  "total_tokens":1250,"cost_in_usd_ticks":12345678}"#;

/// OpenRouter: a decimal already in dollars, and deliberately not a figure
/// any `f64` renders back as itself.
const OPENROUTER_USAGE: &str = r#"{"prompt_tokens":1000,"completion_tokens":250,
  "total_tokens":1250,"cost":0.0000123456}"#;

fn usd(decimal: &str) -> Money {
    Money::new(
        decimal.parse::<Amount>().expect("a valid amount"),
        CurrencyCode::new("USD").expect("USD is valid"),
    )
}

// --- decoding, per provider ---

#[test]
fn xai_reads_an_integer_tick_count_as_exact_dollars() {
    let decoder = XaiUsdTicks::new("2026-08-21");
    assert_eq!(
        decoder.read(&usage_object(XAI_USAGE)),
        Some(usd("0.0012345678"))
    );
    assert_eq!(decoder.revision().provider, "xai");
    assert_eq!(decoder.revision().field, XAI_COST_FIELD);
}

#[test]
fn openrouter_reads_a_decimal_without_ever_touching_an_f64() {
    let decoder = OpenRouterUsd::new("2026-08-21");
    // The `f64` nearest to 0.0000123456 renders as `1.23456e-5`, an
    // exponential form `Amount` refuses outright — so a decoder that had gone
    // through `serde_json::Value` would return `None` here rather than a
    // slightly wrong figure. Getting the exact digits back is the proof the
    // raw text survived.
    assert_eq!(
        decoder.read(&usage_object(OPENROUTER_USAGE)),
        Some(usd("0.0000123456"))
    );
    assert_eq!(decoder.revision().provider, "openrouter");
    assert_eq!(decoder.revision().field, OPENROUTER_COST_FIELD);
}

/// A `usage` object without the declared field is not zero. A row that
/// declared it states a cost and then did not is the case
/// `UnknownCost::ProviderReportedCostMissing` exists for, and reading absence
/// as free would under-bill silently.
#[test]
fn a_usage_object_missing_the_declared_field_reads_as_nothing_not_as_zero() {
    let bare = usage_object(r#"{"prompt_tokens":1000,"completion_tokens":250}"#);
    assert_eq!(XaiUsdTicks::new("2026-08-21").read(&bare), None);
    assert_eq!(OpenRouterUsd::new("2026-08-21").read(&bare), None);
}

/// A stated zero is a real charge of nothing — a fully cached request — and
/// is not the same fact as a missing field.
#[test]
fn a_stated_zero_is_a_charge_of_zero_and_not_an_absent_one() {
    assert_eq!(
        XaiUsdTicks::new("2026-08-21").read(&usage_object(r#"{"cost_in_usd_ticks":0}"#)),
        Some(usd("0"))
    );
    assert_eq!(
        OpenRouterUsd::new("2026-08-21").read(&usage_object(r#"{"cost":0}"#)),
        Some(usd("0"))
    );
}

/// A figure this decoder cannot represent exactly is refused, not rounded.
/// `Amount` holds twelve decimal places; rounding a charge to fit is a
/// decision a decoder is not authorised to take, and the generation settles
/// at its bound instead — which errs towards spending less, not more.
#[test]
fn a_figure_too_fine_to_represent_is_refused_rather_than_rounded() {
    assert_eq!(
        OpenRouterUsd::new("2026-08-21").read(&usage_object(r#"{"cost":0.0000000000001}"#)),
        None,
        "thirteen decimal places is one more than Amount carries"
    );
    assert_eq!(
        XaiUsdTicks::new("2026-08-21").read(&usage_object(r#"{"cost_in_usd_ticks":1.5}"#)),
        None,
        "a fractional tick means the unit is no longer what this decoder reads"
    );
}

// --- the shared path ---

fn quote() -> PricingQuote {
    let rate = |d: &str| Some(d.parse::<Amount>().expect("a valid rate"));
    PricingQuote {
        web_search: None,
        revision: PricingRevision("supplied-by-the-caller".to_string()),
        currency: CurrencyCode::new("USD").expect("USD is valid"),
        uncached_input: rate("0.000003"),
        cache_read: rate("0.0000003"),
        cache_write: rate("0.00000375"),
        cache_write_extended: None,
        output: rate("0.000015"),
    }
}

fn caps() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        max_tools: Some(128),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::SameProviderOnly,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Long,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

fn request() -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
        model: "grok-5".to_string(),
        messages: Conversation::new(vec![Message::text(Role::User, "x".repeat(200_000))])
            .expect("one user message is a conversation"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(1_024),
        parallel_tool_calls: false,
    }
}

/// A transcript in the shape `openai-completions` decodes, carrying the given
/// provider's own `usage` object on the final frame.
fn transcript(usage: &str) -> String {
    let flat: String = usage.split_whitespace().collect();
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"ok\"}},\
         \"finish_reason\":null}}],\"usage\":null}}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\
         \"usage\":null}}\n\n\
         data: {{\"choices\":[],\"usage\":{flat}}}\n\n\
         data: [DONE]\n\n"
    )
}

struct StubHttp {
    body: Arc<Mutex<Option<String>>>,
}

impl StubHttp {
    fn new(body: String) -> Arc<Self> {
        Arc::new(StubHttp {
            body: Arc::new(Mutex::new(Some(body))),
        })
    }
}

impl Http for StubHttp {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(StubCall(Arc::clone(&self.body)))
    }
}

struct StubCall(Arc<Mutex<Option<String>>>);

impl HttpCall for StubCall {
    fn execute(
        &mut self,
        _call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let body = self.0.lock().expect("not poisoned").take();
        Box::pin(async move {
            match body {
                Some(body) => Ok(OneHopResponse::response(
                    200,
                    vec![
                        HttpHeader::new("content-type", b"text/event-stream".to_vec())
                            .expect("fixture header"),
                    ],
                    body.into_bytes(),
                )),
                None => Err(TransportError::definitely_unsent(
                    TransportFailure::Exchange("the stub was asked twice".to_owned()),
                )),
            }
        })
    }
}

fn row(id: &str, cost: Arc<dyn ProviderCost>) -> ProviderRow {
    let base = Url::parse("https://api.example/v1/").expect("a valid url");
    let mut row = match id {
        // The decoder remains independently tested, but the retired xAI
        // API-key row is no longer a fixture authority.
        "xai" => ProviderRow::openrouter(base.clone()),
        "openrouter" => ProviderRow::openrouter(base),
        other => panic!("unsupported provider fixture {other}"),
    };
    row.cost = Some(cost);
    row
}

/// A machine where every provider's key is configured.
///
/// Authentication is not what this file is about, but the rows above are the
/// real thing rather than a keyless simplification — a fixture that could not
/// authenticate would not exercise the same path a priced run takes.
fn creds(row: &ProviderRow) -> ResolvedCredential {
    row.credential
        .resolve(&FakeCredentials::holding("sk-not-a-real-key"))
        .expect("the fake holds every variable")
}

/// Everything from the socket to the committed total, for one provider.
///
/// Deliberately *the whole path*: the row's decoder, the wire format, the
/// port's event, `AttemptStream`'s fold, and `BudgetGate::reconcile`. A test
/// that reached into `settled_cost` directly would prove the arithmetic and
/// nothing about the independence, because it would skip every layer the two
/// providers are supposed to share.
async fn settle_through_core(
    id: &str,
    decoder: Arc<dyn ProviderCost>,
    usage: &str,
) -> (BudgetGate, Settlement, Vec<AiEvent>) {
    let metering = catalogue::provider_reported(decoder.as_ref());
    let mut gate = BudgetGate::new(usd("10"));
    let admission = gate
        .admit(&metering, Some(&quote()), &request(), &caps())
        .expect("admission does not reserve a monetary upper bound");
    let attempt = GenerationAttempt::new(
        GenerationId::new(),
        admission,
        &metering,
        Some(&quote()),
        &request(),
        &caps(),
    )
    .expect("built from the request that was priced");

    let row = row(id, decoder);
    let credential = creds(&row);
    let generator = WireGenerator::new(
        row,
        &OpenAiCompletions,
        StubHttp::new(transcript(usage)) as Arc<dyn Http>,
        credential,
    );
    let mut stream = generator.generate(request(), attempt);
    let events: Vec<AiEvent> = stream.by_ref().collect().await;
    let settlement = stream.settle(&mut gate).expect("its own gate settles it");
    (gate, settlement, events)
}

/// The event the port gained, produced by a real decoder reading a real
/// transcript — not by a test constructing it directly.
#[tokio::test]
async fn a_stated_cost_reaches_the_port_as_a_provider_neutral_event() {
    let (_, _, events) =
        settle_through_core("xai", Arc::new(XaiUsdTicks::new("2026-08-21")), XAI_USAGE).await;

    assert!(
        events.contains(&AiEvent::ProviderCost {
            cost: usd("0.0012345678")
        }),
        "the decoder's figure must reach the port: {events:?}"
    );
}

/// The claim, both halves of it: the same code settles both providers, and
/// it settles them at their own stated figures rather than at the table.
#[tokio::test]
async fn two_unrelated_wire_conventions_settle_through_the_identical_core_path() {
    let (xai_gate, xai, _) =
        settle_through_core("xai", Arc::new(XaiUsdTicks::new("2026-08-21")), XAI_USAGE).await;
    let (or_gate, openrouter, _) = settle_through_core(
        "openrouter",
        Arc::new(OpenRouterUsd::new("2026-08-21")),
        OPENROUTER_USAGE,
    )
    .await;

    // Each committed its own provider's figure, exactly.
    assert_eq!(xai.reconciled.charged, Some(usd("0.0012345678")));
    assert_eq!(openrouter.reconciled.charged, Some(usd("0.0000123456")));
    assert_eq!(
        *xai_gate.committed().expect("a limited gate keeps totals"),
        usd("0.0012345678")
    );
    assert_eq!(
        *or_gate.committed().expect("a limited gate keeps totals"),
        usd("0.0000123456")
    );

    // Both exact, both released, neither a breach — the same three
    // properties, from the same `reconcile`.
    for settled in [&xai, &openrouter] {
        assert!(settled.reconciled.exact, "a stated total is not a guess");
    }

    // And neither is the table's answer. The transcript reports 1000 prompt
    // tokens and 250 output, which the quote prices at $0.00675 — orders of
    // magnitude away from both. If settlement had silently fallen back to the
    // quote, both figures would be that one, and identical to each other.
    let from_the_table = quote().price(&ego_core::state::Usage {
        uncached_input_tokens: 1_000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_extended_tokens: 0,
        output_tokens: 250,
        web_search_requests: 0,
        reasoning_tokens: None,
    });
    let CostDelta::Known {
        amount: tabulated, ..
    } = from_the_table
    else {
        panic!("the fixture quote prices every category");
    };
    assert_ne!(xai.reconciled.charged, Some(tabulated));
    assert_ne!(openrouter.reconciled.charged, Some(tabulated));
}

/// The attribution that reaches the log names *which* provider contract
/// produced the figure, and the two must not be interchangeable — a log that
/// said only "the provider said so" could not be re-checked against anything.
#[tokio::test]
async fn each_settlement_is_attributed_to_the_contract_that_produced_it() {
    let xai = XaiUsdTicks::new("2026-08-21");
    let openrouter = OpenRouterUsd::new("2026-08-21");

    let attribution = |metering: &Metering, cost: Money| match settled_cost(
        metering,
        Some(&quote()),
        UsageReport::Missing,
        Some(cost),
        CurrencyCode::new("USD").expect("USD is valid"),
    ) {
        CostDelta::Known { attribution, .. } => attribution,
        other => panic!("a stated cost is known: {other:?}"),
    };

    let from_xai = attribution(&catalogue::provider_reported(&xai), usd("0.0012345678"));
    let from_openrouter = attribution(
        &catalogue::provider_reported(&openrouter),
        usd("0.0000123456"),
    );

    assert_eq!(
        from_xai,
        CostAttribution::ProviderReported(ProviderCostRevision {
            provider: "xai".to_string(),
            field: XAI_COST_FIELD.to_string(),
            schema_revision: "2026-08-21".to_string(),
        })
    );
    assert_eq!(
        from_openrouter,
        CostAttribution::ProviderReported(ProviderCostRevision {
            provider: "openrouter".to_string(),
            field: OPENROUTER_COST_FIELD.to_string(),
            schema_revision: "2026-08-21".to_string(),
        })
    );
    assert_ne!(from_xai, from_openrouter);
}

/// A row that declared it states a cost, and then did not, stays unknown
/// and says which of the two silences it was.
///
/// The distinction matters: `ProviderReportedNoUsage` means no token counts
/// arrived, which is a quote-settled row's problem. This is a provider that
/// broke its own cost contract, and reading it as free would be a silent
/// under-bill on every generation.
#[tokio::test]
async fn a_provider_that_states_no_cost_stays_unknown_and_names_why() {
    const NO_COST: &str = r#"{"prompt_tokens":1000,"completion_tokens":250,"total_tokens":1250}"#;
    let (_gate, settled, events) =
        settle_through_core("xai", Arc::new(XaiUsdTicks::new("2026-08-21")), NO_COST).await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::ProviderCost { .. })),
        "nothing was stated, so nothing may be reported: {events:?}"
    );
    assert_eq!(
        settled.reconciled.charged, None,
        "missing actual cost is not an estimated bill"
    );
    assert!(
        !settled.reconciled.exact,
        "the actual cost is unknown and must not claim otherwise"
    );

    assert_eq!(
        settled_cost(
            &catalogue::provider_reported(&XaiUsdTicks::new("2026-08-21")),
            Some(&quote()),
            UsageReport::Reported(ego_core::state::Usage {
                uncached_input_tokens: 1_000,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_extended_tokens: 0,
                output_tokens: 250,
                web_search_requests: 0,
                reasoning_tokens: None,
            }),
            None,
            CurrencyCode::new("USD").expect("USD is valid"),
        ),
        CostDelta::Unknown(UnknownCost::ProviderReportedCostMissing),
        "usage arriving does not excuse a missing cost on a row that states one"
    );
}

/// A row that states no cost at all is not a provider-reported row, and the
/// two must not be confused: `cost: None` on the row means the settlement
/// comes from the table, and the table's answer is what turns up.
#[tokio::test]
async fn a_row_with_no_cost_decoder_settles_from_its_table_as_it_always_did() {
    let metering = Metering::Currency {
        settlement: SettlementSource::Quote,
    };
    let mut gate = BudgetGate::new(usd("10"));
    let admission = gate
        .admit(&metering, Some(&quote()), &request(), &caps())
        .expect("a $10 budget admits it");
    let attempt = GenerationAttempt::new(
        GenerationId::new(),
        admission,
        &metering,
        Some(&quote()),
        &request(),
        &caps(),
    )
    .expect("built from the request that was priced");

    // The transcript still carries xAI's cost field; the row simply does not
    // declare that it reads one, so nothing may read it.
    let plain = ProviderRow::openai_at(Url::parse("https://api.example/v1/").expect("a valid url"));
    let credential = creds(&plain);
    let generator = WireGenerator::new(
        plain,
        &OpenAiCompletions,
        StubHttp::new(transcript(XAI_USAGE)) as Arc<dyn Http>,
        credential,
    );
    let mut stream = generator.generate(request(), attempt);
    let events: Vec<AiEvent> = stream.by_ref().collect().await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::ProviderCost { .. })),
        "an undeclared field is not read just because it happens to be there: {events:?}"
    );

    let settled = stream.settle(&mut gate).expect("its own gate settles it");
    assert_eq!(
        settled.reconciled.charged,
        Some(usd("0.00675")),
        "the table"
    );
    assert!(settled.reconciled.exact);
}

// --- the third decoder, written from outside ---

/// The field this stranger states a charge in.
///
/// Nested, which is the first thing neither shipped decoder does. The
/// `field` helper they share reads one key off the top of the `usage` object
/// and is private to `ego-ai`; a decoder that needs a path has to bring its
/// own reader, and the fact that it *can* is the point.
const TOLLGATE_COST_FIELD: &str = "usage.billing.charged";

/// A provider that shares no wire convention with either shipped decoder.
///
/// The two that ship differ in field, unit and JSON type, and a reader might
/// still conclude that `ProviderCost` was shaped around the pair of them.
/// This one is the control: it disagrees with both on every axis at once —
/// a **nested** path, a decimal sent as a JSON **string**, and a currency
/// **read off the wire** rather than assumed. `provider_cost.rs` says a
/// decoder for another currency "would say so itself"; this is what saying
/// so looks like.
///
/// It lives here rather than in `src/` because that is the claim. An
/// integration test reaches public API only, and `ego-ai` depends on
/// `ego-core` and never the reverse — so if a third decoder needed anything
/// added to `budget.rs`, `cost.rs` or `port.rs`, this file could not be made
/// to compile from where it sits. Passing is the settlement; **compiling** is
/// the invariant.
#[derive(Debug)]
struct TollgateStated {
    revision: ProviderCostRevision,
}

impl TollgateStated {
    fn new(schema_revision: &str) -> Self {
        Self {
            revision: ProviderCostRevision {
                provider: "tollgate".to_string(),
                field: TOLLGATE_COST_FIELD.to_string(),
                schema_revision: schema_revision.to_string(),
            },
        }
    }
}

impl ProviderCost for TollgateStated {
    fn revision(&self) -> &ProviderCostRevision {
        &self.revision
    }

    /// The digits as sent, in the currency sent beside them.
    ///
    /// Borrowed straight out of the response bytes and parsed by `Amount`,
    /// for the same reason the shipped pair do it: a decimal charge that went
    /// through `f64` on its way here would already have been rounded. A
    /// `unit` this build cannot name as a currency reports nothing rather
    /// than guessing dollars, which is the direction that costs budget
    /// instead of mis-stating it.
    fn read(&self, usage: &RawValue) -> Option<Money> {
        #[derive(Deserialize)]
        struct Stated<'a> {
            #[serde(borrow)]
            billing: Option<Billing<'a>>,
        }

        #[derive(Deserialize)]
        struct Billing<'a> {
            charged: &'a str,
            unit: &'a str,
        }

        let billing = serde_json::from_str::<Stated<'_>>(usage.get())
            .ok()?
            .billing?;
        Some(Money::new(
            billing.charged.parse::<Amount>().ok()?,
            CurrencyCode::new(billing.unit).ok()?,
        ))
    }
}

/// Tollgate: a decimal in a JSON string, nested, with its currency beside it.
const TOLLGATE_USAGE: &str = r#"{"prompt_tokens":1000,"completion_tokens":250,
  "total_tokens":1250,"billing":{"charged":"0.004275","unit":"USD"}}"#;

/// A decoder nobody in `ego-ai` wrote settles a generation through the same
/// code the shipped two do.
///
/// This is the ADR rule's *sufficient* condition, and the counterpart of
/// `crates/ego-core/tests/near_side.rs`, which is its necessary one: that
/// grep proves the near side never names a provider, and this proves an
/// externally written decoder can use it. Neither implies the other — a
/// `match` on something other than a name would pass the grep, and a near
/// side that named providers freely could still happen to be extensible.
///
/// The figure asserted is Tollgate's own and is nowhere near the table's
/// $0.00675 for the same transcript, so a settlement that had quietly fallen
/// back to the quote fails here rather than looking correct.
#[tokio::test]
async fn a_decoder_written_outside_this_crate_settles_without_touching_the_near_side() {
    let (_gate, settled, events) = settle_through_core(
        "openrouter",
        Arc::new(TollgateStated::new("2026-08-22")),
        TOLLGATE_USAGE,
    )
    .await;

    assert!(
        events.contains(&AiEvent::ProviderCost {
            cost: usd("0.004275")
        }),
        "a stranger's figure reaches the port under the same event as anybody else's: {events:?}"
    );
    assert_eq!(settled.reconciled.charged, Some(usd("0.004275")));
    assert!(
        settled.reconciled.exact,
        "a stated total is not a guess, whoever stated it"
    );
}

/// The stranger's settlement is attributed to the stranger's own contract.
///
/// Without this the test above is satisfied by a figure that arrived
/// correctly and was then logged as somebody else's — the log would name a
/// provider contract that never produced it, and no later audit could tell.
#[tokio::test]
async fn the_stranger_is_attributed_to_its_own_contract_and_not_a_shipped_one() {
    let tollgate = TollgateStated::new("2026-08-22");
    let CostDelta::Known { attribution, .. } = settled_cost(
        &catalogue::provider_reported(&tollgate),
        Some(&quote()),
        UsageReport::Missing,
        Some(usd("0.004275")),
        CurrencyCode::new("USD").expect("USD is valid"),
    ) else {
        panic!("a stated cost is known");
    };

    assert_eq!(
        attribution,
        CostAttribution::ProviderReported(ProviderCostRevision {
            provider: "tollgate".to_string(),
            field: TOLLGATE_COST_FIELD.to_string(),
            schema_revision: "2026-08-22".to_string(),
        })
    );
}
