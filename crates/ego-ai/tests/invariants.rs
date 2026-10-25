//! The two invariants of the provider seam, proved from outside it.
//!
//! `ARCHITECTURE.md` §5
//! states three, and each has a *necessary* condition and a *sufficient* one.
//! The necessary one — that the near side names no provider and no protocol —
//! is a grep, and it lives in `crates/ego-core/tests/near_side.rs` because the
//! source it reads is that crate's. This file is the sufficient one: **a
//! stranger can do it from outside.**
//!
//! Outside is what makes these evidence rather than decoration. An integration
//! test in `ego-ai` reaches only public API, and `ego-ai` depends on `ego-core`
//! and never the reverse — so if adding a provider or a protocol required
//! touching a private detail of `budget.rs` or `turn/`, the test below would
//! not compile, and could not be made to compile from here at all.
//!
//! - **I1** adds a provider on an already-supported dialect, and may change
//!   nothing outside configuration. Its test builds a row for a provider this
//!   build has never heard of, out of public API only.
//! - **I2** adds a genuinely new protocol, and may change only the adapter and
//!   its registration. Its test defines a [`WireFormat`] *here*, for a wire
//!   that resembles none of the three: its own framing, its own error shape,
//!   and a usage report that is cumulative rather than incremental.
//!
//! Both then drive one whole generation through `TurnLoop` and require a
//! settled ending — a priced turn with a real total. Reaching the events is
//! not the claim; **settling** is, because that is the part that runs through
//! the budget gate and the ledger, which is the code I1 and I2 say must not
//! have to know who it is serving.
//!
//! I3 is proved the same way and in `tests/provider_cost.rs`, not here,
//! because its subject is a decoder rather than a whole turn. This paragraph
//! used to say I3 was untestable — that a second provider-reported cost
//! decoder would be "a second row in one adapter's field→unit table" and so
//! not a second decoder at all. There is no such table: `ProviderCost` is a
//! trait, each decoder is a self-contained value a row carries, and
//! `TollgateStated` at the bottom of that file is a third one written outside
//! `ego-ai`'s `src/` for exactly the reason I1 and I2 are written outside
//! `ego-core`.

use std::sync::{Arc, Mutex};

use ego_ai::catalogue::{self, Catalogue, HostedModel, ModelSlug, SuppliedAuthority};
use ego_ai::generator::WireGenerator;
use ego_ai::http::{Http, HttpCall, HttpError, HttpRequest, HttpResponse, PreparedHttpCall};
use ego_ai::provider::{CredentialStrategy, ProviderRow};
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_ai::wire::provider_cost::ProviderCost;
use ego_ai::wire::{RequestRefused, RequestTarget, WireFormat};
use ego_core::budget::{BudgetGate, PricingQuote, SharedBudgetGate};
use ego_core::cost::{Amount, CumulativeCost, CurrencyCode, Money, PricingRevision};
use ego_core::event::domain::{DomainEvent, ExitReason};
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{
    EvidenceAdmissionService, EvidenceError as AdmissionError, EvidenceKey, EvidenceRecord,
    EvidenceStore as AdmissionStore, EvidenceWrite, RunInvocationId,
};
use ego_core::generation_model_source::{
    GenerationModelError, GenerationModelRequest, GenerationModelSource, GenerationProfile,
};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ErrorKind, GenRequest, StopReason, ThinkingLevel};
use ego_core::permit::fake::FakeFs;
use ego_core::permit::{CanonPath, ProcessError, ProcessExecutor, SpawnSpec, Tool, ToolResult};
use ego_core::permit::{HttpHeader, OneHopResponse, TransportError, TransportFailure};
use ego_core::run::{RunAppend, RunRevision, RunTransaction};
use ego_core::sink::NullSink;
use ego_core::state::{ItemKind, SessionState, Usage};
use ego_core::store::blob::{BlobError, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::turn::{
    CancelToken, GenerationRuntime, Session, ToolEffectPorts, Tools, TurnConfig, TurnLoop,
};
use ego_core::turn::{GenerationPolicyCapture, GenerationPolicySnapshot, GenerationPolicySource};
use futures::StreamExt;
use futures::future::{self, BoxFuture};
use futures::stream::BoxStream;
use serde::Deserialize;
use url::Url;

mod support;
use support::{FakeCredentials, NoForeign};

/// The openai-completions recording the family is already tested against.
///
/// Reused deliberately: I1 is the claim that a *new provider* on a supported
/// dialect needs no new code, and a fixture minted for the occasion would
/// quietly weaken it into "a new provider and a new transcript".
const FAMILY_TRANSCRIPT: &str = include_str!("goldens/chat_completions/stream/text.sse");

/// The key a configured machine would have put in the fake credentials.
const KEY: &str = "sk-not-a-real-key";

// --- the near side's fixtures ---

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
        uncached_input: Some("0.000003".parse().expect("a valid rate")),
        cache_read: Some("0.0000003".parse().expect("a valid rate")),
        cache_write: Some("0.00000375".parse().expect("a valid rate")),
        cache_write_extended: None,
        output: Some("0.000015".parse().expect("a valid rate")),
    }
}

fn caps() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        max_tools: Some(128),
        supports_parallel_tool_calls: false,
        reasoning_carryover: ReasoningCarryover::Unsupported,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

/// A hosted model priced from a table the operator supplied — the arrangement
/// that makes a turn settle at a *number*, which is what both tests assert on.
fn hosted() -> HostedModel {
    HostedModel {
        authority: SuppliedAuthority::System,
        capabilities: caps(),
        metering: catalogue::tabulated(),
        rates: Some(catalogue::operator_rates(quote())),
    }
}

// --- the far side's doubles ---

