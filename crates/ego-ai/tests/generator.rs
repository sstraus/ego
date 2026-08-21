//! The binding: a `Generator` that actually reaches a provider.
//!
//! See `plans/m0b-vertical-slice.md` Step 13.5 and
//! `docs/decisions/2026-08-21-a-generator-that-actually-reaches-a-provider.md`.
//!
//! Steps 8, 9 and 10 shipped the `Http` seam, the openai-completions wire
//! format and the turn loop, each green in isolation and none of them joined
//! to the others. This file is about the join, so it tests exactly the join:
//! that the bytes the wire format would send are the bytes the transport is
//! handed, that the events the wire format would decode are the events the
//! caller gets, and — the part with no other owner — that a request refused
//! *before* a socket exists still ends the generation instead of returning
//! an empty stream that settles as if the provider went silent.
//!
//! The transport is a stub. That is not a shortcut: a live provider proves
//! nothing about composition that a recorded transcript does not, and it
//! cannot be made to fail on demand in the three ways that matter here.

use std::sync::{Arc, Mutex};

use ego_ai::generator::WireGenerator;
use ego_ai::http::{Http, HttpError, HttpRequest, HttpResponse, Method};
use ego_ai::provider::{Auth, ProviderRow};
use ego_ai::wire::openai_completions::{OpenAiCompletions, request_body};
use ego_core::budget::{BudgetGate, Metering, PricingQuote, SettlementSource};
use ego_core::cost::{Amount, CurrencyCode, Money, PricingRevision};
use ego_core::event::id::GenerationId;
use ego_core::generator::attempt::{DeliveryCertainty, GenerationAttempt};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::port::{AiEvent, Generator};
use ego_core::generator::types::{
    ContentBlock, Conversation, ErrorKind, GenRequest, Message, ReasoningState, Role, StopReason,
    ThinkingLevel,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use url::Url;

mod support;
use support::FakeCredentials;

const TEXT_TRANSCRIPT: &str = include_str!("goldens/stream_text.sse");

/// The variable the fixture row names, and the value a configured machine
/// would have put in it. Two constants rather than two literals: the tests
/// below assert that the first appears where the second must not.
const KEY_VAR: &str = "EGO_TEST_API_KEY";
const KEY: &str = "sk-not-a-real-key";

// --- fixtures ---

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
        cache_write_extended: None,
        output: Some("0.000015".parse().unwrap()),
    }
}

/// The fixture quote as a metering: a table that both bounds and settles.
fn metered() -> Metering {
    Metering::Currency {
        settlement: SettlementSource::Quote,
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
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
    }
}

fn request() -> GenRequest {
    GenRequest {
        model: "gpt-5.1".to_string(),
        messages: Conversation::new(vec![Message::text(Role::User, "list the files")])
            .expect("one user message is a conversation"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(1_024),
        parallel_tool_calls: false,
    }
}

/// [`request`] with one reasoning block in its history — the thing this wire
/// format has nowhere to put, and refuses rather than drops.
fn request_carrying_reasoning() -> GenRequest {
    GenRequest {
        messages: Conversation::new(vec![
            Message::text(Role::User, "list the files"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Reasoning {
                    state: ReasoningState {
                        blob: "opaque".to_string(),
                        dialect: "anthropic-messages".to_string(),
                    },
                }],
            },
        ])
        .expect("a reasoning block takes no part in the call invariants"),
        ..request()
    }
}

fn row() -> ProviderRow {
    ProviderRow {
        id: "stub".to_string(),
        endpoint: Url::parse("https://api.example/v1/chat/completions").expect("a valid url"),
        wire: &OpenAiCompletions,
        auth: Auth::BearerFromEnv {
            var: KEY_VAR.to_string(),
        },
        cost: None,
    }
}

/// The fixture row, over `http`, on a machine where the key is configured.
///
/// Every test below the credential section wants that machine — the
/// credential is not what they are about — so the three-argument constructor
/// is spelled once here rather than at each call site.
fn generator(http: Arc<dyn Http>) -> WireGenerator {
    WireGenerator::new(row(), http, Arc::new(FakeCredentials::holding(KEY)))
}

