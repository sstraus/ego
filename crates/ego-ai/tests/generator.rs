//! The binding: a `Generator` that actually reaches a provider.
//!
//! See `plans/m0b-vertical-slice.md` Step 13.5 and
//! `ARCHITECTURE.md`.
//!
//! Steps 8, 9 and 10 shipped the `Http` seam, the openai-completions wire
//! format and the turn loop, each green in isolation and none of them joined
//! to the others. This file is about the join, so it tests exactly the join:
//! that the transport receives the wire payload with the selected service tier,
//! that the events the wire format would decode are the events the
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
    ContentBlock, Conversation, ErrorKind, GenRequest, Message, ReasoningState, Role, StopReason,
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
        web_search: None,
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
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
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
        web_search: None,
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
        web_search: None,
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

/// Catches the binding dropping wire fields while adding the explicit Standard tier.
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
    let mut expected: serde_json::Value =
        serde_json::from_slice(&request_body(&request()).expect("this request is expressible"))
            .expect("the wire format builds JSON");
    expected["service_tier"] = "default".into();
    assert_eq!(
        sent.body.as_deref(),
        Some(
            serde_json::to_vec(&expected)
                .expect("valid JSON")
                .as_slice()
        ),
        "the binding preserves every wire field and explicitly selects Standard"
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
            body: Some(br#"{"marker":"request built by the marker format"}"#.to_vec()),
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
    let body: serde_json::Value =
        serde_json::from_slice(sent.body.as_deref().expect("the marker body is preserved"))
            .expect("the request body is JSON");
    assert_eq!(
        body,
        serde_json::json!({
            "marker": "request built by the marker format",
            "service_tier": "default",
        }),
        "the binding preserves the marker payload and explicitly selects Standard"
    );
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
    for original in ["history.search", "__ego_custom", "undeclared"] {
        let transcript = include_str!("goldens/chat_completions/stream/tool_calls.sse")
            .replace("read_file", original);
        let http = StubHttp::responding(200, &transcript);
        let mut req = request();
        req.tools = vec![ego_core::generator::types::ToolDescriptor {
            name: if original == "undeclared" {
                "read_file"
            } else {
                original
            }
            .to_owned(),
            description: "Read saved history".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let mut gate = BudgetGate::new(usd("10"));
        let attempt = attempt_for(&mut gate, &req);
        let events: Vec<_> = generator(http).generate(req, attempt).collect().await;
        assert!(events.iter().any(|event| matches!(event,
            AiEvent::Error { message, kind: ErrorKind::Unclassified }
                if message.contains(original))));
        assert_eq!(
            events.iter().find_map(|event| match event {
                AiEvent::Stop(reason) => Some(*reason),
                _ => None,
            }),
            Some(StopReason::Error)
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::ToolCall { name, .. } if name == original)),
            "unknown provider name bypassed its alias and selected catalogue tool {original}: {events:?}"
        );
    }
}

// Catches: diagnostic request projection leaks prompt/schema/credentials instead of only tool names.
#[test]
fn request_diagnostics_include_wire_tool_names_without_private_request_data() {
    use tracing::field::{Field, Visit};

    struct Recorder(Arc<Mutex<Vec<String>>>);
    impl Visit for Recorder {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "request_tool_names" {
                self.0.lock().unwrap().push(format!("{value:?}"));
            }
        }
    }
    struct Collector(Arc<Mutex<Vec<String>>>);
    impl tracing::Subscriber for Collector {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::Id {
            tracing::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::Id, _: &tracing::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            event.record(&mut Recorder(Arc::clone(&self.0)));
        }
        fn enter(&self, _: &tracing::Id) {}
        fn exit(&self, _: &tracing::Id) {}
    }
    let captured = Arc::new(Mutex::new(Vec::new()));
    let _registered = tracing::Dispatch::new(Collector(Arc::clone(&captured)));
    let _guard = tracing::subscriber::set_default(Collector(Arc::clone(&captured)));
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = generator(http);
    let mut req = request();
    req.tools = vec![ego_core::generator::types::ToolDescriptor {
        name: "read_file".into(),
        description: "private tool description".into(),
        parameters: serde_json::json!({"type":"object", "description":"private schema"}),
    }];
    generator.ensure_expressible(&req).unwrap();
    assert_eq!(*captured.lock().unwrap(), ["[Some(String(\"read_file\"))]"]);
}