/// A `SessionStore` that keeps the log in memory.
#[derive(Default)]
struct MemStore(Mutex<Vec<DomainEvent>>);

impl SessionStore for MemStore {
    fn append(&self, _id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError> {
        self.0.lock().expect("not poisoned").push(ev.clone());
        Ok(())
    }

    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _expected_revision: Option<RunRevision>,
        _transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this event-only fixture does not accept run transactions",
        )))
    }

    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        unimplemented!("these tests only append")
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        unimplemented!("these tests only append")
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(1_787_000_000_000)
    }
}

struct NoProcess;

impl ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(future::ready(Err(ProcessError::Invariant(
            "provider invariant tests have no process tools".to_string(),
        ))))
    }
}

struct NoEvidence;

impl BlobStore for NoEvidence {
    fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError> {
        Err(BlobError::Limit { limit: 0 })
    }

    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        Err(BlobError::NotFound {
            reference: reference.id.clone(),
        })
    }

    fn read_page<'a>(
        &'a self,
        request: &'a BlobRead,
    ) -> BoxFuture<'a, Result<BlobPage, BlobError>> {
        Box::pin(future::ready(Err(BlobError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }

    fn collect_garbage(&self, _reachable: &[BlobRef]) -> Result<usize, BlobError> {
        Ok(0)
    }
}

struct SingleGenerationSource {
    profile: GenerationProfile,
}

impl GenerationModelSource for SingleGenerationSource {
    fn resolve<'a>(
        &'a self,
        _request: GenerationModelRequest,
    ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
        let profile = GenerationProfile {
            output_limit: None,
            identity: self.profile.identity.clone(),
            request_model: self.profile.request_model.clone(),
            thinking: self.profile.thinking,
            generator: self.profile.generator.clone(),
            capabilities: self.profile.capabilities.clone(),
            metering: self.profile.metering.clone(),
            rates: self.profile.rates.clone(),
        };
        Box::pin(future::ready(Ok(profile)))
    }
}

impl EvidenceStore for NoEvidence {
    fn blob_store(&self) -> &dyn BlobStore {
        self
    }

    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        Err(EvidenceError::Limit { limit: 0 })
    }

    fn read_page<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(future::ready(Err(EvidenceError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }
}

impl AdmissionStore for NoEvidence {
    fn write(
        &self,
        _key: EvidenceKey,
        _record: EvidenceRecord,
    ) -> Result<EvidenceWrite, AdmissionError> {
        Ok(EvidenceWrite::Published)
    }
}

/// Answers one request with one canned body, in small chunks.
///
/// Chunked at a size that falls inside every frame of both transcripts, so a
/// mapping that assumed a frame arrives whole fails here rather than against a
/// real socket.
struct StubHttp(Arc<Mutex<Option<String>>>);

impl StubHttp {
    fn replaying(body: impl Into<String>) -> Arc<Self> {
        Arc::new(StubHttp(Arc::new(Mutex::new(Some(body.into())))))
    }
}

impl Http for StubHttp {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(StubCall(Arc::clone(&self.0)))
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
            let Some(body) = body else {
                return Err(TransportError::definitely_unsent(
                    TransportFailure::Exchange("the stub was asked twice".to_owned()),
                ));
            };
            let chunks: Vec<Result<Vec<u8>, TransportError>> = body
                .as_bytes()
                .chunks(7)
                .map(|chunk| Ok(chunk.to_vec()))
                .collect();
            Ok(OneHopResponse::streaming_response(
                200,
                vec![
                    HttpHeader::new("content-type", b"application/octet-stream".to_vec())
                        .expect("fixture header"),
                ],
                Box::pin(futures::stream::iter(chunks)),
            ))
        })
    }
}

// --- the harness ---

struct FixedPolicySource;

impl GenerationPolicySource for FixedPolicySource {
    fn current(&self) -> Result<Arc<ego_core::permit::PolicyEngine>, String> {
        Ok(Arc::new(ego_core::permit::PolicyEngine::new(
            CanonPath::new("/ws"),
            ego_core::config::Mode::Default,
            ego_core::config::Ruleset::default(),
        )))
    }

    fn capture(&self) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::new(
            CanonPath::new("/ws"),
            ego_core::config::Mode::Default,
            ego_core::config::Ruleset::default(),
            None,
        )
        .map_err(|error| error.to_string())
    }

    fn restore(
        &self,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::restore(snapshot, None).map_err(|error| error.to_string())
    }
}

/// One whole turn against `row`, over a transport replaying `transcript`.
///
/// Everything a `TurnLoop` borrows is built here and dropped here, which is
/// the only reason this is a function and not a fixture struct: both tests
/// want exactly one turn and nothing between them.
async fn one_turn(
    slug: &str,
    row: ProviderRow,
    wire: &'static dyn WireFormat,
    transcript: impl Into<String>,
) -> (ExitReason, SessionState) {
    let (outcome, state) = turn_over_http(
        slug,
        row,
        wire,
        StubHttp::replaying(transcript),
        0,
        Arc::new(NoRetryPause),
        TurnFixture::default(),
    )
    .await;
    (outcome.exit_reason, state)
}

#[derive(Default)]
struct TurnFixture {
    registry: Vec<Arc<dyn Tool>>,
    tier: Option<ego_ai::service_tier::ServiceTier>,
}

async fn turn_over_http(
    slug: &str,
    row: ProviderRow,
    wire: &'static dyn WireFormat,
    http: Arc<dyn Http>,
    retry: u32,
    retry_pause: Arc<dyn ego_core::turn::RetryPause>,
    fixture: TurnFixture,
) -> (ego_core::turn::TurnOutcome, SessionState) {
    let TurnFixture { registry, tier } = fixture;
    turn_over_http_with_question(
        slug,
        row,
        wire,
        http,
        retry,
        retry_pause,
        (
            registry
                .into_iter()
                .map(ego_core::turn::RegisteredTool::Effect)
                .collect(),
            None,
            tier,
        ),
    )
    .await
}