/// Mints a real attempt through the only door there is.
///
/// `Reservation`'s constructor is sealed in `ego_core::budget`, so this has to
/// go through `BudgetGate::reserve` — the same fact `tests/skeleton.rs` leans
/// on, and the reason a generator cannot route around the gate.
fn attempt_for(gate: &mut BudgetGate, req: &GenRequest) -> GenerationAttempt {
    let reservation = gate
        .reserve(&metered(), Some(&quote()), req, &caps())
        .expect("a $10 budget admits an ordinary request");
    GenerationAttempt::new(
        GenerationId::new(),
        reservation,
        &metered(),
        Some(&quote()),
        req,
        &caps(),
    )
    .expect("built from the request that was priced")
}

// --- the stub transport ---

/// Answers every request with one canned outcome, and keeps what it was
/// asked. The `Mutex` is what makes it `Sync`, which `Http` requires.
struct StubHttp {
    answer: Mutex<Option<Result<HttpResponse, HttpError>>>,
    seen: Mutex<Vec<HttpRequest>>,
}

impl StubHttp {
    fn new(answer: Result<HttpResponse, HttpError>) -> Arc<Self> {
        Arc::new(StubHttp {
            answer: Mutex::new(Some(answer)),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn responding(status: u16, body: &'static str) -> Arc<Self> {
        let chunks: Vec<Result<bytes::Bytes, HttpError>> = body
            .as_bytes()
            .chunks(7)
            .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
            .collect();
        StubHttp::new(Ok(HttpResponse {
            status,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: Box::pin(futures::stream::iter(chunks)),
        }))
    }

    fn calls(&self) -> usize {
        self.seen.lock().expect("not poisoned").len()
    }
}

impl Http for StubHttp {
    fn execute(&self, req: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        self.seen.lock().expect("not poisoned").push(req);
        let answer = self
            .answer
            .lock()
            .expect("not poisoned")
            .take()
            .unwrap_or_else(|| {
                Err(HttpError::Transport {
                    message: "the stub was asked twice".to_string(),
                    delivery: DeliveryCertainty::DefinitelyUnsent,
                })
            });
        Box::pin(async move { answer })
    }
}

// --- what the transport is handed ---

/// The join, in the only direction a test can see it: what the wire format
/// would have sent is what the transport is handed, byte for byte.
#[tokio::test]
async fn the_transport_is_handed_exactly_what_the_wire_format_would_send() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let mut gate = BudgetGate::new(usd("10"));
    let generator = generator(Arc::clone(&http) as Arc<dyn Http>);

    let attempt = attempt_for(&mut gate, &request());
    let _: Vec<AiEvent> = generator.generate(request(), attempt).collect().await;

    let seen = http.seen.lock().expect("not poisoned");
    let sent = seen.first().expect("the transport was called once");
    assert_eq!(sent.method, Method::Post);
    assert_eq!(sent.url, row().endpoint);
    assert_eq!(
        sent.body.as_deref(),
        Some(
            request_body(&request())
                .expect("this request is expressible")
                .as_slice()
        ),
        "the binding composes the wire format's bytes; it does not build its own"
    );
    assert!(
        sent.headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == &format!("Bearer {KEY}")),
        "the provider row's credential reaches the request: {:?}",
        sent.headers.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
}

/// The identity is stamped by `ego-core` and carried by `ego-ai`, never
/// minted here — the same claim `tests/skeleton.rs` makes for the skeleton,
/// which has to keep holding once there is a real provider behind it.
#[tokio::test]
async fn the_generation_carries_the_attempt_ego_core_stamped() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let mut gate = BudgetGate::new(usd("10"));
    let generator = generator(http as Arc<dyn Http>);

    let attempt = attempt_for(&mut gate, &request());
    let expected = attempt.attempt();

    let stream = generator.generate(request(), attempt);
    assert_eq!(stream.attempt(), expected);
}

// --- a request that never leaves ---

/// A request this format cannot express must end the generation, not return
/// an empty stream.
///
/// The distinction is the whole point of the test. An empty stream settles as
/// `Aborted` with `DefinitelyUnsent` — which reads as "the provider was
/// called and said nothing", and it was not called at all. The refusal is
/// local, so the certainty is honest for the same reason
/// `HttpError::refused_before_sending` is: no DNS, no socket, no bytes.
#[tokio::test]
async fn a_request_this_format_cannot_express_stops_the_generation() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let mut gate = BudgetGate::new(usd("10"));
    let generator = generator(Arc::clone(&http) as Arc<dyn Http>);

