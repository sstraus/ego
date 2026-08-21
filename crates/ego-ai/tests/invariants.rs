//! The two invariants of the provider seam, proved from outside it.
//!
//! `docs/decisions/2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §5
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
//! I3 is not here. Under §4 the second provider-reported cost decoder is a
//! second row in one adapter's field→unit table rather than a second decoder,
//! so the change that would test it does not exist yet; `tests/provider_cost.rs`
//! covers the table.

use std::sync::{Arc, Mutex};

use ego_ai::catalogue::{self, Catalogue, HostedModel, ModelSlug};
use ego_ai::generator::WireGenerator;
use ego_ai::http::{Http, HttpError, HttpRequest, HttpResponse};
use ego_ai::provider::{CredentialStrategy, ProviderRow};
use ego_ai::resolver::Supplied;
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_ai::wire::provider_cost::ProviderCost;
use ego_ai::wire::{RequestRefused, RequestTarget, WireFormat};
use ego_core::budget::{BudgetGate, PricingQuote};
use ego_core::config::{Mode, Ruleset};
use ego_core::cost::{Amount, CumulativeCost, CurrencyCode, Money, PricingRevision};
use ego_core::event::domain::{DomainEvent, ExitReason};
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::port::AiEvent;
use ego_core::generator::types::{ErrorKind, GenRequest, StopReason, ThinkingLevel};
use ego_core::permit::fake::FakeFs;
use ego_core::permit::{CanonPath, PolicyEngine, Tool};
use ego_core::sink::NullSink;
use ego_core::state::{ItemKind, SessionState, Usage};
use ego_core::store::session_store::{SessionMeta, SessionStore, StoreError};
use ego_core::turn::{CancelToken, Model, Session, Tools, TurnConfig, TurnLoop};
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde::Deserialize;
use url::Url;

mod support;
use support::FakeCredentials;

/// The openai-completions recording the family is already tested against.
///
/// Reused deliberately: I1 is the claim that a *new provider* on a supported
/// dialect needs no new code, and a fixture minted for the occasion would
/// quietly weaken it into "a new provider and a new transcript".
const FAMILY_TRANSCRIPT: &str = include_str!("goldens/profile/openai/stream/text.sse");

/// The variable the invented hosted row names, and what a configured machine
/// would have put in it.
const KEY_VAR: &str = "ACME_API_KEY";
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
        schema_version: 1,
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
    }
}

