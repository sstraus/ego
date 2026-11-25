//! The binding: a `Generator` that actually reaches a provider.
//!
//! See `plans/m0b-vertical-slice.md` Step 13.5 and
//! `docs/internals/decisions/2026-08-21-a-generator-that-actually-reaches-a-provider.md`.
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

use ego_ai::protocol_filter::ChildFailure;
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

use crate::support;
use support::FakeCredentials;

const TEXT_TRANSCRIPT: &str = include_str!("../goldens/chat_completions/stream/text.sse");

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
        long_context: Vec::new(),
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
                        connection: None,
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

// Catches: a vendor-name allowlist advertises hosted search for a resolved
// function-only mapping, or rejects a search-capable mapping under another row.
#[test]
fn hosted_search_capability_follows_the_resolved_wire_not_the_provider_label() {
    use ego_ai::wire::openai_responses::OpenAiResponses;
    use ego_core::generator::types::HostedSearch;

    let http = StubHttp::responding(200, "");
    let mut req = request();
    req.web_search = Some(HostedSearch {
        max_requests: std::num::NonZeroU32::new(5).unwrap(),
        allow_live_access: true,
    });
    let function_only = generator(http.clone());
    assert!(!function_only.supports_web_search());
    assert!(function_only.ensure_expressible(&req).is_err());

    let hosted = WireGenerator::new(
        ProviderRow::ollama(Url::parse("https://fixture.example/v1/").unwrap()),
        &OpenAiResponses,
        http.clone(),
        credential(&FakeCredentials::holding(KEY)),
    );
    assert!(hosted.supports_web_search());
    hosted.ensure_expressible(&req).unwrap();
    assert_eq!(http.calls(), 0, "capability resolution must not dispatch");
}

// Catches: private reasoning or hosted-search history is replayed under another
// stable connection, including adopting legacy unbound history after selection.
#[test]
fn named_connections_private_history_requires_the_original_connection() {
    use ego_ai::wire::{
        openai_codex_responses::OpenAiCodexResponses, openai_responses::OpenAiResponses,
        xai_responses::XaiResponses,
    };
    let cases: [(&dyn WireFormat, &str, bool); 5] = [
        (&OpenAiResponses, "openai-responses", true),
        (&OpenAiCodexResponses, "openai-codex-responses", false),
        (&OpenAiCodexResponses, "openai-codex-responses", true),
        (&XaiResponses, "xai-responses", false),
        (&XaiResponses, "xai-responses", true),
    ];
    for (wire, dialect, provider_content) in cases {
        for origin in [None, Some("account-a"), Some("account-b")] {
            for selected in [None, Some("account-a"), Some("account-b")] {
                let http = StubHttp::responding(200, "");
                let state = ReasoningState {
                    connection: origin.map(str::to_owned),
                    dialect: dialect.into(),
                    blob: if provider_content {
                        r#"{"type":"web_search_call","id":"search_1","status":"completed","action":{"type":"search","query":"rust"}}"#
                    } else {
                        r#"{"type":"reasoning","id":"reasoning_1","encrypted_content":"opaque","summary":[]}"#
                    }.into(),
                };
                let block = if provider_content {
                    ContentBlock::ProviderContent { state, text: None }
                } else {
                    ContentBlock::Reasoning { state }
                };
                let mut req = request();
                req.thinking = ThinkingLevel::Low;
                req.messages = Conversation::new(vec![
                    Message::text(Role::User, "previous question"),
                    Message {
                        role: Role::Assistant,
                        content: vec![block],
                    },
                    Message::text(Role::User, "continue"),
                ])
                .unwrap();
                let generator = WireGenerator::new(
                    row(),
                    wire,
                    http.clone(),
                    credential(&FakeCredentials::holding(KEY)),
                )
                .with_connection_id(selected.map(str::to_owned));
                let result = generator.ensure_expressible(&req);
                assert_eq!(
                    result.is_ok(),
                    origin == selected,
                    "dialect={dialect}, origin={origin:?}, selected={selected:?}, provider_content={provider_content}: {result:?}"
                );
                assert_eq!(http.calls(), 0);
            }
        }
    }
}

// Catches: decoded hosted-search items retain no origin, so the next request
// cannot distinguish the generating connection from a foreign account.
#[tokio::test]
async fn named_connections_bind_decoded_private_items_before_persistence() {
    use ego_ai::wire::{
        WireFormat, openai_codex_responses::OpenAiCodexResponses,
        openai_responses::OpenAiResponses, xai_responses::XaiResponses,
    };
    let output: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../goldens/hosted-search/openai-output.json")).unwrap();
    let mut transcript = String::new();
    for item in output {
        transcript.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({"type":"response.output_item.done","item":item})
        ));
    }
    transcript.push_str("data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}}\n\n");
    let wires: [&dyn WireFormat; 3] = [&OpenAiResponses, &OpenAiCodexResponses, &XaiResponses];
    for wire in wires {
        let http = StubHttp::responding(200, &transcript);
        let generator = WireGenerator::new(
            row(),
            wire,
            http.clone(),
            credential(&FakeCredentials::holding(KEY)),
        )
        .with_connection_id(Some("account-a".into()));
        let mut req = request();
        req.thinking = ThinkingLevel::Low;
        let mut gate = BudgetGate::new(usd("10"));
        let mut stream = generator.generate(req.clone(), attempt_for(&mut gate, &req));
        let events: Vec<_> = stream.by_ref().collect().await;
        stream.settle(&mut gate).unwrap();
        let states: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AiEvent::ProviderContent { state, .. } => Some(state),
                _ => None,
            })
            .collect();
        assert!(!states.is_empty());
        assert!(
            states
                .iter()
                .all(|state| state.connection.as_deref() == Some("account-a"))
        );
        assert_eq!(http.calls(), 1);
    }
}

/// The fixture row's credential, resolved.
///
/// A generator cannot be built without one, which is the type-level half of
/// `docs/internals/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §6:
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
/// `GenerationAdmission`'s constructor is sealed in `ego_core::budget`, so this has to
/// go through `BudgetGate::admit` — the same fact `tests/skeleton.rs` leans
/// on, and the reason a generator cannot route around the gate.
fn attempt_for(gate: &mut BudgetGate, req: &GenRequest) -> GenerationAttempt {
    let reservation = gate
        .admit(&metered(), Some(&quote()), req, &caps())
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

// Catches: an external Codex token leaks into receipts or reflected auth errors.
#[tokio::test]
async fn external_token_absent_from_receipts_and_errors() {
    use ego_ai::provider::{Credentials, MissingCredential, Secret};
    const TOKEN: &str = "head.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xIn19.sig";
    struct External;
    impl Credentials for External {
        fn secret(&self, name: &str) -> Result<Secret, MissingCredential> {
            if name == "OPENAI_CODEX_ACCESS_TOKEN" {
                Ok(TOKEN.into())
            } else {
                Err(MissingCredential { var: name.into() })
            }
        }
    }
    // The live service uses a detail envelope, unlike the client-derived
    // nested error probe. Both must remain typed, terminal and secret-free.
    let reflected = format!(r#"{{"error":{{"message":"expired {TOKEN}"}}}}"#);
    for body in [
        reflected.as_str(),
        include_str!("../goldens/openai-codex/auth-401-live.json"),
    ] {
        let row = ProviderRow::openai_codex_default();
        let credential = row.credential.resolve(&External).unwrap();
        assert!(!format!("{row:?} {credential:?}").contains(TOKEN));
        let http = StubHttp::responding(401, body);
        let generator = WireGenerator::new(
            row,
            &ego_ai::wire::openai_codex_responses::OpenAiCodexResponses,
            Arc::clone(&http) as Arc<dyn Http>,
            credential,
        );
        let mut gate = BudgetGate::new(usd("10"));
        let mut req = request();
        req.model = "gpt-5.6-sol".into();
        let attempt = attempt_for(&mut gate, &req);
        let mut stream = generator.generate(req, attempt);
        let events: Vec<_> = stream.by_ref().collect().await;
        let receipt = stream.settle(&mut gate).unwrap();
        assert!(!format!("{receipt:?}").contains(TOKEN));
        assert!(events.iter().any(|event| matches!(
            event,
            AiEvent::Error {
                kind: ego_core::generator::types::ErrorKind::Refused,
                ..
            }
        )));
        assert_eq!(http.calls(), 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::Stop(StopReason::Error)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::ProviderFailure { .. }))
        );
        assert!(!format!("{events:?}").contains(TOKEN));
        assert!(!format!("{:?}", http.seen.lock().unwrap()).contains(TOKEN));
    }
}