#[tokio::test]
async fn fast_contract_catches_provider_field_drift_and_silent_unsupported_tiers() {
    use ego_ai::service_tier::ServiceTier;
    for (row, wire, model, field, beta) in [
        (
            ProviderRow::openai(),
            &ego_ai::wire::openai_responses::OpenAiResponses as &'static dyn WireFormat,
            "gpt-6.1-sol",
            "service_tier",
            false,
        ),
        (
            ProviderRow::anthropic_default(),
            &ego_ai::wire::anthropic_messages::AnthropicMessages as &'static dyn WireFormat,
            "claude-opus-5-5",
            "speed",
            false,
        ),
        (
            ProviderRow::anthropic_default(),
            &ego_ai::wire::anthropic_messages::AnthropicMessages as &'static dyn WireFormat,
            "claude-opus-4-8",
            "speed",
            true,
        ),
        (
            ProviderRow::openrouter_default(),
            &ego_ai::wire::openai_completions::OpenAiCompletions as &'static dyn WireFormat,
            "openai/gpt-6.1-sol",
            "service_tier",
            false,
        ),
    ] {
        for tier in [ServiceTier::Standard, ServiceTier::Fast] {
            let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
            let credential = row
                .credential
                .resolve(&FakeCredentials::holding(KEY))
                .unwrap();
            let generator = WireGenerator::new(row.clone(), wire, http.clone(), credential)
                .with_service_tier(tier);
            let mut req = request();
            req.model = model.into();
            let mut gate = BudgetGate::new(usd("10"));
            let attempt = attempt_for(&mut gate, &req);
            let _: Vec<_> = generator.generate(req, attempt).collect().await;
            let seen = http.seen.lock().unwrap();
            let sent = seen.first().unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(sent.body.as_deref().unwrap()).unwrap();
            if tier == ServiceTier::Fast {
                assert_eq!(
                    body[field],
                    if row.id() == "openrouter" {
                        "priority"
                    } else {
                        "fast"
                    }
                );
                if beta {
                    assert!(sent.headers.iter().any(|header| {
                        header.name() == "anthropic-beta"
                            && String::from_utf8_lossy(header.value())
                                .contains("fast-mode-2026-02-01")
                    }));
                }
            } else {
                assert_ne!(body[field], "fast");
                assert_ne!(body[field], "priority");
            }
        }
    }
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = generator(http.clone()).with_service_tier(ServiceTier::Fast);
    let mut req = request();
    req.model = "unknown".into();
    assert!(
        generator
            .ensure_expressible(&req)
            .unwrap_err()
            .to_string()
            .contains("no verified fast tier")
    );
    assert_eq!(http.calls(), 0);
}

#[tokio::test]
async fn standard_tier_catches_fast_project_default_surviving_fast_off() {
    // Captured OpenAI fast-mode documentation says omitted service_tier inherits
    // the project's tier. Standard must override a Fast project explicitly, or
    // /fast off can still incur Fast charges while ego quotes Standard prices.
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = WireGenerator::new(
        row(),
        &ego_ai::wire::openai_responses::OpenAiResponses,
        http.clone(),
        credential(&FakeCredentials::holding(KEY)),
    )
    .with_service_tier(ego_ai::service_tier::ServiceTier::Standard);
    let mut req = request();
    req.model = "gpt-6.1-sol".into();
    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &req);
    let _: Vec<_> = generator.generate(req, attempt).collect().await;
    let seen = http.seen.lock().unwrap();
    let sent = seen.first().expect("standard generation reaches transport");
    let body: serde_json::Value = serde_json::from_slice(sent.body.as_deref().unwrap()).unwrap();
    assert_eq!(
        body["service_tier"], "default",
        "Standard must override OpenAI project Fast defaults, not inherit them"
    );
}