async fn turn_over_http_with_question(
    slug: &str,
    row: ProviderRow,
    wire: &'static dyn WireFormat,
    http: Arc<dyn Http>,
    retry: u32,
    retry_pause: Arc<dyn ego_core::turn::RetryPause>,
    question: (
        Vec<ego_core::turn::RegisteredTool>,
        Option<Arc<dyn ego_core::question::Elicitor>>,
        Option<ego_ai::service_tier::ServiceTier>,
    ),
) -> (ego_core::turn::TurnOutcome, SessionState) {
    let (registry, elicitor, tier) = question;
    let slug = ModelSlug::parse(slug).expect("a constant this file controls");
    // Through the catalogue rather than around it: "configuration" is the
    // claim, and a row handed straight to the generator would prove only that
    // the generator is generic.
    let catalogue = Catalogue::new([row]).with_hosted(&slug.to_string(), hosted());
    let mut resolved = catalogue
        .resolve_hosted(&slug, wire)
        .expect("the row and its model were both configured above");
    if tier.is_some() {
        // Recorded GPT-6.1 Sol USD tariff, not a value computed by the policy under test.
        let rates = resolved.rates.as_mut().unwrap();
        rates.quote.uncached_input = Some("0.000002".parse().unwrap());
        rates.quote.cache_read = Some("0.0000001".parse().unwrap());
        rates.quote.cache_write = Some("0.0000025".parse().unwrap());
        rates.quote.output = Some("0.00001".parse().unwrap());
    }
    let tier = tier.unwrap_or_default();
    if tier == ego_ai::service_tier::ServiceTier::Fast {
        let support = ego_ai::service_tier::fast_support(&slug.to_string()).unwrap();
        let rates = resolved.rates.as_mut().unwrap();
        rates.quote = ego_ai::service_tier::fast_quote(&rates.quote, &support).unwrap();
    }
    let credential = resolved
        .row
        .credential
        .resolve(&FakeCredentials::holding(KEY))
        .expect("every variable is set on this fake machine");

    let generator = Arc::new(
        WireGenerator::new(resolved.row.clone(), resolved.wire, http, credential)
            .with_service_tier(tier),
    );
    let store = MemStore::default();
    let clock = FixedClock;
    let fs = FakeFs::default();
    let process = NoProcess;
    let evidence = NoEvidence;
    let foreign: Arc<NoForeign> = Arc::new(NoForeign);
    let gate = SharedBudgetGate::new(BudgetGate::new(usd("10")));

    let mut loop_ = TurnLoop::new(
        Session {
            id: SessionId::new(),
            state: SessionState::default(),
            store: &store,
            evidence: &evidence,
            clock: &clock,
            sink: &NullSink,
            cancel: CancelToken::new(),
        },
        GenerationRuntime {
            source: Arc::new(SingleGenerationSource {
                profile: GenerationProfile {
                    output_limit: None,
                    identity: ego_core::config::ModelRef(slug.to_string()),
                    request_model: resolved.model.clone(),
                    thinking: ThinkingLevel::Off,
                    generator,
                    capabilities: resolved.capabilities.clone(),
                    metering: resolved.metering.clone(),
                    rates: resolved.rates.clone(),
                },
            }),
            policy_source: Arc::new(FixedPolicySource),
            gate,
            config: TurnConfig {
                cache: ego_core::generator::capabilities::CacheRetention::Short,
                system: None,
                identity: None,
                token_ceiling: None,
                retry,
                fallbacks: Vec::new(),
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
            retry_pause,
        },
        Tools::from_registry(
            &registry,
            ToolEffectPorts::new(
                &fs,
                &process,
                &evidence,
                foreign.clone(),
                foreign.clone(),
                foreign,
            ),
            CanonPath::new("/ws"),
        ),
    )
    .expect("budget gate is available");

    if let Some(elicitor) = elicitor {
        loop_ = loop_.with_elicitor(elicitor);
    }

    let outcome = loop_
        .run("list the files")
        .await
        .expect("the store accepted every append");
    (
        outcome,
        loop_.into_state().expect("journal ownership is released"),
    )
}

/// The assistant text this session ended up holding.
fn answer(state: &SessionState) -> String {
    state
        .items
        .values()
        .filter(|item| item.kind == ItemKind::Assistant)
        .max_by_key(|item| item.seq)
        .map(|item| item.text.clone())
        .unwrap_or_default()
}

/// What both tests require of a successful ending, so neither can drift from
/// the other.
///
/// The money is the load-bearing part: a total that stayed `Unstarted` would
/// mean nothing reached the ledger, and an `Unknown` would mean the gate could
/// not price what came back — either of which is the seam failing quietly
/// while every event still looked right.
fn settled(exit: ExitReason, state: &SessionState, usage: Usage, total: &str) {
    assert_eq!(exit, ExitReason::Completed);
    assert_eq!(answer(state), "Due file — perché no? 😀");
    assert_eq!(state.usage, usage);
    assert_eq!(state.cost, CumulativeCost::Known { total: usd(total) });
}

// --- I1: a provider on a dialect this build already speaks ---

/// A provider nobody wrote a line of code for generates, settles and is
/// priced.
///
/// The row is constructor-backed, with an operator-supplied endpoint and the
/// OpenAI mapping. Nothing in the core path branches on the provider name.
#[tokio::test]
async fn a_provider_on_a_supported_dialect_is_configuration_and_nothing_else() {
    let acme =
        ProviderRow::openai_at(Url::parse("https://api.acme.example/v1/").expect("a valid url"));

    let (exit, state) = one_turn(
        "openai/wonder-1",
        acme,
        &OpenAiCompletions,
        FAMILY_TRANSCRIPT,
    )
    .await;

    settled(
        exit,
        &state,
        Usage {
            uncached_input_tokens: 500,
            cache_read_tokens: 400,
            cache_write_tokens: 100,
            cache_write_extended_tokens: 0,
            output_tokens: 250,
            web_search_requests: 0,
            reasoning_tokens: Some(180),
        },
        "0.005745",
    );
}

// --- I2: a protocol nobody designed `AiEvent` against ---

/// A wire that deliberately resembles none of the three.
///
/// Three differences, each chosen because it is the kind of thing that would
/// force a change in shared code if the seam were in the wrong place:
///
/// - **Framing.** Length-prefixed frames — `<byte count>|<json>` — not
///   `data:` lines. Nothing here may touch [`ego_ai::wire::sse`], and nothing
///   does.
/// - **Errors.** A `halt` frame carrying the provider's own retryability flag,
///   arriving mid-stream with a `200` still on the status line — rather than a
///   status code and the body shape the family shares.
/// - **Usage.** Reported *cumulatively*: every frame restates the running
///   totals, in a partition of its own where the prompt figure *includes* what
///   the cache served. [`AiEvent::Usage`] is an increment in a disjoint
///   partition, so both the buffering and the subtraction are this mapping's
///   obligation — which is exactly what the port's doc comment says, said to a
///   mapping that did not exist when it was written.
#[derive(Debug)]
struct Beacon;

/// The path this stranger serves, joined onto its row's base like any other.
const BEACON_PATH: &str = "beacon/v1/emit";

/// One frame of the stranger's stream.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Frame {
    /// Visible text, with the running totals as of this frame.
    Say { text: String, seen: Totals },
    /// The provider gave up. Its own vocabulary, not anybody else's.
    Halt { why: String, permanent: bool },
    /// A clean close, with the final totals.
    Done { seen: Totals },
}