// Catches: direct HTTP omits dispatch evidence, duplicates it, or alters
// assistant content/accounting while inserting local metadata.
#[tokio::test]
async fn cache_transport_direct_http_preserves_recorded_content_and_usage() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = generator(http.clone());
    let req = request();
    let mut gate = BudgetGate::new(usd("10"));
    let mut stream = generator.generate(req.clone(), attempt_for(&mut gate, &req));
    let events: Vec<_> = stream.by_ref().collect().await;
    let settled = stream.settle(&mut gate).unwrap();
    assert_eq!(
        settled.outcome.cache_transport(),
        Some(ego_core::usage::CacheTransport::HttpFull)
    );
    assert!(matches!(
        events.first(),
        Some(AiEvent::CacheTransportObserved(
            ego_core::usage::CacheTransport::HttpFull
        ))
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AiEvent::CacheTransportObserved(_)))
            .count(),
        1
    );
    let text = events
        .iter()
        .filter_map(|event| match event {
            AiEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(text, "Due file — perché no? 😀");
    let usage = events
        .iter()
        .find_map(|event| match event {
            AiEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .unwrap();
    assert_eq!(usage.cache_read_tokens, 400);
    assert_eq!(usage.cache_write_tokens, 100);
    assert_eq!(usage.output_tokens, 250);
    assert_eq!(http.calls(), 1);
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
        gate.committed()
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

    // The binding owns local dispatch evidence; the wire decoder still owns every provider event.
    let expected: Vec<_> = std::iter::once(AiEvent::CacheTransportObserved(
        ego_core::usage::CacheTransport::HttpFull,
    ))
    .chain(direct)
    .collect();
    assert_eq!(through_binding, expected);
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

/// Story `031-806b`'s fifth criterion, stated as a property of the type
/// rather than of who may call an accessor: a row names the variable its
/// credential lives in and never holds the value.
///
/// The row is asked for its headers *first*, and inspected after. That
/// ordering is the test. It used to be true only that a held [`Secret`]
/// declined to print itself — a redaction one `derive` away from being wrong
/// (`docs/internals/decisions/2026-08-21-a-profile-is-not-a-protocol.md` §3). Resolving
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
    let transcript = include_str!("../goldens/chat_completions/stream/tool_calls.sse")
        .replace("read_file", ALIAS);
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
        let transcript = include_str!("../goldens/chat_completions/stream/tool_calls.sse")
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
            AiEvent::ToolCallRejected { name, message, .. }
                if name == original && message.contains("Declared tools:"))));
        assert!(!events.iter().any(|event| matches!(
            event,
            AiEvent::Error { .. } | AiEvent::Stop(StopReason::Error)
        )));
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
            "claude-opus-5",
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
            verifier: None,
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
        RateLimitsThenComplete,
    }
    type SendScript = VecDeque<Result<Reply, TransportError>>;
    type OpenScript = Result<SendScript, TransportError>;

    struct Socket {
        sent: Arc<Mutex<Vec<Value>>>,
        sends: SendScript,
        replies: VecDeque<Result<Option<Vec<u8>>, TransportError>>,
        // The real transport refuses a send whose headers differ from the handshake.
        handshake: Vec<ego_core::permit::HttpHeader>,
    }
    impl HttpWebSocket for Socket {
        fn send(&mut self, call: PreparedHttpCall) -> BoxFuture<'_, Result<(), TransportError>> {
            let (_, _, _, request) = call.into_parts();
            if request.headers() != self.handshake.as_slice() {
                return Box::pin(async {
                    Err(TransportError::definitely_unsent(
                        TransportFailure::AuthorityMismatch,
                    ))
                });
            }
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
                                Reply::RateLimitsThenComplete => vec![
                                    Ok(Some(serde_json::to_vec(&rate_limits_event()).unwrap())),
                                    Ok(Some(serde_json::to_vec(&json!({
                        "type":"response.completed",
                        "response":{"id":"resp_old","status":"completed","output":[]}
                    })).unwrap())),
                                ],
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
        quota: Mutex<Vec<(String, String, Value)>>,
    }
    impl Http for FaultHttp {
        fn observe_rate_limit_event(&self, provider: &str, credential: &str, event: &Value) {
            self.quota
                .lock()
                .unwrap()
                .push((provider.into(), credential.into(), event.clone()));
        }
        fn begin_call(&self) -> Box<dyn HttpCall> {
            self.fallback.begin_call()
        }
        fn open_websocket(
            &self,
            call: PreparedHttpCall,
        ) -> BoxFuture<'static, Result<Box<dyn HttpWebSocket>, TransportError>> {
            let handshake = call.into_parts().3.headers().to_vec();
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
                        handshake,
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
            quota: Default::default(),
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
            verifier: None,
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

    // Catches: transport metadata predicts a logical full request before the
    // session rewrites it, or confuses a reused full send with a fresh socket.
    #[tokio::test]
    async fn cache_transport_tracks_actual_socket_and_rewritten_body_through_settlement() {
        use ego_core::usage::CacheTransport;
        let (http, generator, mut req, mut gate) = setup(vec![Ok(vec![
            Ok(Reply::Complete),
            Ok(Reply::Complete),
            Ok(Reply::Complete),
        ]
        .into())]);
        for expected in [
            CacheTransport::WebSocketFresh,
            CacheTransport::WebSocketContinuation,
            CacheTransport::WebSocketReusedFull,
        ] {
            let (events, settled) = collect(&generator, &req, &mut gate).await;
            assert_eq!(settled.outcome.cache_transport(), Some(expected));
            assert_eq!(settled.outcome.stop(), StopReason::Stop);
            assert!(
                matches!(events.first(), Some(AiEvent::CacheTransportObserved(mode)) if *mode == expected)
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AiEvent::CacheTransportObserved(_)))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AiEvent::Stop(_)))
                    .count(),
                1
            );
            match expected {
                CacheTransport::WebSocketFresh => extend(&mut req),
                CacheTransport::WebSocketContinuation => {
                    req.messages =
                        Conversation::new(vec![Message::text(Role::User, "compacted checkpoint")])
                            .unwrap();
                }
                _ => {}
            }
        }
        assert_eq!(http.opened.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(http.fallback.calls(), 0);
        let sent = http.sent.lock().unwrap();
        assert!(sent[0].get("previous_response_id").is_none());
        assert_eq!(sent[1]["previous_response_id"], "resp_old");
        assert_eq!(
            sent[1]["input"],
            json!([{"role":"user","content":"continue"}])
        );
        assert_full(&sent[2], &req);
        for body in sent.iter() {
            assert!(
                body.get("cache_transport").is_none(),
                "local evidence must not become provider input"
            );
        }
    }

    // Catches: a reused socket receiving a request whose routing headers changed
    // (the prefix cache key follows the system text, model and tools), which the
    // transport refuses as an authority mismatch instead of opening a new socket.
    #[tokio::test]
    async fn changed_routing_headers_open_a_new_socket_with_a_full_request() {
        let (http, generator, mut req, mut gate) = setup(vec![
            Ok(vec![Ok(Reply::Complete)].into()),
            Ok(vec![Ok(Reply::Complete)].into()),
        ]);
        collect(&generator, &req, &mut gate).await;
        extend(&mut req);
        let mut messages = vec![Message::text(
            Role::System,
            "A different system text changes the prefix key.",
        )];
        messages.extend(req.messages.messages().iter().cloned());
        req.messages = Conversation::new(messages).unwrap();
        let (_, settled) = collect(&generator, &req, &mut gate).await;
        assert_eq!(settled.outcome.stop(), StopReason::Stop);
        assert_eq!(
            settled.outcome.cache_transport(),
            Some(ego_core::usage::CacheTransport::WebSocketFresh)
        );
        assert_eq!(http.opened.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_full(&http.sent.lock().unwrap()[1], &req);
    }

    // Catches: an unavailable WebSocket is reported as a fresh socket after
    // the admitted path actually falls back to a full HTTP request.
    #[tokio::test]
    async fn cache_transport_fallback_reports_http_once_and_preserves_settlement() {
        let (http, generator, req, mut gate) = setup(vec![Err(TransportError::definitely_unsent(
            TransportFailure::Connect("scripted unavailable".into()),
        ))]);
        let (events, settled) = collect(&generator, &req, &mut gate).await;
        assert_eq!(
            settled.outcome.cache_transport(),
            Some(ego_core::usage::CacheTransport::HttpFull)
        );
        assert_eq!(settled.outcome.stop(), StopReason::Stop);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AiEvent::CacheTransportObserved(_)))
                .count(),
            1
        );
        assert!(matches!(
            events.first(),
            Some(AiEvent::CacheTransportObserved(
                ego_core::usage::CacheTransport::HttpFull
            ))
        ));
        assert!(http.sent.lock().unwrap().is_empty());
        assert_eq!(http.fallback.calls(), 1);
        assert_full(
            &serde_json::from_slice::<Value>(
                http.fallback.seen.lock().unwrap()[0]
                    .body
                    .as_deref()
                    .unwrap(),
            )
            .unwrap(),
            &req,
        );
    }

    // Catches: failed handshakes or sends mint transport proof without a
    // successful exchange, or a failed socket is silently retried over HTTP.
    #[tokio::test]
    async fn cache_transport_clean_failures_publish_no_proof() {
        for script in [
            Err(TransportError::definitely_unsent(
                TransportFailure::AuthorityMismatch,
            )),
            Ok(vec![Err(TransportError::definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))]
            .into()),
        ] {
            let (http, generator, req, mut gate) = setup(vec![script]);
            let (events, settled) = collect(&generator, &req, &mut gate).await;
            assert_eq!(settled.outcome.cache_transport(), None);
            assert_eq!(settled.outcome.stop(), StopReason::Error);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, AiEvent::CacheTransportObserved(_)))
            );
            assert_eq!(http.fallback.calls(), 0);
        }
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

    /// The recorded key names of the WebSocket quota event, with synthetic values.
    fn rate_limits_event() -> Value {
        json!({
            "type": "codex.rate_limits",
            "plan_type": "pro",
            "rate_limits": {
                "allowed": true,
                "limit_reached": false,
                "primary": {"used_percent": 37, "window_minutes": 10080, "reset_after_seconds": 100, "reset_at": 1_795_000_000_u64},
                "secondary": null
            },
            "credits": {"has_credits": true, "unlimited": false, "balance": "12.5"}
        })
    }

    // Catches: a Codex WebSocket generation never publishing its quota. The
    // recorded handshake carries no x-codex quota headers, so the in-stream
    // `codex.rate_limits` event is the only quota a WebSocket session states.
    #[tokio::test]
    async fn codex_websocket_rate_limits_event_reaches_the_quota_port() {
        let (http, generator, req, mut gate) =
            setup(vec![Ok(vec![Ok(Reply::RateLimitsThenComplete)].into())]);
        let (_, settled) = collect(&generator, &req, &mut gate).await;
        assert_eq!(settled.outcome.stop(), StopReason::Stop);
        let quota = http.quota.lock().unwrap();
        assert_eq!(quota.len(), 1);
        assert_eq!(quota[0].0, "openai-codex");
        assert_eq!(
            quota[0].1,
            ego_ai::usage_limits::credential_fingerprint(&support::credential().headers())
        );
        assert_eq!(quota[0].2, rate_limits_event());
    }
}