// Catches: a fork with the same cache root/history inherits its parent's response ID,
// or compaction/unidentified/resumed requests reuse connection-local continuation.
#[tokio::test]
async fn codex_fork_and_compaction_never_send_another_lineages_response_id() {
    use ego_ai::http::HttpWebSocket;
    use ego_ai::wire::openai_codex_responses::OpenAiCodexResponses;
    use ego_core::event::id::SessionId;
    use ego_core::generator::types::PromptCache;
    use serde_json::{Value, json};

    #[derive(Default)]
    struct RecordingHttp {
        sent: Arc<Mutex<Vec<Value>>>,
        opened: std::sync::atomic::AtomicUsize,
    }
    struct Socket {
        sent: Arc<Mutex<Vec<Value>>>,
        response: Option<Vec<u8>>,
    }
    impl Http for RecordingHttp {
        fn begin_call(&self) -> Box<dyn HttpCall> {
            panic!("lineage guard must exercise WebSocket, not HTTP fallback")
        }
        fn open_websocket(
            &self,
            _: PreparedHttpCall,
        ) -> BoxFuture<'static, Result<Box<dyn HttpWebSocket>, TransportError>> {
            self.opened
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let socket = Socket {
                sent: self.sent.clone(),
                response: None,
            };
            Box::pin(async move { Ok(Box::new(socket) as Box<dyn HttpWebSocket>) })
        }
    }
    impl HttpWebSocket for Socket {
        fn send(&mut self, call: PreparedHttpCall) -> BoxFuture<'_, Result<(), TransportError>> {
            let (_, _, _, request) = call.into_parts();
            let mut sent = self.sent.lock().unwrap();
            sent.push(serde_json::from_slice(request.body()).unwrap());
            // Scripted component response, not evidence of a live backend contract.
            self.response = Some(serde_json::to_vec(&json!({
                "type":"response.completed",
                "response":{"id":format!("resp_{}", sent.len()), "status":"completed", "output":[]}
            })).unwrap());
            Box::pin(async { Ok(()) })
        }
        fn receive(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
            let response = self.response.take();
            Box::pin(async move { Ok(response) })
        }
    }

    for boundary in ["fork", "compaction", "unidentified", "resume"] {
        let http = Arc::new(RecordingHttp::default());
        let make_generator = || {
            WireGenerator::new(
                ProviderRow::openai_codex_default(),
                &OpenAiCodexResponses,
                http.clone(),
                support::credential(),
            )
        };
        let mut generator = make_generator();
        let parent = SessionId::new();
        let mut req = request();
        req.cache = Some(PromptCache {
            session_id: parent,
            lineage_root: parent,
            retention: CacheRetention::Short,
            stable_messages: 0,
        });
        let mut gate = BudgetGate::new(usd("10"));
        let attempt = attempt_for(&mut gate, &req);
        let first: Vec<_> = generator.generate(req.clone(), attempt).collect().await;
        assert!(matches!(
            first.last(),
            Some(AiEvent::Stop(StopReason::Stop))
        ));

        // The completed response has empty output, so all prior input is the
        // continuation baseline. Prove continuation actually works before invalidation.
        let mut messages = req.messages.messages().to_vec();
        messages.push(Message::text(Role::User, "continue"));
        req.messages = Conversation::new(messages).unwrap();
        let attempt = attempt_for(&mut gate, &req);
        let second: Vec<_> = generator.generate(req.clone(), attempt).collect().await;
        assert!(matches!(
            second.last(),
            Some(AiEvent::Stop(StopReason::Stop))
        ));
        {
            let sent = http.sent.lock().unwrap();
            assert!(sent[0].get("previous_response_id").is_none());
            assert_eq!(sent[1]["previous_response_id"], "resp_1");
            assert_eq!(
                sent[1]["input"],
                json!([{"role":"user","content":"continue"}])
            );
        }

        match boundary {
            // Keep input AND routing key identical: only active session changes.
            "fork" => req.cache.as_mut().unwrap().session_id = SessionId::new(),
            "compaction" => {
                req.messages =
                    Conversation::new(vec![Message::text(Role::User, "compacted checkpoint")])
                        .unwrap()
            }
            "unidentified" => req.cache = None,
            "resume" => generator = make_generator(),
            _ => unreachable!(),
        }
        let full: Value = serde_json::from_slice(
            &ego_ai::wire::openai_codex_responses::request_body(&req).unwrap(),
        )
        .unwrap();
        let attempt = attempt_for(&mut gate, &req);
        let third: Vec<_> = generator.generate(req, attempt).collect().await;
        assert!(matches!(
            third.last(),
            Some(AiEvent::Stop(StopReason::Stop))
        ));
        let sent = http.sent.lock().unwrap();
        assert_eq!(sent.len(), 3, "{boundary}");
        assert_eq!(sent[2]["type"], "response.create", "{boundary}");
        assert!(
            sent[2].get("previous_response_id").is_none(),
            "{boundary}: leaked response ID"
        );
        assert_eq!(
            sent[2]["input"], full["input"],
            "{boundary}: lost full history"
        );
        if boundary == "fork" {
            assert_eq!(sent[2]["prompt_cache_key"], sent[1]["prompt_cache_key"]);
            assert_eq!(sent[2]["input"], full["input"]);
        }
        assert_eq!(
            http.opened.load(std::sync::atomic::Ordering::SeqCst),
            if boundary == "compaction" { 1 } else { 2 },
            "{boundary}: wrong connection ownership",
        );
    }
}

