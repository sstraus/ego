//! The binding: a `Generator` that actually reaches a provider.
//!
//! See `plans/m0b-vertical-slice.md` Step 13.5 and
//! `ARCHITECTURE.md`.
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
use ego_ai::http::{
    Http, HttpCall, HttpError, HttpRequest, HttpResponse, Method, PreparedHttpCall,
};
use ego_ai::provider::{CredentialStrategy, ProviderRow, ResolvedCredential};
use ego_ai::wire::github_copilot_completions::GithubCopilotCompletions;
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_ai::wire::openai_completions::request_body;
use ego_ai::wire::{RequestTarget, WireFormat};
use ego_core::budget::{BudgetGate, Metering, PricingQuote, SettlementSource};
use ego_core::cost::{Amount, CurrencyCode, Money, PricingRevision};
use ego_core::event::id::GenerationId;
use ego_core::generator::attempt::{DeliveryCertainty, GenerationAttempt};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::port::{AiEvent, Generator};
use ego_core::generator::types::{
    ContentBlock, Conversation, GenRequest, Message, ReasoningState, Role, StopReason,
    ThinkingLevel,
};
use ego_core::permit::{HttpHeader, HttpScheme, OneHopResponse, TransportError, TransportFailure};
use futures::StreamExt;
use futures::future::BoxFuture;
use url::Url;

mod support;
use support::FakeCredentials;

const TEXT_TRANSCRIPT: &str = include_str!("goldens/chat_completions/stream/text.sse");

/// The variable the fixture row names, and the value a configured machine
/// would have put in it. Two constants rather than two literals: the tests
/// below assert that the first appears where the second must not.
const KEY_VAR: &str = "OPENAI_API_KEY";
const KEY: &str = "sk-not-a-real-key";
const COPILOT_TOKEN: &str = "tid=1;proxy-ep=proxy.enterprise.example;exp=1800003600";

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
        schema_version: ModelCapabilities::SCHEMA_VERSION,
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
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