// Catches: a key response being reduced to limit minus lifetime usage, or attached to another provider.
#[tokio::test]
async fn usage_limits_recorded_key_crosses_the_authenticated_http_boundary() {
    struct Clock;
    impl ego_core::event::time::Clock for Clock {
        fn now(&self) -> ego_core::event::time::Timestamp {
            ego_core::event::time::Timestamp::from_millis(1_793_110_247_363)
        }
    }
    let row = ProviderRow::openrouter_default();
    let credential = row
        .credential
        .resolve(&FakeCredentials::holding(KEY))
        .unwrap();
    let body = include_bytes!("../fixtures/usage_limits/openrouter-key.json").to_vec();
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
    assert_eq!(json["observed_at"], 1_793_110_247_363_u64);
    assert_eq!(json["limits"][0]["remaining"], "195.970819484");
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
    let original: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../fixtures/usage_limits/openrouter-key.json"
    ))
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

struct TestProtocolFilter {
    calls: std::sync::atomic::AtomicUsize,
    result: Result<Vec<u8>, ChildFailure>,
}
impl ego_ai::protocol_filter::ProtocolFilterCommand for TestProtocolFilter {
    fn open_response(
        &self,
    ) -> Result<Box<dyn ego_ai::protocol_filter::ProtocolFilterResponse>, ChildFailure> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(PassthroughResponse(self.result.clone())))
    }
}