/// A hosted model priced from a table the operator supplied — the arrangement
/// that makes a turn settle at a *number*, which is what both tests assert on.
fn hosted() -> HostedModel {
    HostedModel {
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

    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<BoxStream<'static, Result<DomainEvent, StoreError>>, StoreError> {
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

/// Answers one request with one canned body, in small chunks.
///
/// Chunked at a size that falls inside every frame of both transcripts, so a
/// mapping that assumed a frame arrives whole fails here rather than against a
/// real socket.
struct StubHttp(Mutex<Option<String>>);

impl StubHttp {
    fn replaying(body: impl Into<String>) -> Arc<Self> {
        Arc::new(StubHttp(Mutex::new(Some(body.into()))))
    }
}

impl Http for StubHttp {
    fn execute(&self, _req: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        let body = self.0.lock().expect("not poisoned").take();
        Box::pin(async move {
            let Some(body) = body else {
                return Err(HttpError::Transport {
                    message: "the stub was asked twice".to_string(),
                    delivery: DeliveryCertainty::DefinitelyUnsent,
                });
            };
            let chunks: Vec<Result<bytes::Bytes, HttpError>> = body
                .as_bytes()
                .chunks(7)
                .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
                .collect();
            Ok(HttpResponse {
                status: 200,
                headers: vec![(
                    "content-type".to_string(),
                    "application/octet-stream".to_string(),
                )],
                body: Box::pin(futures::stream::iter(chunks)),
            })
        })
    }
}

// --- the harness ---

/// One whole turn against `row`, over a transport replaying `transcript`.
///
/// Everything a `TurnLoop` borrows is built here and dropped here, which is
/// the only reason this is a function and not a fixture struct: both tests
/// want exactly one turn and nothing between them.
async fn one_turn(
    slug: &str,
    row: ProviderRow,
    transcript: impl Into<String>,
) -> (ExitReason, SessionState) {
    let slug = ModelSlug::parse(slug).expect("a constant this file controls");
    // Through the catalogue rather than around it: "configuration" is the
    // claim, and a row handed straight to the generator would prove only that
    // the generator is generic.
    let catalogue = Catalogue::new([row]).with_hosted(&slug.to_string(), hosted());
    let resolved = catalogue
        .resolve_hosted(&slug)
        .expect("the row and its model were both configured above");
    let credential = resolved
        .row
        .credential
        .resolve(&FakeCredentials::holding(KEY))
        .expect("every variable is set on this fake machine");

    let http: Arc<dyn Http> = StubHttp::replaying(transcript);
    let generator = WireGenerator::new(resolved.row.clone(), http, credential);
    let store = MemStore::default();
    let clock = FixedClock;
    let policy = PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default());
    let fs = FakeFs::default();
    let registry: Vec<Arc<dyn Tool>> = Vec::new();
    let mut gate = BudgetGate::new(usd("10"));

    let mut loop_ = TurnLoop::new(
        Session {
            id: SessionId::new(),
            state: SessionState::default(),
            store: &store,
            clock: &clock,
            sink: &NullSink,
            cancel: CancelToken::new(),
        },
        Model {
            generator: &generator,
            gate: &mut gate,
            metering: resolved.metering.clone(),
            rates: resolved.rates.clone(),
            token_ceiling: None,
            caps: resolved.capabilities.clone(),
            config: TurnConfig {
                model: resolved.model.clone(),
                system: None,
                thinking: ThinkingLevel::Off,
                max_output_tokens: Some(1_024),
                parallel_tool_calls: false,
            },
        },
        Tools::new(&registry, &policy, &fs, CanonPath::new("/ws")),
    );

    let outcome = loop_
        .run("list the files")
        .await
        .expect("the store accepted every append");
    (outcome.exit_reason, loop_.into_state())
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
/// Everything about "acme" here is a value: an id, a base URL, the variable
/// its key lives in, the mapping it speaks, and the model facts an operator
/// supplied. Nothing anywhere learns the name — which is the invariant, and
/// the reason this is written as a row literal rather than through a
/// constructor named after a vendor.
#[tokio::test]
async fn a_provider_on_a_supported_dialect_is_configuration_and_nothing_else() {
    let acme = ProviderRow {
        resolver: &Supplied,
        id: "acme".to_string(),
        base: Url::parse("https://api.acme.example/v1/").expect("a valid url"),
        credential: CredentialStrategy::BearerFromEnv {
            var: KEY_VAR.to_string(),
        },
        wire: &OpenAiCompletions,
        cost: None,
    };

    let (exit, state) = one_turn("acme/wonder-1", acme, FAMILY_TRANSCRIPT).await;

    settled(
        exit,
        &state,
        Usage {
            uncached_input_tokens: 500,
            cache_read_tokens: 400,
            cache_write_tokens: 100,
            cache_write_extended_tokens: 0,
            output_tokens: 250,
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
    ProviderRow {
        resolver: &Supplied,
        id: "beacon".to_string(),
        base: Url::parse("https://beacon.example/").expect("a valid url"),
        credential: CredentialStrategy::None,
        wire: &Beacon,
        cost: None,
    }
}

/// A protocol nobody designed `AiEvent` against reaches the same settled end.
///
/// The totals differ from I1's because this wire reports differently — no
/// cache writes, no reasoning split — and the *price* differs with them. That
/// is the point: the ledger priced a report it had never seen the shape of.
#[tokio::test]
async fn a_new_protocol_is_an_adapter_and_its_registration() {
    let (exit, state) = one_turn("beacon/lighthouse", beacon_row(), beacon_transcript()).await;

    settled(
        exit,
        &state,
        Usage {
            uncached_input_tokens: 500,
            cache_read_tokens: 400,
            cache_write_tokens: 0,
            cache_write_extended_tokens: 0,
            output_tokens: 250,
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

    let (exit, state) = one_turn("beacon/lighthouse", beacon_row(), transcript).await;

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

    let (exit, state) = one_turn("beacon/lighthouse", beacon_row(), transcript).await;

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