    let req = request_carrying_reasoning();
    let attempt = attempt_for(&mut gate, &req);

    let events: Vec<AiEvent> = generator.generate(req, attempt).collect().await;

    assert_eq!(
        http.calls(),
        0,
        "a request that cannot be built must not be sent"
    );
    assert!(
        matches!(
            events.first(),
            Some(AiEvent::DeliveryObserved(
                DeliveryCertainty::DefinitelyUnsent
            ))
        ),
        "{events:?}"
    );
    assert!(
        matches!(events.last(), Some(AiEvent::Stop(StopReason::Error))),
        "{events:?}"
    );
    let named = events.iter().any(
        |event| matches!(event, AiEvent::Error { message, .. } if message.contains("reasoning state")),
    );
    assert!(named, "the error must say what it refused: {events:?}");
}

/// And it still settles. A generation that was refused before it started must
/// release the gate's hold like any other — a wire format's refusal must not
/// be able to strand a budget.
#[test]
fn a_locally_refused_generation_still_settles_and_releases_the_hold() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let mut gate = BudgetGate::new(usd("10"));
    let generator = generator(http as Arc<dyn Http>);

    let req = request_carrying_reasoning();
    let attempt = attempt_for(&mut gate, &req);

    let mut stream = generator.generate(req, attempt);
    let _: Vec<AiEvent> = futures::executor::block_on(stream.by_ref().collect());
    let settled = stream
        .settle(&mut gate)
        .expect("the gate that minted the reservation settles it");

    assert_eq!(settled.outcome.stop(), StopReason::Error);
    assert_eq!(
        settled.outcome.delivery(),
        DeliveryCertainty::DefinitelyUnsent
    );
    assert!(
        settled.outcome.retry_is_safe(),
        "nothing was sent, so retrying cannot double-bill"
    );
    assert!(
        gate.outstanding()
            .expect("a limited gate keeps totals")
            .amount
            .is_zero()
    );
}

// --- what comes back ---

/// A transport failure reaches the caller as the transport read it, not as
/// the binding guessed it.
#[tokio::test]
async fn a_transport_failure_keeps_the_certainty_the_transport_reported() {
    let http = StubHttp::new(Err(HttpError::Transport {
        message: "connection reset by peer".to_string(),
        delivery: DeliveryCertainty::PossiblySent,
    }));
    let mut gate = BudgetGate::new(usd("10"));
    let generator = generator(http as Arc<dyn Http>);

    let attempt = attempt_for(&mut gate, &request());
    let events: Vec<AiEvent> = generator.generate(request(), attempt).collect().await;

    assert!(
        matches!(
            events.first(),
            Some(AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent))
        ),
        "a request that may have been sent must never be reported unsent: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(AiEvent::Stop(StopReason::Error))),
        "{events:?}"
    );
}

/// A real body decodes to exactly what the wire format decodes from it. The
/// binding adds no event and drops none.
#[tokio::test]
async fn a_streamed_response_decodes_to_what_the_wire_format_decodes() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let mut gate = BudgetGate::new(usd("10"));
    let generator = generator(http as Arc<dyn Http>);

    let attempt = attempt_for(&mut gate, &request());
    let through_binding: Vec<AiEvent> = generator.generate(request(), attempt).collect().await;

    let direct: Vec<AiEvent> = ego_ai::wire::openai_completions::decode(
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: Box::pin(futures::stream::iter(
                TEXT_TRANSCRIPT
                    .as_bytes()
                    .chunks(7)
                    .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
                    .collect::<Vec<_>>(),
            )),
        }),
        // `row()` states no cost field, so the binding passes `None` too:
        // "what the wire format decodes from it" has to mean the same
        // configuration on both sides or the comparison proves nothing.
        None,
    )
    .collect()
    .await;

    assert_eq!(through_binding, direct);
    assert!(
        through_binding
            .iter()
            .filter(|event| matches!(event, AiEvent::Stop(_)))
            .count()
            == 1,
        "exactly one terminal frame: {through_binding:?}"
    );
}