/// The stranger's usage report: cumulative, and partitioned its own way.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct Totals {
    /// Everything the prompt cost, cache included — the *inclusive* direction
    /// [`Usage::uncached_input_tokens`] documents as the one a mapping has to
    /// subtract from rather than pass through.
    prompt: u64,
    from_cache: u64,
    emitted: u64,
}

impl WireFormat for Beacon {
    fn build_request(
        &self,
        target: &RequestTarget<'_>,
        req: &GenRequest,
    ) -> Result<HttpRequest, RequestRefused> {
        // A body of this wire's own devising. `post_json` is shared because
        // "JSON in, events out" is not what makes a protocol a stranger; the
        // framing of the answer is.
        let body = serde_json::json!({
            "beacon": 1,
            "engine": req.model,
            "history": req.messages.messages(),
        });
        Ok(target.post_json(BEACON_PATH, serde_json::to_vec(&body).expect("plain JSON")))
    }

    fn decode(
        &self,
        response: Result<HttpResponse, HttpError>,
        _cost: Option<Arc<dyn ProviderCost>>,
    ) -> BoxStream<'static, AiEvent> {
        Box::pin(futures::stream::once(decode_beacon(response)).flat_map(futures::stream::iter))
    }
}

/// The whole body, then the events — the simplest decoder that can be wrong in
/// the ways this test is about.
///
/// Buffering the response is a real limitation and an honest one for a wire
/// with no implementation behind it: what I2 asks is whether [`AiEvent`] is
/// sufficient vocabulary, not whether a test can write a streaming parser.
async fn decode_beacon(response: Result<HttpResponse, HttpError>) -> Vec<AiEvent> {
    let response = match response {
        Ok(response) => response,
        Err(err) => {
            return ego_ai::wire::failed(err.to_string(), err.delivery(), ErrorKind::Unclassified)
                .to_vec();
        }
    };
    let mut body = Vec::new();
    let mut chunks = response.body;
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(bytes) => body.extend_from_slice(&bytes),
            Err(err) => {
                return ego_ai::wire::failed(
                    err.to_string(),
                    err.delivery(),
                    ErrorKind::Unclassified,
                )
                .to_vec();
            }
        }
    }

    let mut out = Vec::new();
    // The latest restatement, held rather than forwarded. This is the port's
    // own instruction to a mapping over a cumulative wire — "buffer, and emit
    // at a clean close, a later report replacing the held one rather than
    // adding to it" — and holding it (rather than discarding every `say`
    // figure and reading only `done`) is what lets a *truncated* stream still
    // settle against a real number instead of a pessimistic bound.
    let mut held = Totals::default();
    let mut rest = body.as_slice();
    while let Some(cut) = rest.iter().position(|byte| *byte == b'|') {
        let len: usize = std::str::from_utf8(&rest[..cut])
            .expect("a frame length is ASCII")
            .parse()
            .expect("this wire states its own frame lengths");
        let (frame, tail) = rest[cut + 1..].split_at(len);
        rest = tail;
        match serde_json::from_slice::<Frame>(frame).expect("this transcript is well formed") {
            // Cumulative in, incremental out: a `say` frame's totals replace
            // what is held and nothing is emitted for them. Forwarding each
            // one would bill the sum of the restatements.
            Frame::Say { text, seen } => {
                held = seen;
                out.push(AiEvent::TextDelta { text });
            }
            Frame::Halt { why, permanent } => {
                let kind = if permanent {
                    ErrorKind::Invalid
                } else {
                    ErrorKind::Transient
                };
                // What was billed up to the halt was still billed.
                out.push(AiEvent::Usage(reported(held)));
                // The response arrived, so the request certainly left — and
                // `PossiblySent` is what that has to be spelled as: the
                // certainty this enum grades is "may a retry double-bill",
                // and the answer here is yes.
                out.extend(ego_ai::wire::failed(
                    why,
                    DeliveryCertainty::PossiblySent,
                    kind,
                ));
                return out;
            }
            Frame::Done { seen } => {
                out.push(AiEvent::Usage(reported(seen)));
                out.push(AiEvent::Stop(StopReason::Stop));
                return out;
            }
        }
    }

    // The frames ran out with no terminal one. What was held is still true,
    // so it is reported before the failure — a settled attempt with a real
    // figure rather than one settled against a bound.
    out.push(AiEvent::Usage(reported(held)));
    out.extend(ego_ai::wire::failed(
        "the stream ended without a terminal frame".to_string(),
        DeliveryCertainty::PossiblySent,
        ErrorKind::Unclassified,
    ));
    out
}

