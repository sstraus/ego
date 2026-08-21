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
use ego_ai::wire::openai_completions::request_body;
use ego_core::budget::{BudgetGate, PricingQuote};
use ego_core::cost::{Amount, CurrencyCode, Money, PricingRevision};
use ego_core::event::id::GenerationId;
use ego_core::generator::attempt::{DeliveryCertainty, GenerationAttempt};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, Transport,
};
use ego_core::generator::port::{AiEvent, Generator};
use ego_core::generator::types::{
    Conversation, GenRequest, Message, ReasoningState, Role, StopReason, ThinkingLevel,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use url::Url;

const TEXT_TRANSCRIPT: &str = include_str!("goldens/stream_text.sse");

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
        model: "gpt-5.1".to_string(),
        messages: Conversation::new(vec![Message::text(Role::User, "list the files")])
            .expect("one user message is a conversation"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(1_024),
        parallel_tool_calls: false,
        reasoning_state: None,
    }
}

fn row() -> ProviderRow {
    ProviderRow {
        id: "stub".to_string(),
        endpoint: Url::parse("https://api.example/v1/chat/completions").expect("a valid url"),
        auth: Auth::Bearer("sk-not-a-real-key".into()),
    }
}

/// Mints a real attempt through the only door there is.
///
/// `Reservation`'s constructor is sealed in `ego_core::budget`, so this has to
/// go through `BudgetGate::reserve` — the same fact `tests/skeleton.rs` leans
/// on, and the reason a generator cannot route around the gate.
fn attempt_for(gate: &mut BudgetGate, req: &GenRequest) -> GenerationAttempt {
    let reservation = gate
        .reserve(&quote(), req, &caps())
        .expect("a $10 budget admits an ordinary request");
    GenerationAttempt::new(GenerationId::new(), reservation, &quote(), req, &caps())
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
    let generator = WireGenerator::new(row(), Arc::clone(&http) as Arc<dyn Http>);

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
            .any(|(name, value)| name == "authorization" && value == "Bearer sk-not-a-real-key"),
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
    let generator = WireGenerator::new(row(), http as Arc<dyn Http>);

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
    let generator = WireGenerator::new(row(), Arc::clone(&http) as Arc<dyn Http>);

    let mut req = request();
    req.reasoning_state = Some(ReasoningState("opaque".to_string()));
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
        |event| matches!(event, AiEvent::Error { message } if message.contains("reasoning state")),
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
    let generator = WireGenerator::new(row(), http as Arc<dyn Http>);

    let mut req = request();
    req.reasoning_state = Some(ReasoningState("opaque".to_string()));
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
    assert!(gate.outstanding().amount.is_zero());
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
    let generator = WireGenerator::new(row(), http as Arc<dyn Http>);

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
    let generator = WireGenerator::new(row(), http as Arc<dyn Http>);

    let attempt = attempt_for(&mut gate, &request());
    let through_binding: Vec<AiEvent> = generator.generate(request(), attempt).collect().await;

    let direct: Vec<AiEvent> = ego_ai::wire::openai_completions::decode(Ok(HttpResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
        body: Box::pin(futures::stream::iter(
            TEXT_TRANSCRIPT
                .as_bytes()
                .chunks(7)
                .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        )),
    }))
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

/// A credential must not be printable. `ProviderRow` is `Debug` — a row is
/// configuration and worth logging — so the redaction has to live on the
/// secret itself, where it holds everywhere the value goes rather than only
/// on the one type someone remembered.
#[test]
fn a_credential_does_not_print_itself() {
    let printed = format!("{:?}", row());
    assert!(
        !printed.contains("sk-not-a-real-key"),
        "the credential is in a Debug string: {printed}"
    );
    assert!(
        printed.contains("stub"),
        "and the rest of the row is still diagnosable: {printed}"
    );
}

/// A missing credential is refused where it can be diagnosed, rather than
/// sent as an empty header and returned as a 401 from the provider.
#[test]
fn a_credential_that_is_not_configured_is_refused_by_name() {
    let missing = Auth::bearer_from_env("EGO_TEST_CREDENTIAL_THAT_IS_NOT_SET")
        .expect_err("this variable is not set");
    assert!(
        missing
            .to_string()
            .contains("EGO_TEST_CREDENTIAL_THAT_IS_NOT_SET"),
        "the refusal must name the variable to set: {missing}"
    );
}