// Scripted component faults exercise the public generator/transport boundary;
// they make no claim about provider responses or live cache behavior.
mod codex_recovery {
    use super::*;
    use ego_ai::http::HttpWebSocket;
    use ego_ai::wire::openai_codex_responses::OpenAiCodexResponses;
    use ego_core::event::id::SessionId;
    use ego_core::generator::types::PromptCache;
    use serde_json::{Value, json};
    use std::collections::VecDeque;

    enum Reply {
        Complete,
        Eof,
        Drop,
        TextThenDrop,
    }
    type SendScript = VecDeque<Result<Reply, TransportError>>;
    type OpenScript = Result<SendScript, TransportError>;

    struct Socket {
        sent: Arc<Mutex<Vec<Value>>>,
        sends: SendScript,
        replies: VecDeque<Result<Option<Vec<u8>>, TransportError>>,
    }
    impl HttpWebSocket for Socket {
        fn send(&mut self, call: PreparedHttpCall) -> BoxFuture<'_, Result<(), TransportError>> {
            let (_, _, _, request) = call.into_parts();
            self.sent
                .lock()
                .unwrap()
                .push(serde_json::from_slice(request.body()).unwrap());
            let result =
                self.sends
                    .pop_front()
                    .expect("unexpected send")
                    .map(|reply| {
                        let drop = || {
                            Err(TransportError::possibly_sent(
                                TransportFailure::ResponseBody("scripted drop".into()),
                            ))
                        };
                        self.replies =
                            match reply {
                                Reply::Complete => vec![Ok(Some(serde_json::to_vec(&json!({
                        "type":"response.completed",
                        "response":{"id":"resp_old","status":"completed","output":[]}
                    })).unwrap()))],
                                Reply::Eof => vec![Ok(None)],
                                Reply::Drop => vec![drop()],
                                Reply::TextThenDrop => {
                                    vec![Ok(Some(serde_json::to_vec(&json!({
                        "type":"response.output_text.delta", "delta":"visible"
                    })).unwrap())), drop()]
                                }
                            }
                            .into();
                    });
            Box::pin(async move { result })
        }
        fn receive(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
            let result = self.replies.pop_front().unwrap_or(Ok(None));
            Box::pin(async move { result })
        }
    }
    struct FaultHttp {
        sockets: Mutex<VecDeque<OpenScript>>,
        opened: std::sync::atomic::AtomicUsize,
        sent: Arc<Mutex<Vec<Value>>>,
        fallback: Arc<StubHttp>,
    }
    impl Http for FaultHttp {
        fn begin_call(&self) -> Box<dyn HttpCall> {
            self.fallback.begin_call()
        }
        fn open_websocket(
            &self,
            _: PreparedHttpCall,
        ) -> BoxFuture<'static, Result<Box<dyn HttpWebSocket>, TransportError>> {
            self.opened
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let result = self
                .sockets
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected reconnect")
                .map(|sends| {
                    Box::new(Socket {
                        sent: self.sent.clone(),
                        sends,
                        replies: VecDeque::new(),
                    }) as Box<dyn HttpWebSocket>
                });
            Box::pin(async move { result })
        }
    }
    fn setup(sockets: Vec<OpenScript>) -> (Arc<FaultHttp>, WireGenerator, GenRequest, BudgetGate) {
        let fallback = StubHttp::responding(
            200,
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_http\",\"status\":\"completed\",\"output\":[]}}\n\n",
        );
        let http = Arc::new(FaultHttp {
            sockets: Mutex::new(sockets.into()),
            opened: Default::default(),
            sent: Default::default(),
            fallback,
        });
        let generator = WireGenerator::new(
            ProviderRow::openai_codex_default(),
            &OpenAiCodexResponses,
            http.clone(),
            support::credential(),
        );
        let mut req = request();
        let session = SessionId::new();
        req.cache = Some(PromptCache {
            session_id: session,
            lineage_root: session,
            retention: CacheRetention::Short,
            stable_messages: 0,
        });
        (http, generator, req, BudgetGate::new(usd("10")))
    }
    async fn collect(
        generator: &WireGenerator,
        req: &GenRequest,
        gate: &mut BudgetGate,
    ) -> (Vec<AiEvent>, ego_core::generator::attempt::Settlement) {
        let mut stream = generator.generate(req.clone(), attempt_for(gate, req));
        let events = stream.by_ref().collect().await;
        (events, stream.settle(gate).unwrap())
    }
    fn extend(req: &mut GenRequest) {
        let mut messages = req.messages.messages().to_vec();
        messages.push(Message::text(Role::User, "continue"));
        req.messages = Conversation::new(messages).unwrap();
    }
    fn assert_full(sent: &Value, req: &GenRequest) {
        let full: Value = serde_json::from_slice(
            &ego_ai::wire::openai_codex_responses::request_body(req).unwrap(),
        )
        .unwrap();
        assert!(
            sent.get("previous_response_id").is_none(),
            "old socket response ID leaked"
        );
        assert_eq!(
            sent["input"], full["input"],
            "new connection lost full input"
        );
    }

    // Catches: a possibly-sent send, cancellation or authority refusal is
    // silently replayed by the session rather than returned to the retry owner.
    #[tokio::test]
    async fn codex_recovery_never_replays_uncertain_or_refused_send() {
        for error in [
            TransportError::possibly_sent(TransportFailure::Exchange("scripted send".into())),
            TransportError::definitely_unsent(TransportFailure::Cancelled),
            TransportError::definitely_unsent(TransportFailure::AuthorityMismatch),
        ] {
            let delivery = error.delivery();
            let (http, generator, req, mut gate) =
                setup(vec![Ok(vec![Ok(Reply::Complete), Err(error)].into())]);
            collect(&generator, &req, &mut gate).await;
            let (_, settled) = collect(&generator, &req, &mut gate).await;
            assert_eq!(settled.outcome.stop(), StopReason::Error);
            assert_eq!(settled.outcome.delivery(), delivery);
            assert_eq!(http.opened.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(http.fallback.calls(), 0);
        }
    }

    // Catches: EOF/drop succeeds or loses retryable classification, silently
    // resends within the attempt, or leaves a stale continuation for the next.
    #[tokio::test]
    async fn codex_recovery_midstream_drop_is_transient_and_next_attempt_is_full() {
        for reply in [Reply::Eof, Reply::Drop, Reply::TextThenDrop] {
            let (http, generator, mut req, mut gate) = setup(vec![
                Ok(vec![Ok(Reply::Complete), Ok(reply)].into()),
                Ok(vec![Ok(Reply::Complete)].into()),
            ]);
            collect(&generator, &req, &mut gate).await;
            extend(&mut req);
            let (events, settled) = collect(&generator, &req, &mut gate).await;
            assert_eq!(settled.outcome.stop(), StopReason::Error);
            assert_eq!(settled.outcome.delivery(), DeliveryCertainty::PossiblySent);
            assert!(events.iter().any(|e| matches!(
                e,
                AiEvent::Error {
                    kind: ErrorKind::Transient,
                    ..
                }
            )));
            assert_eq!(
                http.opened.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "must not replay midstream"
            );
            assert_eq!(http.fallback.calls(), 0);
            // Explicit next generation stands in for the caller's policy decision.
            let (_, next) = collect(&generator, &req, &mut gate).await;
            assert_eq!(next.outcome.stop(), StopReason::Stop);
            let sent = http.sent.lock().unwrap();
            assert_eq!(sent.len(), 3);
            assert_eq!(sent[1]["previous_response_id"], "resp_old");
            assert_full(&sent[2], &req);
        }
    }
}