struct PassthroughResponse(Result<Vec<u8>, ChildFailure>);
impl ego_ai::protocol_filter::ProtocolFilterResponse for PassthroughResponse {
    fn exchange(&mut self, input: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, ChildFailure>> {
        let input: serde_json::Value = serde_json::from_slice(&input).unwrap();
        let output = match input["direction"].as_str().unwrap() {
            "hello" => Ok(br#"{"version":1,"request":true,"response":true}"#.to_vec()),
            "request" => {
                assert!(input.get("headers").is_none());
                assert!(input.get("url").is_none());
                self.0.clone()
            }
            "response" => Ok(serde_json::to_vec(&serde_json::json!({
                "version":1,"sequence":input["sequence"],"body_base64":input["body_base64"]
            }))
            .unwrap()),
            _ => unreachable!(),
        };
        Box::pin(futures::future::ready(output))
    }
    fn finish(&mut self) -> BoxFuture<'_, Result<(), ChildFailure>> {
        Box::pin(futures::future::ready(Ok(())))
    }
}
fn protocol_config() -> ego_core::config::ProtocolFilterConfig {
    ego_core::config::ProtocolFilterConfig {
        command: if cfg!(windows) {
            "C:/filters/filter.exe"
        } else {
            "/filters/filter"
        }
        .into(),
        timeout_ms: 1000,
        enabled: true,
    }
}

#[derive(Default)]
struct FilterJournalStore {
    events: Mutex<Vec<ego_core::event::domain::DomainEvent>>,
    fail: std::sync::atomic::AtomicBool,
}
impl ego_core::store::session_store::SessionStore for FilterJournalStore {
    fn append(
        &self,
        _: &ego_core::event::id::SessionId,
        event: &ego_core::event::domain::DomainEvent,
    ) -> Result<(), ego_core::store::session_store::StoreError> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ego_core::store::session_store::StoreError::Io(
                std::io::Error::other("disk refused append"),
            ));
        }
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
    fn read(
        &self,
        _: &ego_core::event::id::SessionId,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<
                ego_core::store::session_store::SessionEntry,
                ego_core::store::session_store::StoreError,
            >,
        >,
        ego_core::store::session_store::StoreError,
    > {
        Ok(futures::stream::iter(
            self.events
                .lock()
                .unwrap()
                .clone()
                .into_iter()
                .map(|event| Ok(ego_core::store::session_store::SessionEntry::Event(event))),
        )
        .boxed())
    }
    fn list(
        &self,
    ) -> Result<
        Vec<ego_core::store::session_store::SessionMeta>,
        ego_core::store::session_store::StoreError,
    > {
        Ok(vec![])
    }
    fn compare_and_append_run(
        &self,
        _: &ego_core::event::id::SessionId,
        _: Option<ego_core::run::RunRevision>,
        _: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, ego_core::store::session_store::StoreError> {
        Err(ego_core::store::session_store::StoreError::Io(
            std::io::Error::other("not a run-control store"),
        ))
    }
}
struct FilterClock;
impl ego_core::event::time::Clock for FilterClock {
    fn now(&self) -> ego_core::event::time::Timestamp {
        ego_core::event::time::Timestamp::EPOCH
    }
}

async fn observe_filtered(
    stream: &mut ego_core::generator::attempt::AttemptStream,
    session: Option<ego_core::event::id::SessionId>,
) -> Vec<AiEvent> {
    observe_filtered_with(stream, session, usize::MAX).await
}

/// Serves at most `acks` journal exchanges; later ones are dropped unanswered.
async fn observe_filtered_with(
    stream: &mut ego_core::generator::attempt::AttemptStream,
    session: Option<ego_core::event::id::SessionId>,
    acks: usize,
) -> Vec<AiEvent> {
    use ego_core::event::id::{ItemId, Producer, TurnId};
    use ego_core::turn::{Journal, SharedJournal};
    let store = FilterJournalStore::default();
    let journal = SharedJournal::new(Journal::new(
        session.unwrap_or_default(),
        &store,
        &FilterClock,
        &ego_core::sink::NullSink,
        Producer::Main,
        ego_core::state::SessionState::default(),
    ));
    let turn = TurnId::new();
    journal
        .append(
            turn,
            Some(ItemId::new()),
            None,
            ego_core::event::domain::EventKind::UserPrompt {
                text: "fixture prompt".into(),
                images: vec![],
                audio: vec![],
                queued_prompt: None,
            },
        )
        .unwrap();
    let mut observed = Vec::new();
    let mut served = 0;
    while let Some(event) = stream.next().await {
        if let AiEvent::FilterState(exchange) = event {
            if served < acks {
                served += 1;
                journal
                    .filter_state(
                        turn,
                        ego_core::event::domain::GenerationRole::Main,
                        exchange,
                    )
                    .unwrap();
            }
        } else {
            observed.push(event);
        }
    }
    observed
}

fn state_filter_generator(
    http: Arc<StubHttp>,
    revision: u64,
    writes: serde_json::Value,
) -> WireGenerator {
    let config = protocol_config();
    generator(http).with_protocol_filter(
        config,
        Arc::new(TestProtocolFilter {
            calls: 0.into(),
            result: Ok(serde_json::to_vec(&serde_json::json!({
                "version":1, "body_base64":"e30=", "identity_headers":[],
                "expected_revision":revision, "state_writes":writes,
            }))
            .unwrap()),
        }),
    )
}

// Catches: dispatch before durable variable acceptance, including a failed
// journal write, a forged revision, invalid variable names or oversized state.
#[tokio::test]
async fn protocol_filter_invalid_state_or_failed_persistence_never_dispatches() {
    use ego_core::event::id::{ItemId, Producer, SessionId, TurnId};
    use ego_core::turn::{Journal, SharedJournal};
    for (name, revision, writes, fail_store) in [
        (
            "disk failure",
            0,
            serde_json::json!({"captured":"first"}),
            true,
        ),
        (
            "wrong revision",
            1,
            serde_json::json!({"captured":"first"}),
            false,
        ),
        (
            "oversized value",
            0,
            serde_json::json!({"captured":"x".repeat(4097)}),
            false,
        ),
        (
            "invalid key",
            0,
            serde_json::json!({"bad key":"first"}),
            false,
        ),
    ] {
        let store = FilterJournalStore::default();
        let journal = SharedJournal::new(Journal::new(
            SessionId::new(),
            &store,
            &FilterClock,
            &ego_core::sink::NullSink,
            Producer::Main,
            ego_core::state::SessionState::default(),
        ));
        let turn = TurnId::new();
        journal
            .append(
                turn,
                Some(ItemId::new()),
                None,
                ego_core::event::domain::EventKind::UserPrompt {
                    text: "fixture prompt".into(),
                    images: vec![],
                    audio: vec![],
                    queued_prompt: None,
                },
            )
            .unwrap();
        store
            .fail
            .store(fail_store, std::sync::atomic::Ordering::SeqCst);
        let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
        let generator = state_filter_generator(http.clone(), revision, writes);
        let mut gate = BudgetGate::new(usd("10"));
        let mut stream = generator.generate(request(), attempt_for(&mut gate, &request()));
        let mut error = false;
        while let Some(event) = stream.next().await {
            match event {
                AiEvent::FilterState(exchange) => {
                    let _ = journal.filter_state(
                        turn,
                        ego_core::event::domain::GenerationRole::Main,
                        exchange,
                    );
                }
                AiEvent::Error { .. } => error = true,
                _ => {}
            }
        }
        assert!(error, "{name}");
        assert_eq!(http.calls(), 0, "{name}");
        assert_eq!(store.events.lock().unwrap().len(), 1, "{name}");
        assert_eq!(
            stream.settle(&mut gate).unwrap().outcome.delivery(),
            DeliveryCertainty::DefinitelyUnsent,
            "{name}"
        );
    }
}

// Catches: using stale prepared bytes after another attempt has committed state.
#[tokio::test]
async fn protocol_filter_concurrent_snapshot_conflict_prevents_second_delivery() {
    use ego_core::event::id::{ItemId, Producer, SessionId, TurnId};
    use ego_core::turn::{Journal, SharedJournal};
    let store = FilterJournalStore::default();
    let journal = SharedJournal::new(Journal::new(
        SessionId::new(),
        &store,
        &FilterClock,
        &ego_core::sink::NullSink,
        Producer::Main,
        ego_core::state::SessionState::default(),
    ));
    let turn = TurnId::new();
    journal
        .append(
            turn,
            Some(ItemId::new()),
            None,
            ego_core::event::domain::EventKind::UserPrompt {
                text: "fixture prompt".into(),
                images: vec![],
                audio: vec![],
                queued_prompt: None,
            },
        )
        .unwrap();
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator =
        state_filter_generator(http.clone(), 0, serde_json::json!({"captured":"first"}));
    let mut gate = BudgetGate::new(usd("10"));
    let mut first = generator.generate(request(), attempt_for(&mut gate, &request()));
    let mut second = generator.generate(request(), attempt_for(&mut gate, &request()));
    for stream in [&mut first, &mut second] {
        let Some(AiEvent::FilterState(exchange)) = stream.next().await else {
            panic!("missing state read")
        };
        journal
            .filter_state(
                turn,
                ego_core::event::domain::GenerationRole::Main,
                exchange,
            )
            .unwrap();
    }
    // The first response cannot even open HTTP until the journal accepts it.
    let Some(AiEvent::FilterState(exchange)) = first.next().await else {
        panic!("missing state save")
    };
    assert_eq!(http.calls(), 0);
    journal
        .filter_state(
            turn,
            ego_core::event::domain::GenerationRole::Main,
            exchange,
        )
        .unwrap();
    let Some(AiEvent::FilterState(exchange)) = second.next().await else {
        panic!("missing second state save")
    };
    journal
        .filter_state(
            turn,
            ego_core::event::domain::GenerationRole::Main,
            exchange,
        )
        .unwrap();
    while first.next().await.is_some() {}
    let second_events: Vec<_> = second.by_ref().collect().await;
    assert!(second_events.iter().any(|event| matches!(event, AiEvent::Error { message, .. } if message.contains("revision conflict"))));
    assert_eq!(http.calls(), 1);
    assert_eq!(
        second.settle(&mut gate).unwrap().outcome.delivery(),
        DeliveryCertainty::DefinitelyUnsent
    );
    first.settle(&mut gate).unwrap();
    let events = store.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    let mut replayed = ego_core::state::SessionState::default();
    // Replay serialized durable bytes, not the already reduced in-memory state.
    for event in &events {
        let event = serde_json::from_slice(&serde_json::to_vec(event).unwrap()).unwrap();
        replayed.apply(&event);
    }
    let saved = replayed.protocol_filter_variables.values().next().unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(
        saved.values.get("captured").map(String::as_str),
        Some("first")
    );
    // Fork/lineage replay may retain a parent's events, but a new session
    // instance must never inherit the parent's filter variables.
    let ego_core::event::domain::EventKind::ProtocolFilterVariablesSaved { namespace, .. } =
        &events[1].kind
    else {
        panic!("missing state event")
    };
    let new_instance = SharedJournal::new(Journal::new(
        SessionId::new(),
        &store,
        &FilterClock,
        &ego_core::sink::NullSink,
        Producer::Main,
        replayed,
    ));
    let (read, reply) =
        ego_core::generator::filter_state::FilterStateExchange::read(namespace.clone());
    new_instance
        .filter_state(turn, ego_core::event::domain::GenerationRole::Main, read)
        .unwrap();
    assert!(reply.await.unwrap().unwrap().state.values.is_empty());
}

// Catches: running a user command during pure preflight, or treating a filter failure as possibly sent/retryable provider I/O.
#[tokio::test]
async fn protocol_filter_preflight_is_pure_and_failure_is_definitely_unsent() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let command = Arc::new(TestProtocolFilter {
        calls: 0.into(),
        result: Err(ChildFailure::InvalidResult),
    });
    let generator =
        generator(http.clone()).with_protocol_filter(protocol_config(), command.clone());
    generator.ensure_expressible(&request()).unwrap();
    assert_eq!(command.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &request());
    let mut stream = generator.generate(request(), attempt);
    let events = observe_filtered(&mut stream, None).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::Invalid,
            ..
        }
    )));
    let settlement = stream.settle(&mut gate).unwrap();
    assert_eq!(
        settlement.outcome.delivery(),
        DeliveryCertainty::DefinitelyUnsent
    );
    assert_eq!(http.calls(), 0);
    assert_eq!(command.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// Inject failures at the command port, not at the retry policy under test.
struct TimeoutFilter {
    calls: std::sync::atomic::AtomicUsize,
    failures: usize,
    phase: &'static str,
    error: ChildFailure,
    stopped: Option<ChildFailure>,
    reject_after_timeouts: bool,
}
impl ego_ai::protocol_filter::ProtocolFilterCommand for TimeoutFilter {
    fn open_response(
        &self,
    ) -> Result<Box<dyn ego_ai::protocol_filter::ProtocolFilterResponse>, ChildFailure> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(TimeoutResponse {
            phase: self.phase,
            failure: (call < self.failures).then_some(self.error),
            reject_after_timeouts: self.reject_after_timeouts,
        }))
    }
    fn check_live(&self) -> Result<(), ChildFailure> {
        if self.calls.load(std::sync::atomic::Ordering::SeqCst) > 0
            && let Some(error) = self.stopped
        {
            return Err(error);
        }
        Ok(())
    }
}
struct TimeoutResponse {
    phase: &'static str,
    failure: Option<ChildFailure>,
    reject_after_timeouts: bool,
}
impl ego_ai::protocol_filter::ProtocolFilterResponse for TimeoutResponse {
    fn exchange(&mut self, input: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, ChildFailure>> {
        Box::pin(async move {
            let input: serde_json::Value = serde_json::from_slice(&input).unwrap();
            if self.phase == "reject" {
                return Ok(br#"{"version":1,"reject":true}"#.to_vec());
            }
            if self.phase == "malformed" {
                return Ok(b"not JSON".to_vec());
            }
            if (input["direction"] == self.phase
                || (self.phase == "response_only" && input["direction"] == "response"))
                && let Some(error) = self.failure
            {
                return Err(error);
            }
            if self.reject_after_timeouts && input["direction"] == "request" {
                return Ok(
                    br#"{"version":1,"reject":true,"notices":["final policy reason"]}"#.to_vec(),
                );
            }
            match input["direction"].as_str().unwrap() {
                "hello" => Ok(serde_json::to_vec(&serde_json::json!({
                    "version":1, "request":self.phase != "response_only", "response":self.phase != "finish",
                    "notices":["retry fixture A", "retry fixture B"]
                }))
                .unwrap()),
                "request" | "mcp" => Ok(serde_json::to_vec(&serde_json::json!({
                    "version":1, "body_base64":input["body_base64"],
                    "identity_headers":[], "expected_revision":input["state"]["revision"],
                    "state_writes":{}, "notices":["request accepted", "request detail"]
                }))
                .unwrap()),
                "response" => Ok(serde_json::to_vec(&serde_json::json!({
                    "version":1,"sequence":input["sequence"],"body_base64":input["body_base64"],
                    "notices":["response accepted"]
                }))
                .unwrap()),
                _ => unreachable!(),
            }
        })
    }
    fn finish(&mut self) -> BoxFuture<'_, Result<(), ChildFailure>> {
        Box::pin(futures::future::ready(if self.phase == "finish" {
            self.failure.map_or(Ok(()), Err)
        } else {
            Ok(())
        }))
    }
}

// Catches: immediate timeout failure, only three total attempts, unlimited retries,
// replay after state acceptance/dispatch, and retrying non-timeout failures.
#[tokio::test]
async fn protocol_filter_timeout_retries_are_bounded_and_precommit_only() {
    for (phase, failures, error, stopped, expected_calls, sent) in [
        ("hello", 3, ChildFailure::CommandTimeout, None, 4, 1),
        ("request", 3, ChildFailure::CommandTimeout, None, 4, 1),
        (
            "request",
            usize::MAX,
            ChildFailure::CommandTimeout,
            None,
            4,
            0,
        ),
        ("request", 1, ChildFailure::Exit, None, 1, 0),
        (
            "request",
            1,
            ChildFailure::CommandTimeout,
            Some(ChildFailure::Cancelled),
            1,
            0,
        ),
        (
            "request",
            1,
            ChildFailure::CommandTimeout,
            Some(ChildFailure::RunTimeout),
            1,
            0,
        ),
        ("reject", 1, ChildFailure::CommandTimeout, None, 1, 0),
        ("malformed", 1, ChildFailure::CommandTimeout, None, 1, 0),
        ("finish", 1, ChildFailure::CommandTimeout, None, 1, 0),
        ("response", 1, ChildFailure::CommandTimeout, None, 1, 1),
        ("response_only", 1, ChildFailure::CommandTimeout, None, 1, 1),
    ] {
        let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
        let command = Arc::new(TimeoutFilter {
            calls: 0.into(),
            failures,
            phase,
            error,
            stopped,
            reject_after_timeouts: false,
        });
        let generator =
            generator(http.clone()).with_protocol_filter(protocol_config(), command.clone());
        let mut gate = BudgetGate::new(usd("10"));
        let attempt = attempt_for(&mut gate, &request());
        let mut stream = generator.generate(request(), attempt);
        let events = observe_filtered(&mut stream, None).await;
        let case = format!("{phase}, {failures}, {error:?}, {stopped:?}");
        assert_eq!(
            command.calls.load(std::sync::atomic::Ordering::SeqCst),
            expected_calls,
            "{case}"
        );
        assert_eq!(http.calls(), sent, "{case}");
        if phase == "request" && stopped.is_none() && error == ChildFailure::CommandTimeout {
            let notices: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AiEvent::FilterNotice { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let expected = if sent == 1 {
                vec![
                    "retry fixture A",
                    "retry fixture B",
                    "request accepted",
                    "request detail",
                ]
            } else {
                vec!["retry fixture A", "retry fixture B"]
            };
            assert_eq!(notices, expected, "{case}");
        }
        if phase == "reject" {
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    AiEvent::Error {
                        kind: ErrorKind::PolicyRejected,
                        ..
                    }
                )),
                "{case}"
            );
        }
        let succeeds = sent == 1 && !matches!(phase, "response" | "response_only");
        assert_eq!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::Error { .. })),
            !succeeds,
            "{case}"
        );
        if succeeds {
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, AiEvent::TextDelta { .. })),
                "{case}"
            );
        }
        let settlement = stream.settle(&mut gate).unwrap();
        if sent == 0 {
            assert_eq!(
                settlement.outcome.delivery(),
                DeliveryCertainty::DefinitelyUnsent,
                "{case}"
            );
        }
    }
}