// --- the credential ---

/// Story `regression_case`'s fifth criterion, stated as a property of the type
/// rather than of who may call an accessor: a row names the variable its
/// credential lives in and never holds the value.
///
/// The row is asked for its headers *first*, and inspected after. That
/// ordering is the test. It used to be true only that a held [`Secret`]
/// declined to print itself — a redaction one `derive` away from being wrong
/// (`docs/decisions/2026-08-21-a-profile-is-not-a-protocol.md` §3). Resolving
/// a credential now leaves the row exactly as it was, so there is nothing for
/// a future `Serialize` to leak.
#[test]
fn resolving_a_credential_leaves_nothing_behind_in_the_row() {
    let row = row();
    let headers = row
        .headers(&FakeCredentials::holding(KEY))
        .expect("the fake holds every variable");

    assert_eq!(
        headers,
        vec![("authorization".to_string(), format!("Bearer {KEY}"))],
        "only the credential: content type and accept belong to the wire format"
    );
    let printed = format!("{row:?}");
    assert!(
        !printed.contains(KEY),
        "the row kept the credential it was handed: {printed}"
    );
    assert!(
        printed.contains(KEY_VAR),
        "a row still says which variable to set: {printed}"
    );
    assert!(
        printed.contains("stub"),
        "and the rest of the row is still diagnosable: {printed}"
    );
}

/// A provider that authenticates with nothing asks for nothing — and that is
/// not the same answer as a failure.
///
/// [`Auth::None`] is a distinct variant rather than an empty variable name for
/// exactly this reason: collapsing the two would make every local run refuse,
/// or would send `Bearer ` to a daemon that never wanted one.
#[test]
fn a_row_that_authenticates_with_nothing_neither_asks_nor_fails() {
    let local = ProviderRow::ollama_default();
    let creds = FakeCredentials::empty();

    assert_eq!(
        local.headers(&creds).expect("no credential is needed"),
        Vec::new()
    );
    local
        .auth
        .ensure_available(&creds)
        .expect("a strategy that needs nothing is available on a machine with nothing");
}

/// A missing credential is refused by name at *both* doors, rather than
/// leaving one of them to send an empty header and return a 401 from the
/// provider.
#[test]
fn a_credential_that_is_not_configured_is_refused_by_name() {
    let row = row();
    let creds = FakeCredentials::empty();

    let early = row
        .auth
        .ensure_available(&creds)
        .expect_err("this variable is not set");
    let late = row.headers(&creds).expect_err("nor is it set here");

    assert_eq!(early, late, "one refusal, whichever door found it");
    assert!(
        early.to_string().contains(KEY_VAR),
        "the refusal must name the variable to set: {early}"
    );
}

/// And the generator refuses the same way, without a socket.
///
/// `model::resolve` calls `ensure_available` before a generation exists, so
/// this should not happen — but should-not-happen is not cannot, and the
/// honest answer to "the variable was unset mid-run" is a failed generation
/// that was never sent, never a `Bearer ` header for the provider to reject.
#[tokio::test]
async fn a_generation_whose_credential_is_gone_fails_without_sending() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let mut gate = BudgetGate::new(usd("10"));
    let generator = WireGenerator::new(
        row(),
        Arc::clone(&http) as Arc<dyn Http>,
        Arc::new(FakeCredentials::empty()),
    );

    let attempt = attempt_for(&mut gate, &request());
    let events: Vec<AiEvent> = generator.generate(request(), attempt).collect().await;

    assert_eq!(
        http.calls(),
        0,
        "an unauthenticated request must not be sent"
    );
    assert!(
        matches!(
            events.first(),
            Some(AiEvent::DeliveryObserved(
                DeliveryCertainty::DefinitelyUnsent
            ))
        ),
        "{events:?}"
    );
    assert!(
        matches!(events.last(), Some(AiEvent::Stop(StopReason::Error))),
        "{events:?}"
    );
    // `Invalid` and not `Refused`: nothing was asked, so no provider refused
    // anything — and this is the classification that exits the way a missing
    // credential already exits at resolution.
    let named = events.iter().any(|event| {
        matches!(event, AiEvent::Error { message, kind }
            if message.contains(KEY_VAR) && *kind == ErrorKind::Invalid)
    });
    assert!(
        named,
        "the error must name the variable and classify itself: {events:?}"
    );
}