/// One of this wire's cumulative reports, in the port's disjoint partition.
fn reported(seen: Totals) -> Usage {
    Usage {
        uncached_input_tokens: seen.prompt.saturating_sub(seen.from_cache),
        cache_read_tokens: seen.from_cache,
        cache_write_tokens: 0,
        cache_write_extended_tokens: 0,
        output_tokens: seen.emitted,
        // This wire does not separate them out, and inventing a zero would
        // claim it said none.
        web_search_requests: 0,
        reasoning_tokens: None,
    }
}

/// A stream in the stranger's framing, from the frames it carries.
///
/// The lengths are computed rather than written down, which is not a
/// convenience: hand-counting the bytes of `perché no? 😀` is how a fixture
/// ends up testing the arithmetic of whoever wrote it.
fn framed(frames: &[&str]) -> String {
    frames
        .iter()
        .map(|frame| format!("{}|{frame}", frame.len()))
        .collect()
}

/// A recording of nothing: this protocol has no implementation to record.
///
/// Written by hand and said so plainly, because every other transcript in this
/// directory came off a real socket and the difference matters. What it can
/// still prove is the only thing I2 claims — that `AiEvent` had the vocabulary
/// — and that claim is about the *port*, not about a vendor.
fn beacon_transcript() -> String {
    framed(&[
        r#"{"say":{"text":"Due ","seen":{"prompt":900,"from_cache":400,"emitted":1}}}"#,
        r#"{"say":{"text":"file — perché no? 😀","seen":{"prompt":900,"from_cache":400,"emitted":250}}}"#,
        r#"{"done":{"seen":{"prompt":900,"from_cache":400,"emitted":250}}}"#,
    ])
}

/// The stranger's row. A value like any other row, which is the claim.
fn beacon_row() -> ProviderRow {
    let mut row =
        ProviderRow::openai_at(Url::parse("https://beacon.example/v1/").expect("a valid url"));
    row.credential = CredentialStrategy::None;
    row
}

/// A protocol nobody designed `AiEvent` against reaches the same settled end.
///
/// The totals differ from I1's because this wire reports differently — no
/// cache writes, no reasoning split — and the *price* differs with them. That
/// is the point: the ledger priced a report it had never seen the shape of.
#[tokio::test]
async fn a_new_protocol_is_an_adapter_and_its_registration() {
    let (exit, state) = one_turn(
        "openai/lighthouse",
        beacon_row(),
        &Beacon,
        beacon_transcript(),
    )
    .await;

    settled(
        exit,
        &state,
        Usage {
            uncached_input_tokens: 500,
            cache_read_tokens: 400,
            cache_write_tokens: 0,
            cache_write_extended_tokens: 0,
            output_tokens: 250,
            web_search_requests: 0,
            reasoning_tokens: None,
        },
        "0.005370",
    );
}

/// The stranger's *failure* shape also lands, without a status code to read
/// it off.
///
/// A `200` that turns into an error part-way through is the case a mapping
/// keyed on HTTP status cannot express, and it is why `WireFormat::decode`
/// takes the whole response rather than a body. The turn ends as a provider
/// failure, the assistant keeps what it had already said, and the tokens
/// spent up to the halt are still counted.
#[tokio::test]
async fn a_stranger_protocols_own_error_shape_ends_the_turn() {
    let transcript = framed(&[
        r#"{"say":{"text":"Due ","seen":{"prompt":900,"from_cache":400,"emitted":1}}}"#,
        r#"{"halt":{"why":"the beacon is dark","permanent":false}}"#,
    ]);

    let (exit, state) = one_turn("openai/lighthouse", beacon_row(), &Beacon, transcript).await;

    assert_eq!(exit, ExitReason::Provider);
    assert_eq!(answer(&state), "Due ");
    assert_eq!(state.usage.output_tokens, 1);
    assert_eq!(state.usage.cache_read_tokens, 400);
}

/// A stream that stops mid-sentence settles against what it had, not against
/// a bound.
///
/// The case the cumulative report exists for. There is no terminal frame and
/// therefore no final total, and the last restatement the mapping held is
/// still a true statement about what the provider has already billed — so it
/// is reported. A mapping that had forwarded each restatement as an increment
/// would report 251 output tokens here; one that had discarded them all would
/// report none and settle against the reservation's pessimistic ceiling.
#[tokio::test]
async fn a_cut_stream_settles_against_the_last_report_the_mapping_held() {
    let transcript = framed(&[
        r#"{"say":{"text":"Due ","seen":{"prompt":900,"from_cache":400,"emitted":1}}}"#,
        r#"{"say":{"text":"file — perché no? 😀","seen":{"prompt":900,"from_cache":400,"emitted":250}}}"#,
    ]);

    let (exit, state) = one_turn("openai/lighthouse", beacon_row(), &Beacon, transcript).await;

    assert_eq!(exit, ExitReason::Provider);
    assert_eq!(answer(&state), "Due file — perché no? 😀");
    assert_eq!(
        state.usage,
        Usage {
            uncached_input_tokens: 500,
            cache_read_tokens: 400,
            cache_write_tokens: 0,
            cache_write_extended_tokens: 0,
            output_tokens: 250,
            web_search_requests: 0,
            reasoning_tokens: None,
        }
    );
    assert_eq!(
        state.cost,
        CumulativeCost::Known {
            total: usd("0.005370")
        }
    );
}