// Catches: egress bypassing timeout retries or replaying a denial/expired scope.
#[tokio::test]
async fn protocol_filter_egress_timeout_retries_preserve_read_only_boundary() {
    for (phase, failures, stopped, expected_calls, expected_error) in [
        ("mcp", 3, None, 4, None),
        (
            "mcp",
            usize::MAX,
            None,
            4,
            Some("protocol_filter timed out"),
        ),
        ("finish", 3, None, 4, None),
        (
            "reject",
            1,
            None,
            1,
            Some("protocol_filter policy rejected this attempt"),
        ),
        (
            "mcp",
            1,
            Some(ChildFailure::Cancelled),
            1,
            Some("protocol_filter cancelled"),
        ),
        (
            "mcp",
            1,
            Some(ChildFailure::RunTimeout),
            1,
            Some("protocol_filter run deadline expired"),
        ),
    ] {
        let command = TimeoutFilter {
            calls: 0.into(),
            failures,
            phase,
            error: ChildFailure::CommandTimeout,
            stopped,
            reject_after_timeouts: false,
        };
        let snapshot = ego_core::generator::filter_state::FilterStateSnapshot {
            session_id: Default::default(),
            turn_index: 1,
            state: Default::default(),
        };
        let request = ego_core::egress::EgressRequest {
            direction: "mcp",
            destination: "fixture/tool",
            payload: serde_json::json!({"text":"unchanged"}),
            turn: None,
            scope: None,
        };
        let (events, _notices) = futures::channel::mpsc::unbounded();
        let result = ego_ai::protocol_filter::apply_egress(
            &command,
            ("fixture", "model"),
            &snapshot,
            request,
            &events,
        )
        .await;
        assert_eq!(
            result.err(),
            expected_error,
            "{phase}, {failures}, {stopped:?}"
        );
        assert_eq!(
            command.calls.load(std::sync::atomic::Ordering::SeqCst),
            expected_calls,
            "{phase}"
        );
    }
}

