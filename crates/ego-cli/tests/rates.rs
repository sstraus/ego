//! §5's third rate source, composed: the cache, the fetch, and the six
//! outcomes `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §7 tabulates.
//!
//! This is the only place in the workspace where the three crates that own the
//! pieces meet — `ego-tools` holds the cache because it owns `StoreIo`,
//! `ego-ai` holds the fetch and the decode because it owns `Http`, and
//! `ego-core` holds the types and opens nothing — so it is also the only place
//! the composition can be wrong.
//!
//! The claim under test is §7's first row and it is a *privacy* claim, not an
//! optimisation: an ordinary `ego` run contacts nobody but the model provider.
//! Everything else here is the fail-closed posture — a ceiling is a guarantee
//! the operator asked for, so "I could not establish what this costs" must
//! never resolve to "proceed unbounded".

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ego_ai::catalogue::{ResolvedModel, tabulated, unmetered};
use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
use ego_ai::provider::ProviderRow;
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_cli::budget::{NoRates, TableSource};
use ego_core::budget::{BudgetGate, Ledger, Metering};
use ego_core::cost::{CurrencyCode, Money, RateSource};
use ego_core::event::time::Timestamp;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::permit::{HttpHeader, OneHopResponse, TransportError, TransportFailure};
use ego_tools::pricing::PricingCache;
use ego_tools::store::OsIo;
use futures::future::BoxFuture;

/// One real entry, trimmed to the fields the decoder reads.
const TABLE: &str = r#"{
  "gpt-5": {"input_cost_per_token": 1.25e-06, "cache_read_input_token_cost": 1.25e-07,
            "cache_creation_input_token_cost": 1.5e-06, "output_cost_per_token": 1e-05,
            "litellm_provider": "openai", "mode": "chat"}
}"#;

const ETAG: &str = "\"8cc8759e\"";

type ProviderReply = (u16, String, Option<String>);
type SharedProviderReply = Arc<Mutex<Option<ProviderReply>>>;

/// A transport that records every request and answers a fixed script.
///
/// Local rather than shared with `ego-ai`'s stub: what these tests assert is
/// *how many times the network was touched*, often zero, and a counter is the
/// whole apparatus that needs.
struct Wire {
    answer: SharedProviderReply,
    calls: Arc<Mutex<u32>>,
}

impl Wire {
    fn silent() -> Arc<Wire> {
        Arc::new(Wire {
            answer: Arc::new(Mutex::new(None)),
            calls: Arc::new(Mutex::new(0)),
        })
    }

    fn answering(status: u16, body: &str, etag: Option<&str>) -> Arc<Wire> {
        Arc::new(Wire {
            answer: Arc::new(Mutex::new(Some((
                status,
                body.to_string(),
                etag.map(str::to_string),
            )))),
            calls: Arc::new(Mutex::new(0)),
        })
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

impl Http for Wire {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(WireCall {
            answer: Arc::clone(&self.answer),
            calls: Arc::clone(&self.calls),
        })
    }
}

struct WireCall {
    answer: SharedProviderReply,
    calls: Arc<Mutex<u32>>,
}

impl HttpCall for WireCall {
    fn execute(
        &mut self,
        _call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        *self.calls.lock().unwrap() += 1;
        let answer = self.answer.lock().unwrap().clone();
        Box::pin(async move {
            let Some((status, body, etag)) = answer else {
                return Err(TransportError::definitely_unsent(
                    TransportFailure::Exchange("upstream is down".to_string()),
                ));
            };
            let mut headers = vec![
                HttpHeader::new("content-type", b"application/json".to_vec())
                    .expect("content type header"),
            ];
            if let Some(etag) = etag {
                headers.push(HttpHeader::new("etag", etag.into_bytes()).expect("etag header"));
            }
            Ok(OneHopResponse::response(status, headers, body.into_bytes()))
        })
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

/// A resolved `openai/gpt-5` with no rates: the case the table exists for.
fn model(metering: Metering) -> ResolvedModel {
    ResolvedModel {
        model: "gpt-5".to_string(),
        row: ProviderRow::openai(),
        wire: &OpenAiCompletions,
        capabilities: caps(),
        metering,
        rates: None,
    }
}

fn usd(decimal: &str) -> Money {
    Money::new(
        decimal.parse().expect("a decimal"),
        CurrencyCode::new("USD").expect("USD"),
    )
}

fn limited() -> Ledger {
    BudgetGate::new(usd("5")).ledger().clone()
}

fn unlimited() -> Ledger {
    BudgetGate::unlimited(CurrencyCode::new("USD").expect("USD"))
        .ledger()
        .clone()
}

const NOW: u64 = 1_000_000_000_000;

struct Fixture {
    home: tempfile::TempDir,
    io: OsIo,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            home: tempfile::tempdir().expect("a temp dir"),
            io: OsIo,
        }
    }