fn request() -> GenRequest {
    GenRequest {
        cache: None,
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
        cache: None,
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
    ProviderRow::openai_at(Url::parse("https://api.example/v1/").expect("a valid url"))
}

/// The fixture row, over `http`, on a machine where the key is configured.
///
/// Every test below the credential section wants that machine — the
/// credential is not what they are about — so the four-argument constructor
/// is spelled once here rather than at each call site.
fn generator(http: Arc<dyn Http>) -> WireGenerator {
    WireGenerator::new(
        row(),
        &OpenAiCompletions,
        http,
        credential(&FakeCredentials::holding(KEY)),
    )
}

/// The fixture row's credential, resolved.
///
/// A generator cannot be built without one, which is the type-level half of
/// `ARCHITECTURE.md` §6:
/// there is no longer a state in which a generation exists and its credential
/// does not.
fn credential(creds: &dyn ego_ai::provider::Credentials) -> ResolvedCredential {
    row()
        .credential
        .resolve(creds)
        .expect("the fixture row's variable is set on this machine")
}

fn row_with_credential_base_override() -> ProviderRow {
    ProviderRow::github_copilot_default()
}

fn credential_with_base_override(row: &ProviderRow) -> ResolvedCredential {
    row.credential
        .resolve(&FakeCredentials::holding(COPILOT_TOKEN))
        .expect("the copilot fixture has a proxy endpoint")
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
    answer: Arc<Mutex<Option<Result<OneHopResponse, TransportError>>>>,
    seen: Arc<Mutex<Vec<HttpRequest>>>,
}

impl StubHttp {
    fn new(answer: Result<OneHopResponse, TransportError>) -> Arc<Self> {
        Arc::new(StubHttp {
            answer: Arc::new(Mutex::new(Some(answer))),
            seen: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn responding(status: u16, body: &str) -> Arc<Self> {
        let chunks: Vec<Result<Vec<u8>, TransportError>> = body
            .as_bytes()
            .chunks(7)
            .map(|chunk| Ok(chunk.to_vec()))
            .collect();
        StubHttp::new(Ok(OneHopResponse::streaming_response(
            status,
            vec![
                HttpHeader::new("content-type", b"text/event-stream".to_vec())
                    .expect("fixture header"),
            ],
            Box::pin(futures::stream::iter(chunks)),
        )))
    }

    fn calls(&self) -> usize {
        self.seen.lock().expect("not poisoned").len()
    }
}

impl Http for StubHttp {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(StubCall {
            answer: Arc::clone(&self.answer),
            seen: Arc::clone(&self.seen),
        })
    }
}

struct StubCall {
    answer: Arc<Mutex<Option<Result<OneHopResponse, TransportError>>>>,
    seen: Arc<Mutex<Vec<HttpRequest>>>,
}

impl HttpCall for StubCall {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let (_, route, _, request) = call.into_parts();
        let scheme = match route.scheme() {
            HttpScheme::Http => "http",
            HttpScheme::Https => "https",
        };
        let headers = request.headers().to_vec();
        let url = Url::parse(&format!("{scheme}://{}{}", route.target(), request.path()))
            .expect("prepared request URL");
        let method = match request.method() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            other => panic!("unsupported fixture method {other}"),
        };
        self.seen.lock().expect("not poisoned").push(HttpRequest {
            url,
            method,
            headers,
            body: (!request.body().is_empty()).then(|| request.body().to_vec()),
        });
        let answer = self
            .answer
            .lock()
            .expect("not poisoned")
            .take()
            .unwrap_or_else(|| {
                Err(TransportError::definitely_unsent(
                    TransportFailure::Exchange("the stub was asked twice".to_owned()),
                ))
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
    assert_eq!(
        sent.url,
        row()
            .base()
            .join(ego_ai::wire::openai_completions::PATH)
            .expect("a path")
    );
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
            .any(|header| header.name() == "authorization"
                && header.value() == format!("Bearer {KEY}").as_bytes()),
        "the provider row's credential reaches the request: {:?}",
        sent.headers
            .iter()
            .map(|header| header.name())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn ensure_expressible_and_generate_share_headers_base_and_wire_body() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let profile = row_with_credential_base_override();
    let resolved = credential_with_base_override(&profile);
    let req = request();
    let mut headers = resolved.headers();
    for (name, value) in &profile.extra_headers {
        headers.retain(|existing| !existing.name().eq_ignore_ascii_case(name));
        headers
            .push(HttpHeader::new(name, value.as_bytes().to_vec()).expect("fixture extra header"));
    }
    let target = RequestTarget::new(resolved.base_override().unwrap_or(profile.base()), &headers);
    let expected = GithubCopilotCompletions
        .build_request(&target, &req)
        .expect("the fixture request is expressible");
    let generator = WireGenerator::new(
        profile,
        &GithubCopilotCompletions,
        Arc::clone(&http) as Arc<dyn Http>,
        resolved,
    );

    assert!(
        generator.ensure_expressible(&req).is_ok(),
        "the fixed request envelope must be locally expressible"
    );
    assert_eq!(http.calls(), 0, "expressibility is checked without HTTP");

    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &req);
    let _: Vec<AiEvent> = generator.generate(req, attempt).collect().await;

    let seen = http.seen.lock().expect("not poisoned");
    let sent = seen.first().expect("generation sent one request");
    assert_eq!(
        sent.url, expected.url,
        "credential base override was preserved"
    );
    assert_eq!(sent.method, expected.method);
    assert_eq!(sent.headers, expected.headers);
    assert_eq!(sent.body, expected.body, "the wire body was preserved");
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
/// A definitely-unsent transport error means: no DNS, no socket, no bytes.
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
    let http = StubHttp::new(Err(TransportError::possibly_sent(
        TransportFailure::Connect("connection reset by peer".to_owned()),
    )));
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
            headers: vec![
                HttpHeader::new("content-type", b"text/event-stream".to_vec())
                    .expect("fixture header"),
            ],
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
/// (`ARCHITECTURE.md` §3). Resolving
/// a credential now leaves the row exactly as it was, so there is nothing for
/// a future `Serialize` to leak.
#[test]
fn resolving_a_credential_leaves_nothing_behind_in_the_row() {
    let row = row();
    let headers = row
        .credential
        .resolve(&FakeCredentials::holding(KEY))
        .expect("the fake holds every variable")
        .headers();

    assert_eq!(headers.len(), 1, "only the credential header remains");
    assert_eq!(headers[0].name(), "authorization");
    assert_eq!(headers[0].value(), format!("Bearer {KEY}").as_bytes());
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
        printed.contains(row.id()),
        "and the rest of the row is still diagnosable: {printed}"
    );
}

/// A provider that authenticates with nothing asks for nothing — and that is
/// not the same answer as a failure.
///
/// [`CredentialStrategy::None`] is a distinct variant rather than an empty
/// variable name for exactly this reason: collapsing the two would make every
/// local run refuse, or would send `Bearer ` to a daemon that never wanted
/// one.
#[test]
fn a_row_that_authenticates_with_nothing_neither_asks_nor_fails() {
    let local = ProviderRow::ollama_default();
    let creds = FakeCredentials::empty();

    assert_eq!(
        local
            .credential
            .resolve(&creds)
            .expect("a strategy that needs nothing resolves on a machine with nothing")
            .headers(),
        Vec::new()
    );
}

/// A provider that does not spell its credential `Authorization: Bearer`.
///
/// Anthropic reads `x-api-key` and Gemini reads `x-goog-api-key`, and neither
/// takes a scheme prefix — so the value travels **verbatim**, which is the
/// whole difference between this strategy and
/// [`CredentialStrategy::BearerFromEnv`]. Both halves are asserted here
/// because getting either wrong produces the same symptom, a 401, and the
/// operator cannot tell from it which mistake was made.
///
/// The variant exists now and not before because `AGENTS.md` forbids one with
/// no producer; its two producers are the mappings this story adds.
#[test]
fn a_credential_can_travel_under_a_header_the_provider_names() {
    let strategy = CredentialStrategy::HeaderFromEnv {
        header: "x-api-key".to_string(),
        var: KEY_VAR.to_string(),
    };

    let headers = strategy
        .resolve(&FakeCredentials::holding(KEY))
        .expect("the fake holds every variable")
        .headers();
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].name(), "x-api-key");
    assert_eq!(headers[0].value(), KEY.as_bytes());
    assert!(headers[0].is_secret());
    assert!(
        strategy
            .resolve(&FakeCredentials::empty())
            .expect_err("this variable is not set")
            .to_string()
            .contains(KEY_VAR),
        "and a missing one is refused by name, like every other"
    );
}

/// A missing credential is refused by name, at the one door there is.
///
/// There used to be two — a check at resolution and a second read when a
/// request was assembled — and the test that lives here used to assert they
/// agreed. §6 removed the second: the value is read once and carried, so the
/// two cannot disagree because there is no longer a second one to disagree
/// with. What must survive is the message, which is the whole reason the
/// check is early: an operator can act on "set `OPENAI_API_KEY`" and cannot
/// act on a 401.
#[test]
fn a_credential_that_is_not_configured_is_refused_by_name() {
    let refused = row()
        .credential
        .resolve(&FakeCredentials::empty())
        .expect_err("this variable is not set");

    assert!(
        refused.to_string().contains(KEY_VAR),
        "the refusal must name the variable to set: {refused}"
    );
}

/// The credential the row resolved is the credential the request carries,
/// exactly once and under the header its strategy names.
///
/// This replaces a test that drove a generator built with an *empty*
/// `Credentials` and asserted it failed without sending. That state no longer
/// exists: `WireGenerator::new` takes a `ResolvedCredential`, so a generation
/// whose credential is missing cannot be constructed, and a test asserting it
/// fails gracefully would be asserting something about code that cannot run.
/// What remains testable — and is the property that mattered — is that the
/// header actually goes out with the request rather than being resolved and
/// dropped.
#[tokio::test]
async fn the_resolved_credential_is_what_the_request_carries() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let mut gate = BudgetGate::new(usd("10"));
    let generator = generator(Arc::clone(&http) as Arc<dyn Http>);

    let attempt = attempt_for(&mut gate, &request());
    let _: Vec<AiEvent> = generator.generate(request(), attempt).collect().await;

    let seen = http.seen.lock().expect("not poisoned");
    let sent = seen.first().expect("the transport was called once");
    let auth: Vec<_> = sent
        .headers
        .iter()
        .filter(|header| header.name() == "authorization")
        .collect();
    assert_eq!(auth.len(), 1, "one authorization header");
    assert_eq!(auth[0].value(), format!("Bearer {KEY}").as_bytes());
}

