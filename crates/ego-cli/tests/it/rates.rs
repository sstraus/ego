//! Compose third-party rates across the real cache, fetch/decode, and core owners. No required
//! quote means no pricing contact. Missing rate knowledge stays explicit rather than inventing
//! a price.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ego_ai::catalogue::{ResolvedModel, tabulated, unmetered};
use ego_ai::http::{Http, HttpCall, PreparedHttpCall};
use ego_ai::provider::ProviderRow;
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_cli::budget::{NoRates, TableSource};
use ego_core::budget::{BudgetGate, Ledger, Metering};
use ego_core::cost::{CurrencyCode, Money, PricingUnavailable, RateSource};
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

/// No ceiling requiring rates means no pricing source lookup, offline or online. This protects
/// privacy as well as avoiding unnecessary work.
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

/// A fresh cache satisfies the ceiling without a fetch.
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

/// A fresh cache works offline without any network request.
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

/// Catches: an empty offline cache losing its recovery cause or causing a forbidden fetch.
#[tokio::test]
async fn offline_empty_price_source_preserves_cause_without_a_fetch() {
    let fixture = Fixture::new();
    let wire = Wire::answering(200, TABLE, Some(ETAG));
    let http = transport(&wire);
    let rates = fixture
        .source(&http, true)
        .rates(&model(tabulated()), &limited())
        .await
        .expect_err("source failure must retain its diagnostic for model admission");
    assert_eq!(rates.cause(), PricingUnavailable::OfflineNoCache);
    assert_eq!(wire.calls(), 0);
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
    assert_eq!(quiet.calls(), 0, "a fresh cache performs no fetch");
}

/// Catches: a failed fetch losing the network recovery cause.
#[tokio::test]
async fn failed_price_fetch_preserves_the_network_recovery_cause() {
    let fixture = Fixture::new();
    let wire = Wire::silent();
    let http = transport(&wire);
    let rates = fixture
        .source(&http, false)
        .rates(&model(tabulated()), &limited())
        .await
        .expect_err("source failure must retain its diagnostic for model admission");
    assert_eq!(rates.cause(), PricingUnavailable::FetchFailed);
    assert_eq!(wire.calls(), 1, "it was tried once");
}

// Catches: malformed prices losing the invalid-rate cause or triggering needless fetches.
#[tokio::test]
async fn malformed_cached_prices_preserve_the_invalid_rate_cause_without_a_fetch() {
    let fixture = Fixture::new();
    fixture.cached(
        &TABLE.replace("1.25e-06", "\"not-a-price\""),
        Duration::ZERO,
    );
    let wire = Wire::silent();
    let http = transport(&wire);
    let rates = fixture
        .source(&http, false)
        .rates(&model(tabulated()), &limited())
        .await
        .expect_err("source failure must retain its diagnostic for model admission");
    assert_eq!(rates.cause(), PricingUnavailable::InvalidRate);
    assert_eq!(wire.calls(), 0);
}