// Catches: timed-out process notices exhausting the final rejection's notice budget.
#[tokio::test]
async fn protocol_filter_timeout_notices_do_not_hide_final_policy_reason() {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let command = Arc::new(TimeoutFilter {
        calls: 0.into(),
        failures: 2,
        phase: "request",
        error: ChildFailure::CommandTimeout,
        stopped: None,
        reject_after_timeouts: true,
    });
    let generator =
        generator(http.clone()).with_protocol_filter(protocol_config(), command.clone());
    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &request());
    let mut stream = generator.generate(request(), attempt);
    let events = observe_filtered(&mut stream, None).await;
    assert_eq!(command.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(http.calls(), 0);
    let notices: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AiEvent::FilterNotice { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        notices,
        ["retry fixture A", "retry fixture B", "final policy reason"]
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AiEvent::Error {
            kind: ErrorKind::PolicyRejected,
            ..
        }
    )));
    stream.settle(&mut gate).unwrap();
}

// Catches: successful filtering reclassifying an uncertain provider exchange as
// definitely unsent, making an automatic duplicate request appear safe.
#[tokio::test]
async fn protocol_filter_preserves_uncertain_delivery_after_dispatch() {
    use ego_core::permit::TransportFailure;
    let http = StubHttp::new(Err(TransportError::possibly_sent(
        TransportFailure::Exchange("connection lost after write".into()),
    )));
    let command = Arc::new(TestProtocolFilter {
        calls: 0.into(),
        result: Ok(br#"{"version":1,"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{}}"#.to_vec()),
    });
    let generator =
        generator(http.clone()).with_protocol_filter(protocol_config(), command.clone());
    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &request());
    let mut stream = generator.generate(request(), attempt);
    observe_filtered(&mut stream, None).await;
    let settlement = stream.settle(&mut gate).unwrap();
    assert_eq!(
        settlement.outcome.delivery(),
        DeliveryCertainty::PossiblySent
    );
    assert!(!settlement.outcome.retry_is_safe());
    assert_eq!(command.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(http.calls(), 1);
    assert_eq!(
        http.seen.lock().unwrap()[0].body.as_deref(),
        Some(b"{}".as_slice())
    );
}

// Catches: hashing the unfiltered cache prefix or publishing an identity for bytes never dispatched.
#[tokio::test]
async fn protocol_filter_cache_identity_follows_final_bytes() {
    use base64::Engine as _;
    use ego_ai::wire::anthropic_messages::AnthropicMessages;
    use ego_core::event::id::SessionId;
    let http = StubHttp::responding(200, "");
    let row = ProviderRow::anthropic(Url::parse("https://api.example/").unwrap());
    let credential = row
        .credential
        .resolve(&FakeCredentials::holding(KEY))
        .unwrap();
    let body = br#"{ "model":"filtered-model", "system":[{"type":"text","text":"filtered stable prefix","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"x"}] }"#;
    let output = serde_json::to_vec(&serde_json::json!({"version":1,
        "body_base64":base64::engine::general_purpose::STANDARD.encode(body),"identity_headers":[],"expected_revision":0,"state_writes":{}})).unwrap();
    let command = Arc::new(TestProtocolFilter {
        calls: 0.into(),
        result: Ok(output),
    });
    let generator = WireGenerator::new(row, &AnthropicMessages, http.clone(), credential)
        .with_protocol_filter(protocol_config(), command);
    let mut req = request();
    let session = SessionId::new();
    req.cache = Some(ego_core::generator::types::PromptCache {
        verifier: None,
        session_id: session,
        lineage_root: session,
        retention: CacheRetention::Short,
        stable_messages: 0,
    });
    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &req);
    let mut stream = generator.generate(req, attempt);
    assert!(stream.cache_prefix().is_none());
    observe_filtered(&mut stream, Some(session)).await;
    let prefix = stream.cache_prefix().unwrap();
    assert_eq!(prefix.session, session);
    // Independently calculated from the insertion-order JSON prefix with messages=[]:
    // SHA-256, not the production cache_prefix helper as its own oracle.
    assert_eq!(
        prefix.digest,
        "cb81975d1351d0e9058ed9afd1ffa0337df5ca2f85e19c7ebab6f2901e7d989e"
    );
    assert_eq!(
        http.seen.lock().unwrap()[0].body.as_deref(),
        Some(body.as_slice())
    );
    stream.settle(&mut gate).unwrap();
}