/// Retries in these tests do not wait on a wall clock.
struct NoRetryPause;

impl ego_core::turn::RetryPause for NoRetryPause {
    fn pause(&self, _delay: std::time::Duration) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(futures::future::ready(()))
    }
}

// regression case: real HTTP framing and body truncation, with no external provider.
const CODEX_TEXT: &str = include_str!("goldens/profile/openai-codex/stream/text.sse");
const CODEX_TOOL: &str = include_str!("goldens/profile/openai-codex/stream/tool_calls.sse");

struct SocketHttp {
    base: String,
    client: reqwest::Client,
}

impl Http for SocketHttp {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(SocketCall {
            base: self.base.clone(),
            client: self.client.clone(),
        })
    }
}

struct SocketCall {
    base: String,
    client: reqwest::Client,
}

impl HttpCall for SocketCall {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let (_, _, _, request) = call.into_parts();
        let mut builder = self
            .client
            .post(format!("{}{}", self.base, request.path()))
            .body(request.body().to_vec());
        for header in request.headers() {
            builder = builder.header(header.name(), header.value());
        }
        Box::pin(async move {
            let response = builder.send().await.map_err(|error| {
                TransportError::possibly_sent(TransportFailure::Exchange(error.to_string()))
            })?;
            let status = response.status().as_u16();
            let body = response.bytes_stream().map(|chunk| {
                chunk.map(|bytes| bytes.to_vec()).map_err(|error| {
                    TransportError::possibly_sent(TransportFailure::ResponseBody(error.to_string()))
                })
            });
            Ok(OneHopResponse::streaming_response(
                status,
                Vec::new(),
                body.boxed(),
            ))
        })
    }
}

#[derive(Default)]
struct RecordedBodyPause(Mutex<Vec<std::time::Duration>>);
impl ego_core::turn::RetryPause for RecordedBodyPause {
    fn pause(&self, delay: std::time::Duration) -> BoxFuture<'static, ()> {
        self.0.lock().unwrap().push(delay);
        Box::pin(future::ready(()))
    }
}

async fn dropped_codex_turn(
    prefix: &str,
    retry: u32,
) -> (
    ego_core::turn::TurnOutcome,
    SessionState,
    Vec<Vec<u8>>,
    Vec<std::time::Duration>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server_seen = seen.clone();
    let prefix = prefix.to_owned();
    let server = tokio::spawn(async move {
        let mut index = 0;
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let size: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + size {
                        break;
                    }
                }
            }
            server_seen.lock().unwrap().push(request);
            let body = if index == 0 { &prefix } else { CODEX_TEXT };
            let size = body.len() + if index == 0 { 128 } else { 0 };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            index += 1;
        }
    });
    let pause = Arc::new(RecordedBodyPause::default());
    let http = Arc::new(SocketHttp {
        base,
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
    });
    let (outcome, state) = turn_over_http(
        "openai/gpt-6.1-sol",
        ProviderRow::openai_at(Url::parse("https://fixture.invalid/backend-api/").unwrap()),
        &ego_ai::wire::openai_codex_responses::OpenAiCodexResponses,
        http,
        retry,
        pause.clone(),
        TurnFixture {
            registry: vec![Arc::new(NeverRunTool)],
            ..Default::default()
        },
    )
    .await;
    server.abort();
    let requests = seen.lock().unwrap().clone();
    let delays = pause.0.lock().unwrap().clone();
    (outcome, state, requests, delays)
}

#[tokio::test]
async fn codex_dropped_body_before_output_retries_without_changing_cache_identity() {
    // Catches: an answered, truncated body is excluded from no-output retries.
    let (outcome, state, requests, delays) =
        dropped_codex_turn(": body will be dropped\n\n", 6).await;
    assert_eq!(outcome.exit_reason, ExitReason::Completed);
    assert_eq!(answer(&state), "ok");
    assert_eq!(requests.len(), 2);
    assert_eq!(delays, [std::time::Duration::from_secs(1)]);
    let identities: Vec<_> = requests
        .iter()
        .map(|request| {
            let end = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let body: serde_json::Value = serde_json::from_slice(&request[end + 4..]).unwrap();
            let headers = String::from_utf8_lossy(&request[..end]);
            let session = headers
                .lines()
                .find_map(|line| line.strip_prefix("session-id: "))
                .unwrap()
                .to_owned();
            assert_eq!(body["prompt_cache_key"], session);
            (body, session)
        })
        .collect();
    assert_eq!(identities[0], identities[1]);
}

#[tokio::test]
async fn codex_dropped_body_after_text_does_not_replay_and_has_uncertain_notice() {
    // Catches: partial output is retried or loses its distinct uncertain failure notice.
    let prefix = CODEX_TEXT
        .split("data: {\"type\":\"response.completed\"")
        .next()
        .unwrap();
    let (outcome, state, requests, delays) = dropped_codex_turn(prefix, 6).await;
    assert_eq!(answer(&state), "ok");
    assert_eq!(requests.len(), 1);
    assert!(delays.is_empty());
    assert!(!outcome.provider_failure.as_ref().unwrap().effect_known);
    match ego_core::run::RunTermination::after_outcome(&outcome) {
        Some(ego_core::run::RunTermination::ProviderUncertain { message }) => {
            assert!(message.contains("automatic retry was suppressed"))
        }
        other => panic!("expected distinct uncertain failure, got {other:?}"),
    }
}