/// Profile identity is data beside the credential, and survives unchanged to
/// the request. The mapping remains unaware of what either header means.
#[tokio::test]
async fn profile_extra_headers_reach_the_request_without_becoming_credentials() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let mut gate = BudgetGate::new(usd("10"));
    let mut profile = row();
    profile.extra_headers = vec![
        ("originator".to_string(), "ego".to_string()),
        ("user-agent".to_string(), "ego-test".to_string()),
    ];
    let generator = WireGenerator::new(
        profile,
        &OpenAiCompletions,
        Arc::clone(&http) as Arc<dyn Http>,
        credential(&FakeCredentials::holding(KEY)),
    );

    let attempt = attempt_for(&mut gate, &request());
    let _: Vec<AiEvent> = generator.generate(request(), attempt).collect().await;

    let seen = http.seen.lock().expect("not poisoned");
    let sent = seen.first().expect("the transport was called once");
    assert!(
        sent.headers
            .iter()
            .any(|header| { header.name() == "originator" && header.value() == b"ego" })
    );
    assert!(
        sent.headers
            .iter()
            .any(|header| { header.name() == "user-agent" && header.value() == b"ego-test" })
    );
}

// --- the seam ---

/// A second `WireFormat`, so that "the generator uses the row's mapping" is a
/// claim a test can fail.
///
/// It exists only here. A second *production* mapping arrives with the
/// provider that needs it — Anthropic's Messages API — and inventing one early
/// would be the stub `AGENTS.md` forbids. But the property this proves is not
/// Anthropic's: it is that `WireGenerator` uses the mapping supplied by the
/// resolved model rather than naming one internally, and a fake is the only way to state that before a second real
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
        target: &ego_ai::wire::RequestTarget<'_>,
        _req: &GenRequest,
    ) -> Result<HttpRequest, ego_ai::wire::RequestRefused> {
        Ok(HttpRequest {
            url: Url::parse(MARKER_PATH).expect("a constant this file controls"),
            method: Method::Post,
            headers: target.headers().to_vec(),
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

#[tokio::test]
async fn the_request_sent_is_the_one_the_rows_mapping_built() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = WireGenerator::new(
        row(),
        &MarkerFormat,
        Arc::clone(&http) as Arc<dyn Http>,
        credential(&FakeCredentials::holding(KEY)),
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
        row(),
        &MarkerFormat,
        http as Arc<dyn Http>,
        credential(&FakeCredentials::holding(KEY)),
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

// Catches invalid MCP punctuation leaking into provider function declarations.
#[tokio::test]
async fn provider_tool_names_reject_no_catalogue_identity() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = generator(Arc::clone(&http) as Arc<dyn Http>);
    let mut req = request();
    for name in [
        "mcp.tuic.agent",
        "history.search",
        "history.open",
        "mcp_tuic_agent",
        "工具",
        "",
        "1starts_with_digit",
        &"x".repeat(200),
    ] {
        req.tools.push(ego_core::generator::types::ToolDescriptor {
            name: name.into(),
            description: String::new(),
            parameters: serde_json::json!({"type":"object"}),
        });
    }
    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &req);
    let _: Vec<_> = generator.generate(req, attempt).collect().await;
    let seen = http.seen.lock().unwrap();
    let body: serde_json::Value = serde_json::from_slice(seen[0].body.as_ref().unwrap()).unwrap();
    let names: Vec<_> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    for name in &names {
        assert!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "invalid provider name: {name}"
        );
    }
    assert_eq!(
        names
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        names.len()
    );
}

// Catches a provider alias reaching the catalogue instead of its original tool.
#[tokio::test]
async fn provider_tool_names_restore_streamed_dispatch_without_alias_collisions() {
    const ALIAS: &str = "__ego_7a8fc438b5e79141b36e6c2f6086a9d323b3ed6ab883dc0186b5f8347d";
    let transcript =
        include_str!("goldens/chat_completions/stream/tool_calls.sse").replace("read_file", ALIAS);
    let http = StubHttp::responding(200, &transcript);
    let generator = generator(Arc::clone(&http) as Arc<dyn Http>);
    let mut req = request();
    for name in ["mcp.tuic.agent", ALIAS, "glob"] {
        req.tools.push(ego_core::generator::types::ToolDescriptor {
            name: name.into(),
            description: String::new(),
            parameters: serde_json::json!({"type":"object"}),
        });
    }
    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &req);
    let events: Vec<_> = generator.generate(req, attempt).collect().await;
    assert!(events.iter().any(
        |event| matches!(event, AiEvent::ToolCall { call_id, name, args }
        if call_id == "call_a" && name == "mcp.tuic.agent" && args == r#"{"path":"src/lib.rs"}"#)
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::ToolCall { name, .. } if name == "glob"))
    );
    let seen = http.seen.lock().unwrap();
    let body: serde_json::Value = serde_json::from_slice(seen[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(body["tools"][0]["function"]["name"], ALIAS);
    assert_ne!(body["tools"][1]["function"]["name"], ALIAS);
}

// Catches invalid names in resumed history after their tools leave the catalogue.
#[tokio::test]
async fn provider_tool_names_encode_retired_history_and_keep_valid_names() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = generator(Arc::clone(&http) as Arc<dyn Http>);
    let mut req = request();
    req.tools.push(ego_core::generator::types::ToolDescriptor {
        name: "read_file".into(),
        description: String::new(),
        parameters: serde_json::json!({"type":"object"}),
    });
    req.messages = Conversation::new(vec![
        Message::text(Role::User, "list the files"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                call_id: "old".into(),
                name: "mcp.tuic.agent".into(),
                args: "{}".into(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                call_id: "old".into(),
                output: "done".into(),
                is_error: false,
            }],
        },
        Message::text(Role::User, "continue"),
    ])
    .unwrap();
    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &req);
    let _: Vec<_> = generator.generate(req, attempt).collect().await;
    let seen = http.seen.lock().unwrap();
    let body: serde_json::Value = serde_json::from_slice(seen[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["function"]["name"],
        "__ego_7a8fc438b5e79141b36e6c2f6086a9d323b3ed6ab883dc0186b5f8347d"
    );
}

#[tokio::test]
async fn unknown_provider_names_cannot_bypass_aliases_and_select_catalogue_tools() {
    // A provider can repeat a familiar original name instead of the declaration
    // alias. That must not become a callable catalogue identity by pass-through.
    for original in ["history.search", "__ego_custom"] {
        let transcript = include_str!("goldens/chat_completions/stream/tool_calls.sse")
            .replace("read_file", original);
        let http = StubHttp::responding(200, &transcript);
        let mut req = request();
        req.tools = vec![ego_core::generator::types::ToolDescriptor {
            name: original.to_owned(),
            description: "Read saved history".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let mut gate = BudgetGate::new(usd("10"));
        let attempt = attempt_for(&mut gate, &req);
        let events: Vec<_> = generator(http).generate(req, attempt).collect().await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::ToolCall { name, .. } if name == original)),
            "unknown provider name bypassed its alias and selected catalogue tool {original}: {events:?}"
        );
    }
}