struct InboundFailureCommand(&'static str);
impl ego_ai::protocol_filter::ProtocolFilterCommand for InboundFailureCommand {
    fn open_response(
        &self,
    ) -> Result<Box<dyn ego_ai::protocol_filter::ProtocolFilterResponse>, ChildFailure> {
        Ok(Box::new(InboundFailureResponse(self.0)))
    }
}
struct InboundFailureResponse(&'static str);
impl ego_ai::protocol_filter::ProtocolFilterResponse for InboundFailureResponse {
    fn exchange(&mut self, input: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, ChildFailure>> {
        let input: serde_json::Value = serde_json::from_slice(&input).unwrap();
        if input["direction"] == "hello" {
            return Box::pin(futures::future::ready(Ok(
                br#"{"version":1,"request":true,"response":true}"#.to_vec(),
            )));
        }
        if input["direction"] == "request" {
            return Box::pin(futures::future::ready(Ok(br#"{"version":1,"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{}}"#.to_vec())));
        }
        let mut output = serde_json::json!({"version":1,"sequence":input["sequence"],"body_base64":input["body_base64"]});
        let result = match self.0 {
            "late failure" if input["eof"] == true => Err(ChildFailure::InvalidResult),
            "policy rejection" if input["eof"] == true => Ok(serde_json::to_vec(&serde_json::json!({
                "version":1,"reject":true,"sequence":input["sequence"],"notices":["local policy"],
            })).unwrap()),
            "sequence" => {
                output["sequence"] = serde_json::json!(999);
                Ok(serde_json::to_vec(&output).unwrap())
            }
            "response writes" => {
                output["state_writes"] = serde_json::json!({"forbidden":"value"});
                Ok(serde_json::to_vec(&output).unwrap())
            }
            _ => Ok(serde_json::to_vec(&output).unwrap()),
        };
        Box::pin(futures::future::ready(result))
    }
    fn finish(&mut self) -> BoxFuture<'_, Result<(), ChildFailure>> {
        Box::pin(futures::future::ready(if self.0 == "cleanup failure" {
            Err(ChildFailure::CleanupUncertain)
        } else {
            Ok(())
        }))
    }
}

// Catches: ignoring local filter failure after a provider terminal marker,
// accepting unsolicited response state writes, or declaring sent HTTP unsent.
#[tokio::test]
async fn protocol_filter_inbound_failures_override_complete_provider_terminal() {
    for mode in [
        "late failure",
        "sequence",
        "response writes",
        "cleanup failure",
        "policy rejection",
    ] {
        let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
        let generator = generator(http.clone())
            .with_protocol_filter(protocol_config(), Arc::new(InboundFailureCommand(mode)));
        let mut gate = BudgetGate::new(usd("10"));
        let attempt = attempt_for(&mut gate, &request());
        let mut stream = generator.generate(request(), attempt);
        let events = observe_filtered(&mut stream, None).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::LocalFailure { .. })),
            "{mode}: {events:?}"
        );
        assert_eq!(
            events.last(),
            Some(&AiEvent::Stop(StopReason::Error)),
            "{mode}: {events:?}"
        );
        if mode == "policy rejection" {
            let notice = events
                .iter()
                .position(|event| matches!(event, AiEvent::FilterNotice { .. }))
                .expect("local rejection notice");
            let error = events
                .iter()
                .position(|event| matches!(event, AiEvent::Error { .. }))
                .expect("policy rejection error");
            assert!(
                notice < error,
                "the diagnostic must precede its terminal error: {events:?}"
            );
            assert!(events.iter().any(|event| matches!(event, AiEvent::FilterNotice { text } if text == "local policy")), "{events:?}");
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    AiEvent::Error {
                        kind: ErrorKind::PolicyRejected,
                        ..
                    }
                )),
                "{events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, AiEvent::TextDelta { .. })),
                "previously published content must remain visible: {events:?}"
            );
        }
        let settlement = stream.settle(&mut gate).unwrap();
        assert_eq!(
            settlement.outcome.delivery(),
            DeliveryCertainty::PossiblySent,
            "{mode}"
        );
        assert_eq!(settlement.outcome.stop(), StopReason::Error, "{mode}");
        assert!(!settlement.outcome.retry_is_safe(), "{mode}");
        assert_eq!(http.calls(), 1, "{mode}");
    }
}

// Catches: truncating a filtered HTTP error before EOF hides final filter
// failure, or treating oversized filtered diagnostics as provider retry advice.
#[tokio::test]
async fn protocol_filter_error_responses_require_bounded_final_validation() {
    for (mode, body) in [
        ("late failure", "provider refused".to_owned()),
        ("pass", "x".repeat(65_537)),
    ] {
        let http = StubHttp::responding(429, &body);
        let generator = generator(http.clone())
            .with_protocol_filter(protocol_config(), Arc::new(InboundFailureCommand(mode)));
        let mut gate = BudgetGate::new(usd("10"));
        let attempt = attempt_for(&mut gate, &request());
        let mut stream = generator.generate(request(), attempt);
        let events = observe_filtered(&mut stream, None).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::LocalFailure { .. })),
            "{mode}: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::ProviderFailure { .. })),
            "{mode}: {events:?}"
        );
        assert_eq!(events.last(), Some(&AiEvent::Stop(StopReason::Error)));
        let settlement = stream.settle(&mut gate).unwrap();
        assert_eq!(
            settlement.outcome.delivery(),
            DeliveryCertainty::PossiblySent
        );
        assert_eq!(settlement.outcome.stop(), StopReason::Error);
        assert!(!settlement.outcome.retry_is_safe());
        assert_eq!(http.calls(), 1);
    }
}