// Catches: a key response being reduced to limit minus lifetime usage, or attached to another provider.
#[tokio::test]
async fn usage_limits_recorded_key_crosses_the_authenticated_http_boundary() {
    struct Clock;
    impl ego_core::event::time::Clock for Clock {
        fn now(&self) -> ego_core::event::time::Timestamp {
            ego_core::event::time::Timestamp::from_millis(1_009_843_200_000)
        }
    }
    let row = ProviderRow::openrouter_default();
    let credential = row
        .credential
        .resolve(&FakeCredentials::holding(KEY))
        .unwrap();
    let body = include_bytes!("fixtures/usage_limits/openrouter-key.json").to_vec();
    let http = StubHttp::new(Ok(OneHopResponse::streaming_response(
        200,
        Vec::new(),
        Box::pin(futures::stream::iter([Ok(body)])),
    )));
    let port = http.clone() as Arc<dyn Http>;
    let observed = ego_ai::usage_limits::openrouter_key_limits(&row, &credential, &port, &Clock)
        .await
        .unwrap()
        .unwrap();
    let json = serde_json::to_value(observed).unwrap();
    assert_eq!(json["provider"], "openrouter");
    assert!(
        json.get("model").is_none(),
        "current credential is not a historical session identity"
    );
    assert_eq!(json["observed_at"], 1_009_843_200_000_u64);
    assert_eq!(json["limits"][0]["remaining"], "73.123456789");
    assert_eq!(
        json["limits"][0]["source"],
        "openrouter_key:data.limit_remaining"
    );
    let calls = http.calls();
    assert!(
        ego_ai::usage_limits::openrouter_key_limits(&self::row(), &credential, &port, &Clock)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        http.calls(),
        calls,
        "a different provider must not contact OpenRouter"
    );
    let seen = http.seen.lock().unwrap();
    assert_eq!(seen[0].method, Method::Get);
    assert_eq!(seen[0].url.as_str(), "https://openrouter.ai/api/v1/key");
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|header| header.name() == "authorization")
    );
}