/// Operator rates outrank third-party rates; do not even read the table when supplied rates
/// answer.
#[tokio::test]
async fn rates_that_are_already_resolved_are_not_replaced_by_the_table() {
    let fixture = Fixture::new();
    fixture.cached(TABLE, Duration::ZERO);
    let wire = Wire::silent();
    let http = transport(&wire);

    let mut supplied = model(tabulated());
    supplied.rates = Some(ego_ai::catalogue::operator_rates(
        ego_core::budget::PricingQuote {
            web_search: None,
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

// Catches: the cached-table path bypasses OpenRouter's absent-cache policy,
// or applies it to providers whose cache-write premium is not known.
#[tokio::test]
async fn openrouter_cached_table_preserves_missing_tariffs_without_reservation_defaults() {
    let fixture = Fixture::new();
    let capture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../ego-ai/tests/goldens/openrouter/cache-prices-2026-10-27.json"
    ))
    .unwrap();
    let public = &capture["data"][0]["pricing"];
    let base = serde_json::json!({
        "input_cost_per_token": serde_json::from_str::<serde_json::Value>(public["prompt"].as_str().unwrap()).unwrap(),
        "output_cost_per_token": serde_json::from_str::<serde_json::Value>(public["completion"].as_str().unwrap()).unwrap(),
        "litellm_provider": "openrouter", "mode": "chat"
    });
    let mut table = serde_json::Map::new();
    table.insert("openrouter/z-ai/glm-4.7-flash".into(), base);
    fixture.cached(
        &serde_json::Value::Object(table).to_string(),
        Duration::ZERO,
    );
    let wire = Wire::silent();
    let http = transport(&wire);
    let mut routed = model(tabulated());
    routed.row = ProviderRow::openrouter_default();
    routed.model = "z-ai/glm-4.7-flash".into();
    let rates = fixture
        .source(&http, true)
        .rates(&routed, &limited())
        .await
        .unwrap()
        .unwrap();
    assert!(rates.quote.cache_read.is_none());
    assert!(rates.quote.cache_write.is_none());
    assert!(rates.quote.cache_write_extended.is_none());
    assert!(matches!(rates.source, RateSource::Table { .. }));
    assert_eq!(wire.calls(), 0);

    // The existing OpenAI supplied quote test protects the non-OpenRouter
    // source path. The generic table must still retain absent cache rates.
    let generic = ego_ai::pricing::Table::decode(
        TABLE
            .replace("\"cache_creation_input_token_cost\": 1.5e-06,", "")
            .as_bytes(),
    )
    .unwrap()
    .quote("openai", "gpt-5")
    .unwrap()
    .into_quote(ego_core::cost::PricingRevision("generic-table".into()));
    assert!(generic.cache_write.is_none());
}

// Catches: a missing table row losing the update-or-configure diagnostic.
#[tokio::test]
async fn missing_catalogue_row_preserves_the_update_or_configure_cause() {
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
        .expect_err("a missing row retains its diagnostic without inventing a quote");
    assert_eq!(rates.cause(), PricingUnavailable::MissingRow);
}

use crate::support;

// Catches: unavailable pricing losing its safe source cause or refusing real CLI generation.
#[test]
fn missing_price_notice_preserves_sanitized_source_cause() {
    use ego_core::event::time::Clock;
    for (table, cause, message) in [
        ("{}", "missing_row", "price table has no matching model row"),
        (
            r#"{"claude-haiku-5-5":{"litellm_provider":"anthropic","mode":"chat","input_cost_per_token":"https://user:fixture-secret@proxy.invalid/?token=fixture-secret"}}"#,
            "invalid_rate",
            "price table contains an invalid rate",
        ),
    ] {
        let places = support::Places::new();
        let stub = support::Stub::start(support::Plan::default());
        PricingCache::new(&OsIo, places.home_path())
            .publish(
                table.as_bytes(),
                ETAG,
                ego_tools::clock::SystemClock.now(),
                std::process::id(),
                &|pid| pid == std::process::id(),
            )
            .unwrap();
        let result = places
            .ego(&stub)
            .env("ANTHROPIC_API_KEY", "fixture-not-a-secret")
            .args([
                "--model",
                "anthropic/claude-haiku-5-5",
                "--sandbox",
                "off",
                "--budget",
                "1$",
                "--max-output-tokens",
                "32",
                "--no-tools",
                "hello",
            ])
            .run();
        let mut evidence = result.stderr.clone();
        for entry in std::fs::read_dir(places.home_path().join("sessions"))
            .into_iter()
            .flatten()
        {
            evidence.push_str(
                &std::fs::read_to_string(entry.unwrap().path().join("events.jsonl")).unwrap(),
            );
        }
        assert!(
            evidence.contains("CostAnomaly") && evidence.contains("anthropic/claude-haiku-5-5"),
            "{}\n{evidence}",
            result.stderr
        );
        assert!(
            !stub.generations().is_empty(),
            "missing pricing must not prevent send"
        );
        assert!(
            evidence.contains(&format!("\"cause\":\"{cause}\"")) && result.stderr.contains(message),
            "missing pricing must preserve its actionable source cause: {evidence}"
        );
        assert!(
            !evidence.contains("fixture-secret"),
            "raw price source data leaked"
        );
    }
}

/// Catches: a stale offline cache being diagnosed as an absent cache.
#[tokio::test]
async fn offline_stale_prices_keep_the_refresh_action_without_a_fetch() {
    let fixture = Fixture::new();
    fixture.cached(TABLE, Duration::from_secs(8 * 24 * 60 * 60));
    let wire = Wire::silent();
    let http = transport(&wire);
    let error = fixture
        .source(&http, true)
        .rates(&model(tabulated()), &limited())
        .await
        .expect_err("expired prices retain the refresh reason");
    assert_eq!(error.cause(), PricingUnavailable::OfflineStaleCache);
    assert_eq!(wire.calls(), 0);
}

/// Catches: arbitrary URLs, error bodies and paths leaking through pricing diagnostics.
#[test]
fn pricing_source_causes_exclude_raw_credentials_and_bound_the_notice() {
    use ego_ai::pricing::{FetchError, TableError};
    use ego_tools::pricing::CacheError;
    let secret = "https://user:fixture-secret@proxy.invalid/?token=fixture-secret";
    for (error, expected) in [
        (
            NoRates::Offline {
                cache: CacheError::Corrupt {
                    message: secret.into(),
                },
            },
            PricingUnavailable::OfflineInvalidCache,
        ),
        (
            NoRates::Fetch {
                source: FetchError::Unreachable {
                    endpoint: secret.into(),
                    message: secret.repeat(1000),
                },
            },
            PricingUnavailable::FetchFailed,
        ),
        (
            NoRates::Fetch {
                source: FetchError::NotATable {
                    source: TableError::NotATable {
                        message: secret.into(),
                    },
                },
            },
            PricingUnavailable::InvalidTable,
        ),
        (
            NoRates::Fetch {
                source: FetchError::Unidentified { header: "etag" },
            },
            PricingUnavailable::UnidentifiedTable,
        ),
        (
            NoRates::MissingRow {
                provider: secret.into(),
                model: secret.into(),
            },
            PricingUnavailable::MissingRow,
        ),
        (
            NoRates::Table {
                source: TableError::Contradiction {
                    model: secret.into(),
                    keys: vec![secret.into()],
                },
            },
            PricingUnavailable::ConflictingRates,
        ),
        (
            NoRates::Cache {
                source: CacheError::Io(std::io::Error::other(secret)),
            },
            PricingUnavailable::CacheUnavailable,
        ),
    ] {
        assert_eq!(error.cause(), expected);
        let notice = error.to_string();
        assert!(notice.len() <= 160, "unbounded source diagnostic: {notice}");
        assert!(!notice.contains("fixture-secret"), "raw source data leaked");
        assert!(
            notice.contains("configure a quote"),
            "missing recovery action: {notice}"
        );
    }
}