#[tokio::test]
async fn codex_dropped_body_after_unrun_tool_bytes_retries_without_executing_partial_call() {
    // Catches: a parsed-but-unrun tool still blocks recovery, or executes its partial arguments.
    let prefix = CODEX_TOOL
        .split("data: {\"type\":\"response.completed\"")
        .next()
        .unwrap();
    let (outcome, state, requests, delays) = dropped_codex_turn(prefix, 6).await;
    assert_eq!(outcome.exit_reason, ExitReason::Completed);
    assert_eq!(answer(&state), "ok");
    assert_eq!(requests.len(), 2);
    assert_eq!(delays, [std::time::Duration::from_secs(1)]);
    assert!(outcome.provider_failure.is_none());
    assert!(
        state
            .items
            .values()
            .all(|item| item.kind != ItemKind::ToolCall || item.ok != Some(true))
    );
}

#[tokio::test]
async fn codex_dropped_body_respects_zero_retry_and_settles_known_without_output() {
    // Catches: a provider mapping retries behind the turn owner's explicit zero allowance.
    let (outcome, _, requests, delays) = dropped_codex_turn(": dropped\n\n", 0).await;
    assert_eq!(outcome.exit_reason, ExitReason::Provider);
    assert_eq!(requests.len(), 1);
    assert!(delays.is_empty());
    assert!(outcome.provider_failure.unwrap().effect_known);
}

struct NeverRunTool;
impl Tool for NeverRunTool {
    fn descriptor(&self) -> ego_core::permit::ToolDescriptor {
        ego_core::permit::ToolDescriptor {
            name: "read_file".to_owned(),
            description: "Must not run after an incomplete response".to_owned(),
            schema: serde_json::from_value(serde_json::json!({"type":"object"})).unwrap(),
        }
    }
    fn prepare(
        &self,
        _: serde_json::Value,
        _: &ego_core::permit::PrepareCtx<'_>,
    ) -> Result<ego_core::permit::PreparedCall, ego_core::permit::ToolError> {
        panic!("a tool from a failed response must not be prepared")
    }
    fn execute<'a, 'io>(
        &'a self,
        _: ego_core::permit::PreparedCall,
        _: &'a dyn ego_core::permit::GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ego_core::mcp::ToolExecution, ego_core::permit::ToolError>>
    where
        'io: 'a,
    {
        panic!("a tool from a failed response must not execute")
    }
}

#[tokio::test]
async fn fast_contract_catches_missing_wire_tier_and_unmultiplied_turn_cost() {
    use ego_ai::service_tier::ServiceTier;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Real loopback HTTP, no model calls. The tariff is captured in
    // ARCHITECTURE.md
    for (tier, expected) in [
        (ServiceTier::Standard, "0.0000453"),
        (ServiceTier::Fast, "0.0000906"),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            let body = loop {
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let size: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + size {
                        break serde_json::from_slice::<serde_json::Value>(
                            &request[end + 4..end + 4 + size],
                        )
                        .unwrap();
                    }
                }
            };
            let transcript = include_str!("goldens/profile/openai/stream/text.sse");
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", transcript.len()).as_bytes()).await.unwrap();
            socket.write_all(transcript.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            body
        });
        let http = Arc::new(SocketHttp {
            base,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
        });
        let (outcome, state) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            turn_over_http(
                "openai/gpt-6.1-sol",
                ProviderRow::openai(),
                &ego_ai::wire::openai_responses::OpenAiResponses,
                http,
                0,
                Arc::new(NoRetryPause),
                TurnFixture {
                    tier: Some(tier),
                    ..Default::default()
                },
            ),
        )
        .await
        .expect("loopback turn must finish");
        let body = server.await.unwrap();
        assert_eq!(outcome.exit_reason, ExitReason::Completed);
        assert_eq!(answer(&state), "ok");
        if tier == ServiceTier::Fast {
            assert_eq!(body["service_tier"], "fast");
        } else {
            assert_eq!(body["service_tier"], "default");
        }
        assert_eq!(
            state.cost,
            CumulativeCost::Known {
                total: usd(expected)
            }
        );
    }
}
// regression case: completed Codex sockets must not survive a human wait.
#[derive(Default)]
struct QuestionSocketState {
    now: std::sync::atomic::AtomicU64,
    live: std::sync::atomic::AtomicUsize,
    opened: std::sync::atomic::AtomicUsize,
    sent: Mutex<Vec<serde_json::Value>>,
    fail_after_answer: std::sync::atomic::AtomicBool,
}

struct QuestionHttp(Arc<QuestionSocketState>);
struct QuestionSocket {
    state: Arc<QuestionSocketState>,
    last_used: u64,
    frames: std::collections::VecDeque<Vec<u8>>,
}