    fn path(&self) -> &Path {
        self.home.path()
    }

    /// Put a table in the cache, stamped `age` before `NOW`.
    fn cached(&self, bytes: &str, age: Duration) {
        PricingCache::new(&self.io, self.path())
            .publish(
                bytes.as_bytes(),
                ETAG,
                Timestamp::from_millis(NOW - age.as_millis() as u64),
                std::process::id(),
                &|pid| pid == std::process::id(),
            )
            .expect("published");
    }

    fn source<'a>(&'a self, http: &'a Arc<dyn Http>, offline: bool) -> TableSource<'a> {
        TableSource {
            io: &self.io,
            ego_home: self.path(),
            http,
            offline,
            now: Timestamp::from_millis(NOW),
            max_age: Duration::from_secs(7 * 24 * 60 * 60),
        }
    }
}

fn transport(wire: &Arc<Wire>) -> Arc<dyn Http> {
    Arc::clone(wire) as Arc<dyn Http>
}

/// **§7's first row, and the one that is a property rather than a case.**
///
/// No ceiling means no rate source is consulted at all — offline or not — so
/// an ordinary run reaches nobody but the model provider. That is a privacy
/// result and it has to be true before there is a third-party fetch to leak
/// through, not after.
#[tokio::test]
async fn a_run_with_no_ceiling_reads_no_cache_and_opens_no_socket() {
    let fixture = Fixture::new();
    fixture.cached(TABLE, Duration::ZERO);
    let wire = Wire::silent();
    let http = transport(&wire);

    let rates = fixture
        .source(&http, false)
        .rates(&model(tabulated()), &unlimited())
        .await
        .expect("no ceiling is not a failure");

    assert!(rates.is_none());
    assert_eq!(wire.calls(), 0, "nothing was asked of the network");
}

/// A money ceiling on a row nobody charges per token for consults nothing
/// either, and this is the test Codex asked for by name.
///
/// The limit admits every generation whatever a table would have said — the
/// metering mints a zero reservation — so fetching one would be a network call
/// whose result cannot change a single decision.
#[tokio::test]
async fn a_money_ceiling_on_an_unmetered_row_neither_reads_nor_fetches() {
    let fixture = Fixture::new();
    fixture.cached(TABLE, Duration::ZERO);
    let wire = Wire::silent();
    let http = transport(&wire);

    let rates = fixture
        .source(&http, false)
        .rates(&model(unmetered()), &limited())
        .await
        .expect("an inapplicable ceiling is not a failure");

    assert!(rates.is_none());
    assert_eq!(wire.calls(), 0);
}

/// §7 row 4: a fresh cache and a ceiling means no fetch is needed at all.
#[tokio::test]
async fn a_fresh_cache_prices_the_ceiling_without_touching_the_network() {
    let fixture = Fixture::new();
    fixture.cached(TABLE, Duration::from_secs(60));
    let wire = Wire::silent();
    let http = transport(&wire);

    let rates = fixture
        .source(&http, false)
        .rates(&model(tabulated()), &limited())
        .await
        .expect("the cache prices it")
        .expect("gpt-5 is in the table");

    assert!(matches!(rates.source, RateSource::Table { .. }));
    assert_eq!(
        rates.quote.uncached_input.map(|a| a.to_string()),
        Some("0.00000125".to_string())
    );
    assert_eq!(wire.calls(), 0, "a fresh table needs no revalidation");
}

/// §7 row 2: the same, offline. No request leaves the host and the run works.
#[tokio::test]
async fn an_offline_run_prices_a_ceiling_from_a_fresh_cache() {
    let fixture = Fixture::new();
    fixture.cached(TABLE, Duration::from_secs(60));
    let wire = Wire::silent();
    let http = transport(&wire);

    let rates = fixture
        .source(&http, true)
        .rates(&model(tabulated()), &limited())
        .await
        .expect("the cache prices it");

    assert!(rates.is_some());
    assert_eq!(wire.calls(), 0);
}

/// §7 row 3: `--offline` with nothing cached is a self-contradictory
/// invocation — an air-gapped posture *and* a guarantee that needs the network
/// — so the command line is what must change, and the error says so rather
/// than reporting a network failure that never happened.
#[tokio::test]
async fn offline_with_no_cached_table_refuses_without_reaching_for_the_network() {
    let fixture = Fixture::new();
    let wire = Wire::answering(200, TABLE, Some(ETAG));
    let http = transport(&wire);

    let err = fixture
        .source(&http, true)
        .rates(&model(tabulated()), &limited())
        .await
        .expect_err("no table, and none may be fetched");

    assert!(matches!(err, NoRates::Offline { .. }), "{err}");
    assert_eq!(
        wire.calls(),
        0,
        "the refusal is by posture, before any transport could refuse it"
    );
}

/// A stale cache offers its validator back, and an unchanged table costs one
/// round trip and no body.
#[tokio::test]
async fn a_stale_table_the_server_says_is_current_is_used_without_a_second_copy() {
    let fixture = Fixture::new();
    fixture.cached(TABLE, Duration::from_secs(30 * 24 * 60 * 60));
    let wire = Wire::answering(304, "", None);
    let http = transport(&wire);

    let rates = fixture
        .source(&http, false)
        .rates(&model(tabulated()), &limited())
        .await
        .expect("the cached bytes are still current")
        .expect("gpt-5 is in them");

    assert!(matches!(rates.source, RateSource::Table { .. }));
    assert_eq!(wire.calls(), 1);

    // And the clock moved, so the next run is fresh again with no fetch.
    let quiet = Wire::silent();
    let http = transport(&quiet);
    assert!(
        fixture
            .source(&http, false)
            .rates(&model(tabulated()), &limited())
            .await
            .expect("fresh now")
            .is_some()
    );
    assert_eq!(quiet.calls(), 0);
}

/// An empty cache and a working network: fetch, store, price.
#[tokio::test]
async fn a_first_priced_run_fetches_the_table_and_leaves_it_cached() {
    let fixture = Fixture::new();
    let wire = Wire::answering(200, TABLE, Some(ETAG));
    let http = transport(&wire);

    let rates = fixture
        .source(&http, false)
        .rates(&model(tabulated()), &limited())
        .await
        .expect("fetched")
        .expect("gpt-5 is in it");
    assert!(matches!(rates.source, RateSource::Table { .. }));
    assert_eq!(wire.calls(), 1);

    let quiet = Wire::silent();
    let http = transport(&quiet);
    assert!(
        fixture
            .source(&http, false)
            .rates(&model(tabulated()), &limited())
            .await
            .expect("from the cache this time")
            .is_some()
    );
    assert_eq!(quiet.calls(), 0, "fetched once, per §6's title");
}

/// §7 row 6: online, the fetch fails, and nothing usable is cached. Refused,
/// and reported as the network failure it is so a script can retry it.
#[tokio::test]
async fn a_failed_fetch_with_no_cache_refuses_as_a_provider_failure() {
    let fixture = Fixture::new();
    let wire = Wire::silent();
    let http = transport(&wire);

    let err = fixture
        .source(&http, false)
        .rates(&model(tabulated()), &limited())
        .await
        .expect_err("nothing to price with");

    assert!(matches!(err, NoRates::Fetch { .. }), "{err}");
    assert_eq!(wire.calls(), 1, "it was tried");
}

/// **034's criterion 4.** A model the table does not carry is not an error
/// here: it is the absence the gate turns into
/// `UnknownCost::NoPricingQuote`, which must answer only "you asked for a
/// ceiling on a model I cannot price".
///
/// Distinguished from every failure above precisely so that refusal can be
/// specific. A table that could not be fetched and a table that simply does
/// not list your model send an operator to two different places.
#[tokio::test]
async fn a_model_the_table_does_not_list_is_unpriced_rather_than_a_failure() {
    let fixture = Fixture::new();
    fixture.cached(TABLE, Duration::ZERO);
    let wire = Wire::silent();
    let http = transport(&wire);

    let mut absent = model(tabulated());
    absent.model = "gpt-99".to_string();

    let rates = fixture
        .source(&http, false)
        .rates(&absent, &limited())
        .await
        .expect("an unlisted model is not a broken table");

    assert!(rates.is_none());
}

/// §5 is an *order*, first hit wins: rates the operator wrote down are never
/// second-guessed by a third party's summary, and the table is not even read.
#[tokio::test]
async fn rates_that_are_already_resolved_are_not_replaced_by_the_table() {
    let fixture = Fixture::new();
    fixture.cached(TABLE, Duration::ZERO);
    let wire = Wire::silent();
    let http = transport(&wire);

    let mut supplied = model(tabulated());
    supplied.rates = Some(ego_ai::catalogue::operator_rates(
        ego_core::budget::PricingQuote {
            revision: ego_core::cost::PricingRevision("acme/negotiated/2026-08".to_string()),
            currency: CurrencyCode::new("USD").expect("USD"),
            uncached_input: Some("0.000001".parse().expect("a rate")),
            cache_read: None,
            cache_write: None,
            cache_write_extended: None,
            output: Some("0.000002".parse().expect("a rate")),
        },
    ));

    let rates = fixture
        .source(&http, false)
        .rates(&supplied, &limited())
        .await
        .expect("already priced")
        .expect("by the operator");

    assert_eq!(rates.source, RateSource::Operator);
    assert_eq!(wire.calls(), 0);
}