// --- the seam ---

/// A second `WireFormat`, so that "the generator uses the row's mapping" is a
/// claim a test can fail.
///
/// It exists only here. A second *production* mapping arrives with the
/// provider that needs it — Anthropic's Messages API — and inventing one early
/// would be the stub `AGENTS.md` forbids. But the property this proves is not
/// Anthropic's: it is that `WireGenerator` reads `row.wire` rather than naming
/// a mapping, and a fake is the only way to state that before a second real
/// one exists. Hardcoding `OpenAiCompletions` back into the generator leaves
/// every other test in this file green and fails exactly these two.
#[derive(Debug)]
struct MarkerFormat;

/// What `MarkerFormat` writes and says, chosen so neither can be produced by
/// `openai-completions` reading the same inputs.
const MARKER_PATH: &str = "https://api.example/marker";
const MARKER_TEXT: &str = "decoded by the marker format";

impl ego_ai::wire::WireFormat for MarkerFormat {
    fn build_request(
        &self,
        _endpoint: Url,
        headers: Vec<(String, String)>,
        _req: &GenRequest,
    ) -> Result<HttpRequest, ego_ai::wire::Unexpressible> {
        Ok(HttpRequest {
            url: Url::parse(MARKER_PATH).expect("a constant this file controls"),
            method: Method::Post,
            headers,
            body: None,
        })
    }

    fn decode(
        &self,
        _response: Result<HttpResponse, HttpError>,
        _cost: Option<Arc<dyn ego_ai::wire::provider_cost::ProviderCost>>,
    ) -> futures::stream::BoxStream<'static, AiEvent> {
        futures::stream::iter([
            AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent),
            AiEvent::TextDelta {
                text: MARKER_TEXT.to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ])
        .boxed()
    }
}

fn marker_row() -> ProviderRow {
    ProviderRow {
        wire: &MarkerFormat,
        ..row()
    }
}

#[tokio::test]
async fn the_request_sent_is_the_one_the_rows_mapping_built() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = WireGenerator::new(
        marker_row(),
        Arc::clone(&http) as Arc<dyn Http>,
        Arc::new(FakeCredentials::holding(KEY)),
    );
    let mut gate = BudgetGate::new(usd("10"));
    let request = request();
    let attempt = attempt_for(&mut gate, &request);

    let _ = generator
        .generate(request, attempt)
        .collect::<Vec<_>>()
        .await;

    let seen = http.seen.lock().expect("not poisoned");
    let sent = seen.first().expect("one request was sent");
    assert_eq!(
        sent.url.as_str(),
        MARKER_PATH,
        "the generator built the request with a mapping the row did not name"
    );
    // The row's endpoint is still the openai one, so this also proves the
    // mapping — not the row's address — decided where the bytes went.
    assert!(sent.body.is_none(), "{sent:?}");
}

#[tokio::test]
async fn the_events_returned_are_the_ones_the_rows_mapping_decoded() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = WireGenerator::new(
        marker_row(),
        http as Arc<dyn Http>,
        Arc::new(FakeCredentials::holding(KEY)),
    );
    let mut gate = BudgetGate::new(usd("10"));
    let request = request();
    let attempt = attempt_for(&mut gate, &request);

    let events: Vec<AiEvent> = generator
        .generate(request, attempt)
        .collect::<Vec<_>>()
        .await;

    let decoded = events
        .iter()
        .any(|event| matches!(event, AiEvent::TextDelta { text } if text == MARKER_TEXT));
    assert!(
        decoded,
        "the transcript is openai-completions and would decode to its own \
         text; only the row's mapping produces this: {events:?}"
    );
}