// Catches: a response publishing another provider's, ambiguous, or malformed counters.
// These are adversarial transport inputs, not live account recordings.
#[tokio::test]
async fn usage_limits_generation_rejects_cross_provider_and_ambiguous_headers() {
    async fn observed(
        row: ProviderRow,
        fields: Vec<HttpHeader>,
    ) -> Vec<ego_core::usage::ProviderLimit> {
        let credential = row
            .credential
            .resolve(&FakeCredentials::holding(KEY))
            .unwrap();
        let http = StubHttp::new(Ok(OneHopResponse::streaming_response(
            200,
            fields,
            Box::pin(futures::stream::iter([Ok(TEXT_TRANSCRIPT
                .as_bytes()
                .to_vec())])),
        )));
        let generator = WireGenerator::new(row, &OpenAiCompletions, http, credential);
        let mut gate = BudgetGate::new(usd("10"));
        let attempt = attempt_for(&mut gate, &request());
        generator
            .generate(request(), attempt)
            .filter_map(|event| async move {
                match event {
                    AiEvent::ProviderLimits { limits, .. } => Some(limits),
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect()
    }
    let header =
        |name: &str, value: &str| HttpHeader::new(name, value.as_bytes().to_vec()).unwrap();
    for (provider, remaining, limit) in [
        (
            ProviderRow::openai(),
            "X-RateLimit-Remaining-Tokens",
            "x-ratelimit-limit-tokens",
        ),
        (
            ProviderRow::anthropic_default(),
            "anthropic-ratelimit-tokens-remaining",
            "anthropic-ratelimit-tokens-limit",
        ),
    ] {
        let fields = || vec![header(remaining, "0"), header(limit, "100")];
        let limits = observed(provider.clone(), fields()).await;
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].remaining, Amount::ZERO);
        assert_eq!(limits[0].limit.unwrap().to_string(), "100");
        assert!(
            observed(ProviderRow::openrouter_default(), fields())
                .await
                .is_empty()
        );
        let other = if provider.id() == "openai" {
            ProviderRow::anthropic_default()
        } else {
            ProviderRow::openai()
        };
        assert!(observed(other, fields()).await.is_empty());
        for invalid in ["-1", "1.2", "NaN", "18446744073709551616", ""] {
            assert!(
                observed(provider.clone(), vec![header(remaining, invalid)])
                    .await
                    .is_empty(),
                "{invalid}"
            );
        }
        assert!(
            observed(
                provider.clone(),
                vec![header(remaining, "5"), header(remaining, "6")]
            )
            .await
            .is_empty()
        );
        assert!(
            observed(provider, vec![header(limit, "100")])
                .await
                .is_empty()
        );
    }
}

// Catches: missing, unlimited or malformed key remaining being invented from limit/usage.
#[tokio::test]
async fn usage_limits_key_boundary_preserves_recorded_absence_mutations() {
    struct Clock;
    impl ego_core::event::time::Clock for Clock {
        fn now(&self) -> ego_core::event::time::Timestamp {
            ego_core::event::time::Timestamp::from_millis(1500)
        }
    }
    let original: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/usage_limits/openrouter-key.json"))
            .unwrap();
    let mut cases = Vec::new();
    for replacement in [
        serde_json::Value::Null,
        serde_json::json!(-1),
        serde_json::json!("42"),
    ] {
        let mut mutated = original.clone();
        mutated["data"]["limit_remaining"] = replacement;
        cases.push(mutated);
    }
    let mut absent = original;
    absent["data"]
        .as_object_mut()
        .unwrap()
        .remove("limit_remaining");
    cases.push(absent);
    for body in cases {
        let row = ProviderRow::openrouter_default();
        let credential = row
            .credential
            .resolve(&FakeCredentials::holding(KEY))
            .unwrap();
        let http = StubHttp::new(Ok(OneHopResponse::streaming_response(
            200,
            Vec::new(),
            Box::pin(futures::stream::iter([Ok(
                serde_json::to_vec(&body).unwrap()
            )])),
        )));
        let port = http as Arc<dyn Http>;
        assert!(
            ego_ai::usage_limits::openrouter_key_limits(&row, &credential, &port, &Clock)
                .await
                .unwrap()
                .is_none()
        );
    }
}