// Catches: hello reporting a default main role or provider ID rather than the
// actual resolved role and encoder, or leaking new metadata into body frames.
#[tokio::test]
async fn protocol_filter_hello_reports_bound_role_and_actual_wire_only() {
    struct ContextFilter;
    impl ego_ai::protocol_filter::ProtocolFilterCommand for ContextFilter {
        fn open_response(
            &self,
        ) -> Result<Box<dyn ego_ai::protocol_filter::ProtocolFilterResponse>, ChildFailure>
        {
            Ok(Box::new(ContextFilter))
        }
    }
    impl ego_ai::protocol_filter::ProtocolFilterResponse for ContextFilter {
        fn exchange(&mut self, input: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, ChildFailure>> {
            let input: serde_json::Value = serde_json::from_slice(&input).unwrap();
            assert_eq!(input["direction"], "hello");
            assert_eq!(input["generation_role"], "hook");
            assert_eq!(input["wire_format"], "OpenAiCompletions");
            assert_eq!(input.as_object().unwrap().len(), 10);
            Box::pin(futures::future::ready(Ok(
                br#"{"version":1,"request":false,"response":false}"#.to_vec(),
            )))
        }
        fn finish(&mut self) -> BoxFuture<'_, Result<(), ChildFailure>> {
            Box::pin(futures::future::ready(Ok(())))
        }
    }
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = generator(http.clone())
        .with_generation_role(ego_core::event::domain::GenerationRole::Hook)
        .with_protocol_filter(protocol_config(), Arc::new(ContextFilter));
    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &request());
    let mut stream = generator.generate(request(), attempt);
    let events = observe_filtered(&mut stream, None).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AiEvent::TextDelta { .. }))
    );
    assert_eq!(http.calls(), 1);
}

fn filter_error_message(events: &[AiEvent]) -> String {
    let mut messages = events.iter().filter_map(|event| match event {
        AiEvent::Error { message, .. } => Some(message.clone()),
        _ => None,
    });
    let message = messages.next().expect("one filter error");
    assert_eq!(messages.next(), None, "exactly one error: {events:?}");
    message
}

async fn filter_failure_events(
    command: Arc<dyn ego_ai::protocol_filter::ProtocolFilterCommand>,
    acks: usize,
) -> (Vec<AiEvent>, Arc<StubHttp>) {
    let http = StubHttp::responding(200, TEXT_TRANSCRIPT);
    let generator = generator(http.clone()).with_protocol_filter(protocol_config(), command);
    let mut gate = BudgetGate::new(usd("10"));
    let attempt = attempt_for(&mut gate, &request());
    let mut stream = generator.generate(request(), attempt);
    (observe_filtered_with(&mut stream, None, acks).await, http)
}

struct OpenFails(ChildFailure);
impl ego_ai::protocol_filter::ProtocolFilterCommand for OpenFails {
    fn open_response(
        &self,
    ) -> Result<Box<dyn ego_ai::protocol_filter::ProtocolFilterResponse>, ChildFailure> {
        Err(self.0)
    }
}

// Catches: an empty filtered request body reported as "output limit", which
// tells the operator to return a smaller result when the result was too small.
#[tokio::test]
async fn protocol_filter_empty_request_body_is_an_invalid_result() {
    let command = Arc::new(TestProtocolFilter {
        calls: 0.into(),
        result: Ok(br#"{"version":1,"body_base64":"","identity_headers":[],"expected_revision":0,"state_writes":{}}"#.to_vec()),
    });
    let (events, http) = filter_failure_events(command, usize::MAX).await;
    assert_eq!(
        filter_error_message(&events),
        "protocol_filter request: invalid result (empty request body; check the filter result against the V1 contract)"
    );
    assert_eq!(http.calls(), 0);
}

// Catches: a missing journal acknowledgement (host fault) reported as an
// invalid filter result that sends the operator to debug a correct filter.
#[tokio::test]
async fn protocol_filter_host_journal_failure_does_not_blame_the_filter() {
    let command = Arc::new(TestProtocolFilter {
        calls: 0.into(),
        result: Ok(br#"{"version":1,"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{}}"#.to_vec()),
    });
    // Serve the namespace read, drop the state save.
    let (events, http) = filter_failure_events(command, 1).await;
    let message = filter_error_message(&events);
    assert_eq!(
        message,
        "protocol_filter request: host (journal acknowledgement missing; check the local session journal, the filter is not at fault)"
    );
    assert_eq!(http.calls(), 0);
}

// Catches: a hello failure labelled "request", and a spawn failure losing
// its "launch" label; both misdirect the operator to the wrong exchange.
#[tokio::test]
async fn protocol_filter_hello_and_launch_failures_name_their_phase() {
    let hello = Arc::new(TimeoutFilter {
        calls: 0.into(),
        failures: 1,
        phase: "hello",
        error: ChildFailure::Io,
        stopped: None,
        reject_after_timeouts: false,
    });
    let (events, http) = filter_failure_events(hello, usize::MAX).await;
    assert_eq!(
        filter_error_message(&events),
        "protocol_filter hello: io (check the filter's stdin and stdout handling)"
    );
    assert_eq!(http.calls(), 0);
    let (events, http) =
        filter_failure_events(Arc::new(OpenFails(ChildFailure::Spawn)), usize::MAX).await;
    assert_eq!(
        filter_error_message(&events),
        "protocol_filter launch: spawn (check that the configured filter is executable)"
    );
    assert_eq!(http.calls(), 0);
}

// Catches: egress diagnostics hard-coding the request phase when the filter
// failed to start, so nothing was exchanged.
#[tokio::test]
async fn protocol_filter_egress_launch_failure_is_not_labelled_request() {
    let snapshot = ego_core::generator::filter_state::FilterStateSnapshot {
        session_id: Default::default(),
        turn_index: 1,
        state: Default::default(),
    };
    let request = ego_core::egress::EgressRequest {
        direction: "mcp",
        destination: "fixture/tool",
        payload: serde_json::json!({"text":"unchanged"}),
        turn: None,
        scope: None,
    };
    let (events, _notices) = futures::channel::mpsc::unbounded();
    let result = ego_ai::protocol_filter::apply_egress(
        &OpenFails(ChildFailure::Io),
        ("fixture", "model"),
        &snapshot,
        request,
        &events,
    )
    .await;
    assert_eq!(
        result.err(),
        Some("protocol_filter launch: io (check the filter's stdin and stdout handling)")
    );
}

// Catches: a response-phase filter failure journaling the decoder's own
// "stream ended" provider error next to the filter error.
#[tokio::test]
async fn protocol_filter_response_failure_journals_only_the_filter_error() {
    let command = Arc::new(TimeoutFilter {
        calls: 0.into(),
        failures: 1,
        phase: "response_only",
        error: ChildFailure::Io,
        stopped: None,
        reject_after_timeouts: false,
    });
    let (events, _http) = filter_failure_events(command, usize::MAX).await;
    assert!(
        filter_error_message(&events).starts_with("protocol_filter response:"),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AiEvent::ProviderFailure { .. })),
        "{events:?}"
    );
}