impl Drop for QuestionSocket {
    fn drop(&mut self) {
        self.state
            .live
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Http for QuestionHttp {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        panic!("WebSocket test must not silently fall back to HTTP")
    }

    fn open_websocket(
        &self,
        _call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<Box<dyn ego_ai::http::HttpWebSocket>, TransportError>> {
        use std::sync::atomic::Ordering::SeqCst;
        self.0.opened.fetch_add(1, SeqCst);
        self.0.live.fetch_add(1, SeqCst);
        let socket = QuestionSocket {
            last_used: self.0.now.load(SeqCst),
            state: self.0.clone(),
            frames: Default::default(),
        };
        Box::pin(async move { Ok(Box::new(socket) as Box<dyn ego_ai::http::HttpWebSocket>) })
    }
}

impl ego_ai::http::HttpWebSocket for QuestionSocket {
    fn send(&mut self, call: PreparedHttpCall) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            use std::sync::atomic::Ordering::SeqCst;
            let (_, _, _, request) = call.into_parts();
            let body: serde_json::Value = serde_json::from_slice(request.body()).unwrap();
            let now = self.state.now.load(SeqCst);
            if now - self.last_used > 60 {
                return Err(TransportError::possibly_sent(TransportFailure::Exchange(
                    "provider idle limit".into(),
                )));
            }
            if now > 0 && self.state.fail_after_answer.swap(false, SeqCst) {
                return Err(TransportError::definitely_unsent(
                    TransportFailure::Connect("real connection failure after answer".into()),
                ));
            }
            self.last_used = now;
            let first = self.state.sent.lock().unwrap().is_empty();
            self.state.sent.lock().unwrap().push(body);
            if first {
                let args = serde_json::json!({"prompt":"Choose", "mode":"single", "options":[{"id":"yes","label":"Yes"}], "free_text":false}).to_string();
                let call = serde_json::json!({"type":"function_call", "id":"fc_question", "call_id":"question_1", "name":"ask_question", "arguments":args});
                self.frames.push_back(
                    serde_json::to_vec(
                        &serde_json::json!({"type":"response.output_item.added", "item":call}),
                    )
                    .unwrap(),
                );
                self.frames.push_back(serde_json::to_vec(&serde_json::json!({"type":"response.completed", "response":{"id":"resp_question", "status":"completed", "output":[call], "usage":{"input_tokens":8,"output_tokens":3}}})).unwrap());
            } else {
                // Reuse the recorded provider text frames, not a hand-written answer dialect.
                self.frames.extend(
                    CODEX_TEXT
                        .lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                        .map(|line| line.as_bytes().to_vec()),
                );
            }
            Ok(())
        })
    }

    fn receive(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(future::ready(Ok(self.frames.pop_front())))
    }
}

struct WaitingQuestion {
    state: Arc<QuestionSocketState>,
    live_during_wait: Arc<std::sync::atomic::AtomicUsize>,
    available: bool,
}

impl ego_core::question::Elicitor for WaitingQuestion {
    fn elicit<'a>(
        &'a self,
        _question: ego_core::event::id::ItemId,
        _request: ego_core::question::QuestionRequest,
    ) -> BoxFuture<'a, Result<ego_core::question::QuestionOutcome, ego_core::question::ElicitorError>>
    {
        Box::pin(async move {
            use std::sync::atomic::Ordering::SeqCst;
            self.live_during_wait
                .store(self.state.live.load(SeqCst), SeqCst);
            self.state.now.store(1801, SeqCst);
            if !self.available {
                return Ok(ego_core::question::QuestionOutcome::Unavailable);
            }
            Ok(ego_core::question::QuestionOutcome::Answered(
                ego_core::question::QuestionAnswer::single("yes"),
            ))
        })
    }
}

async fn question_wait_turn(fail_after_answer: bool, available: bool) {
    use std::sync::atomic::Ordering::SeqCst;
    let sockets = Arc::new(QuestionSocketState::default());
    sockets.fail_after_answer.store(fail_after_answer, SeqCst);
    let live = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
    let pause = Arc::new(RecordedBodyPause::default());
    let (outcome, state) = turn_over_http_with_question(
        "openai/gpt-6.1-sol",
        ProviderRow::openai_at(Url::parse("https://fixture.invalid/backend-api/").unwrap()),
        &ego_ai::wire::openai_codex_responses::OpenAiCodexResponses,
        Arc::new(QuestionHttp(sockets.clone())),
        6,
        pause.clone(),
        (
            vec![ego_core::turn::RegisteredTool::HumanInput(Arc::new(
                ego_core::question::AskQuestionTool,
            ))],
            Some(Arc::new(WaitingQuestion {
                state: sockets.clone(),
                live_during_wait: live.clone(),
                available,
            })),
            None,
        ),
    )
    .await;
    assert_eq!(
        live.load(SeqCst),
        0,
        "completed provider socket stayed open during the human wait"
    );
    if !available {
        assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
        assert!(state.pending_question().is_some());
        assert_eq!(sockets.sent.lock().unwrap().len(), 1);
        assert_eq!(sockets.live.load(SeqCst), 0);
        return;
    }
    assert_eq!(outcome.exit_reason, ExitReason::Completed);
    assert_eq!(answer(&state), "ok");
    let requests = sockets.sent.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "one question request, one answer continuation"
    );
    assert!(
        requests[1].get("previous_response_id").is_none(),
        "connection-local response ID must not survive the question"
    );
    assert!(
        requests[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output")
    );
    assert_eq!(
        sockets.opened.load(SeqCst),
        if fail_after_answer { 3 } else { 2 }
    );
    assert_eq!(
        pause.0.lock().unwrap().len(),
        usize::from(fail_after_answer)
    );
}

#[tokio::test]
async fn question_wait_does_not_hold_the_provider_stream() {
    // Catches: the completed provider socket idles out while the question waits.
    question_wait_turn(false, true).await;
}

#[tokio::test]
async fn question_answer_still_retries_a_real_connection_failure() {
    // Catches: releasing idle state disables genuine post-answer network retries.
    question_wait_turn(true, true).await;
}

#[tokio::test]
async fn unavailable_question_host_releases_the_provider_connection_before_suspension() {
    // Catches: a durable pending question retains its previous provider socket.
    question_wait_turn(false, false).await;
}
