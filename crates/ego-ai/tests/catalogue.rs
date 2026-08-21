//! What `--model P/M` resolves to, and what it refuses to invent.
//!
//! See `docs/decisions/2026-08-21-what-a-model-resolves-to.md` and
//! `plans/m0b-vertical-slice.md` Step 13.5.
//!
//! The daemon is a stub that replays what a real Ollama 0.32.14 answered.
//! That is deliberate and not a shortcut: the interesting cases here are a
//! model that is *not* installed, a daemon that answers with the field
//! missing, and a cold model that has to be loaded before it can be observed
//! — and a developer's live daemon cannot be made to be in three states at
//! once. The transcripts it replays were captured from the real thing; the
//! measurement they encode is recorded in the ADR.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use ego_ai::catalogue::{
    Catalogue, HostedModel, LOCAL_UNMETERED, ModelSlug, ResolveError, operator_rates, subscription,
    tabulated, unmetered,
};
use ego_ai::http::{Http, HttpError, HttpRequest, HttpResponse, Method};
use ego_ai::ollama;
use ego_ai::provider::{Auth, ProviderRow};
use ego_ai::wire::provider_cost::{ProviderCost, XaiUsdTicks};
use ego_core::budget::{Metering, PricingQuote, ResolvedRates, SettlementSource};
use ego_core::cost::{CurrencyCode, MeteringRevision, PricingRevision, RateSource, UnknownCost};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::types::{Conversation, GenRequest, ThinkingLevel};
use futures::executor::block_on;
use futures::future::BoxFuture;
use url::Url;

// --- fixtures ---

fn usd() -> CurrencyCode {
    CurrencyCode::new("USD").expect("USD is valid")
}

/// What a real `GET /api/tags` answered for this machine, trimmed to the
/// fields resolution reads.
const TAGS: &str = r#"{"models":[
  {"name":"gemma4:12b-mlx","capabilities":["completion","tools","thinking"]},
  {"name":"gemma4:e4b-mlx","capabilities":["completion","tools","thinking"]}
]}"#;

/// What `GET /api/ps` answered while `gemma4:e4b-mlx` was loaded. The 32768
/// is the measured value, four times smaller than `/api/show`'s 131072.
const PS_LOADED: &str = r#"{"models":[
  {"name":"gemma4:e4b-mlx","context_length":32768}
]}"#;

/// What `GET /api/ps` answers with nothing loaded.
const PS_EMPTY: &str = r#"{"models":[]}"#;

/// What the load-only request answers: no content, no usage.
const LOADED: &str = r#"{"model":"gemma4:e4b-mlx","done":true,"done_reason":"load",
  "message":{"role":"assistant","content":""}}"#;

/// A transport that answers a scripted queue and records what it was asked.
struct StubDaemon {
    answers: Mutex<VecDeque<(u16, String)>>,
    seen: Mutex<Vec<(Method, String, Option<String>)>>,
}

impl StubDaemon {
    fn new(answers: impl IntoIterator<Item = (u16, &'static str)>) -> Arc<Self> {
        Arc::new(StubDaemon {
            answers: Mutex::new(
                answers
                    .into_iter()
                    .map(|(status, body)| (status, body.to_string()))
                    .collect(),
            ),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect()
    }

    fn bodies(&self) -> Vec<Option<String>> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, body)| body.clone())
            .collect()
    }
}

impl Http for StubDaemon {
    fn execute(&self, req: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        self.seen.lock().unwrap().push((
            req.method,
            req.url.path().to_string(),
            req.body
                .as_ref()
                .map(|body| String::from_utf8_lossy(body).to_string()),
        ));
        let answer = self.answers.lock().unwrap().pop_front();
        Box::pin(async move {
            match answer {
                Some((status, body)) => Ok(HttpResponse {
                    status,
                    headers: vec![("content-type".to_string(), "application/json".to_string())],
                    body: Box::pin(futures::stream::once(async move {
                        Ok(bytes::Bytes::from(body.into_bytes()))
                    })),
                }),
                None => Err(HttpError::Transport {
                    message: "the stub was asked more than it was scripted for".to_string(),
                    delivery: ego_core::generator::attempt::DeliveryCertainty::DefinitelyUnsent,
                }),
            }
        })
    }
}

fn daemon(
    answers: impl IntoIterator<Item = (u16, &'static str)>,
) -> (Arc<dyn Http>, Arc<StubDaemon>) {
    let stub = StubDaemon::new(answers);
    (Arc::clone(&stub) as Arc<dyn Http>, stub)
}

fn local_row() -> ProviderRow {
    ProviderRow::ollama(Url::parse("http://localhost:11434/v1/chat/completions").unwrap())
}

fn hosted_row() -> ProviderRow {
    ProviderRow {
        id: "openai".to_string(),
        endpoint: Url::parse("https://api.openai.com/v1/chat/completions").unwrap(),
        auth: Auth::Bearer("sk-not-a-real-key".into()),
        cost: None,
    }
}

/// A deliberately NON-zero hosted quote. The point of the test that uses it:
/// a catalogue that assigns zero by accident to a hosted row is exactly what
/// a free-first table hides, so one test spends real (stubbed) money.
fn hosted_quote() -> PricingQuote {
    PricingQuote {
        revision: PricingRevision("supplied-by-the-caller".to_string()),
        currency: usd(),
        uncached_input: Some("0.000003".parse().unwrap()),
        cache_read: Some("0.0000003".parse().unwrap()),
        cache_write: Some("0.00000375".parse().unwrap()),
        output: Some("0.000015".parse().unwrap()),
    }
}

fn hosted_caps() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: 1,
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        max_tools: None,
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::SameProviderOnly,
        reasoning_efforts: ReasoningEffortSupport::Any,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Long,
        transport: Transport::Sse,
    }
}

fn request() -> GenRequest {
    GenRequest {
        model: "gemma4:e4b-mlx".to_string(),
        messages: Conversation::new(Vec::new()).expect("an empty history is valid"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(1_024),
        parallel_tool_calls: false,
    }
}

// --- the slug ---

#[test]
fn a_model_slug_splits_on_the_first_separator_only() {
    // `CLI.md` gives `openrouter/z-ai/glm-4.7` as an example, so a model id
    // may contain slashes of its own. Splitting on the last one would send a
    // request to a provider called `openrouter/z-ai`.
    let slug = ModelSlug::parse("openrouter/z-ai/glm-4.7").expect("a well-formed slug");
    assert_eq!(slug.provider, "openrouter");
    assert_eq!(slug.model, "z-ai/glm-4.7");
    assert_eq!(slug.to_string(), "openrouter/z-ai/glm-4.7");
}

#[test]
fn a_reference_that_names_no_provider_is_refused_rather_than_defaulted() {
    // Guessing a provider for a bare model name would send a request
    // somewhere the user did not ask for, on an account they did not name.
    for given in ["gpt-5.6", "", "/gpt-5.6", "openai/"] {
        assert_eq!(
            ModelSlug::parse(given),
            Err(ResolveError::MalformedSlug {
                given: given.to_string()
            }),
            "`{given}` does not name a provider and a model"
        );
    }
}

// --- hosted: supplied, or refused ---

/// The fixture quote as an operator supplied it: `models.toml` is the only
/// place a hosted rate can come from in this build, and it is the first of
/// §5's three consults.
fn hosted_rates() -> ResolvedRates {
    operator_rates(hosted_quote())
}

#[test]
fn a_hosted_model_with_no_supplied_capabilities_is_refused_by_name() {
    // What this refusal is now about, after
    // `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §1: the model's
    // *shape*, not its price. A hosted provider publishes no capability
    // endpoint, so nothing here can discover the context window or the
    // output ceiling, and a request built against a guessed window is a
    // request the provider rejects.
    //
    // The price is deliberately no longer part of it. §1 removed the refusal
    // for want of a rate — a run with no ceiling needs no rate, and refusing
    // it would be ego imposing a ceiling of its own. The test below
    // (`a_hosted_row_with_capabilities_and_no_rates_resolves`) is the other
    // half of that pair.
    let catalogue = Catalogue::new([hosted_row()]);
    let slug = ModelSlug::parse("openai/gpt-5.6").unwrap();

    let refused = catalogue
        .resolve_hosted(&slug)
        .expect_err("no figures were supplied for it");

    assert_eq!(
        refused,
        ResolveError::HostedModelNotConfigured {
            slug: "openai/gpt-5.6".to_string()
        }
    );
    assert!(
        refused.to_string().contains("openai/gpt-5.6"),
        "the refusal names the model: {refused}"
    );
}

#[test]
fn a_supplied_hosted_model_resolves_to_exactly_what_was_supplied() {
    let catalogue = Catalogue::new([hosted_row()]).with_hosted(
        "openai/gpt-5.6",
        HostedModel {
            capabilities: hosted_caps(),
            metering: tabulated(),
            rates: Some(hosted_rates()),
        },
    );

    let resolved = catalogue
        .resolve_hosted(&ModelSlug::parse("openai/gpt-5.6").unwrap())
        .expect("it was supplied");

    assert_eq!(resolved.model, "gpt-5.6", "the provider half is not sent");
    assert_eq!(resolved.row.id, "openai");
    assert_eq!(resolved.capabilities, hosted_caps());
    assert_eq!(resolved.metering, tabulated());
    assert_eq!(
        resolved.rates,
        Some(hosted_rates()),
        "the rates travel beside the metering, and arrive with their provenance"
    );
}

/// The other half of the narrowed refusal: capabilities and no rates is a
/// legal, resolvable hosted row. §1 — a run with no ceiling needs no rate, so
/// refusing this would be ego imposing a ceiling nobody asked for.
#[test]
fn a_hosted_row_with_capabilities_and_no_rates_resolves() {
    let catalogue = Catalogue::new([hosted_row()]).with_hosted(
        "openai/gpt-5.6",
        HostedModel {
            capabilities: hosted_caps(),
            metering: tabulated(),
            rates: None,
        },
    );

    let resolved = catalogue
        .resolve_hosted(&ModelSlug::parse("openai/gpt-5.6").unwrap())
        .expect("a shape was supplied, which is all a run without a ceiling needs");

    assert_eq!(
        resolved.metering,
        tabulated(),
        "it is still billed in money"
    );
    assert_eq!(resolved.rates, None);
    assert_eq!(
        resolved
            .metering
            .upper_bound(None, &request(), &resolved.capabilities, usd()),
        Ok(None),
        "with no rates there is no bound to compute, and that is not an error"
    );
}

#[test]
fn a_hosted_model_is_not_quietly_free() {
    // The constraint this whole file exists to keep: a catalogue that assigns
    // zero by accident to a hosted row is what a free-first table hides. So
    // one test bounds a real hosted request and asserts it costs money.
    let catalogue = Catalogue::new([hosted_row()]).with_hosted(
        "openai/gpt-5.6",
        HostedModel {
            capabilities: hosted_caps(),
            metering: tabulated(),
            rates: Some(hosted_rates()),
        },
    );
    let resolved = catalogue
        .resolve_hosted(&ModelSlug::parse("openai/gpt-5.6").unwrap())
        .expect("it was supplied");

    let bound = resolved
        .metering
        .upper_bound(
            resolved.rates.as_ref().map(|rates| &rates.quote),
            &request(),
            &resolved.capabilities,
            usd(),
        )
        .expect("a fully priced model with stated ceilings can be bounded")
        .expect("and rates were supplied, so there is a bound");

    assert!(
        !bound.amount.is_zero(),
        "a hosted generation that bounds to nothing is a table that forgot to charge"
    );
    // Two spellings of "not the local policy", and both are needed. The
    // variant check is the one the type system now makes possible: a hosted
    // row that came out `Unmetered` would bound to zero whatever its
    // revision said. The revision check is the one that survives if someone
    // ever hands a hosted row an all-zero table wearing the local label.
    assert!(
        matches!(resolved.metering, Metering::Currency { .. }),
        "a hosted row is metered in currency, not by policy: {:?}",
        resolved.metering
    );
    assert_ne!(
        resolved.metering,
        unmetered(),
        "the local-unmetered policy must never reach a hosted row"
    );
    let rates = resolved.rates.as_ref().expect("rates were supplied");
    assert_ne!(
        rates.quote.revision,
        PricingRevision(LOCAL_UNMETERED.to_string()),
        "nor may its label be borrowed for a hosted table"
    );
    assert_eq!(
        rates.source,
        RateSource::Operator,
        "and the published provenance is the one the rates actually came from"
    );
}

#[test]
fn an_unknown_provider_is_refused_with_the_ones_that_exist() {
    let catalogue = Catalogue::new([hosted_row(), local_row()]);
    let refused = catalogue
        .row("anthropic")
        .expect_err("no such row is configured");

    match refused {
        ResolveError::UnknownProvider { provider, known } => {
            assert_eq!(provider, "anthropic");
            assert_eq!(known, vec!["ollama".to_string(), "openai".to_string()]);
        }
        other => panic!("an unconfigured provider is not {other:?}"),
    }
}

// --- local: discovered, never written down ---

#[test]
fn a_loaded_local_model_resolves_to_the_context_the_daemon_actually_serves() {
    // The measurement this design turns on: `/api/show` reports 131072 for
    // this model and `/api/ps` reports 32768, because the served ceiling
    // depends on a server-side setting no API exposes. Taking the first would
    // tell the gate there is four times the room there is.
    let (http, stub) = daemon([(200, TAGS), (200, PS_LOADED)]);

    let resolved = block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http))
        .expect("the daemon serves it and says how much context it has");

    assert_eq!(resolved.capabilities.max_context_tokens, Some(32_768));
    assert_eq!(
        stub.paths(),
        vec!["/api/tags".to_string(), "/api/ps".to_string()],
        "a model that is already loaded must not be loaded again"
    );
}

#[test]
fn ollama_reports_the_levels_it_honours_and_not_the_ones_it_merely_accepts() {
    // Read from `ollama/ollama@v0.32.14`, `openai/openai.go`:
    //
    //     case "minimal":                    return "low"
    //     case "low","medium","high","max":  return effort
    //     case "xhigh","ultra":              return "max"
    //
    // Every one of the seven answers `200`. Two of them come back a tier
    // away from what was asked. This row reported `Any` until the wire
    // survey read that function, because the measurement behind `Any` had
    // tested acceptance and the comment had written "validates".
    //
    // The set below is therefore the answer to "which levels survive the
    // round trip", which is the only question `ReasoningEffortSupport`
    // exists to answer — its own preamble says the parameter is not
    // advisory. `Minimal` and `XHigh` are refused by `request_gaps` before
    // a request is built, naming these five; that refusal is pinned in
    // `ego-core`'s `a_level_outside_the_stated_set_is_refused_naming_the_ones_that_would_work`,
    // so this test owns the fact and that one owns the behaviour.
    let (http, _stub) = daemon([(200, TAGS), (200, PS_LOADED)]);

    let resolved = block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http))
        .expect("the daemon serves it");

    assert_eq!(
        resolved.capabilities.reasoning_efforts,
        ReasoningEffortSupport::Only(BTreeSet::from([
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Max,
        ])),
        "a 200 proves the daemon took the request, never that it did what \
         the request said"
    );
}

#[test]
fn a_cold_local_model_is_loaded_by_the_control_plane_and_never_by_a_generation() {
    // `/api/ps` answers only for models loaded right now, so a cold model has
    // to be loaded first. The load is the daemon's own `messages: []`
    // request, which answers `done_reason: "load"` with no content and no
    // usage. A one-token generation would produce output and usage that would
    // then have to be hidden from the log — which is the dishonest version of
    // this, and the reason the distinction is asserted here rather than
    // assumed.
    let (http, stub) = daemon([
        (200, TAGS),
        (200, PS_EMPTY),
        (200, LOADED),
        (200, PS_LOADED),
    ]);

    let resolved = block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http))
        .expect("it loads, then answers");

    assert_eq!(resolved.capabilities.max_context_tokens, Some(32_768));
    assert_eq!(
        stub.paths(),
        vec![
            "/api/tags".to_string(),
            "/api/ps".to_string(),
            "/api/chat".to_string(),
            "/api/ps".to_string(),
        ],
        "cold: list, look, load, look again"
    );

    let load_body = stub.bodies()[2]
        .clone()
        .expect("the load request carries a body");
    let sent: serde_json::Value = serde_json::from_str(&load_body).expect("it is JSON");
    assert_eq!(
        sent["messages"],
        serde_json::json!([]),
        "an empty message list is what makes this a load and not a generation: {load_body}"
    );
    assert!(
        sent.get("prompt").is_none() && sent.get("stream").is_none(),
        "the load request asks for no generation at all: {load_body}"
    );
}

#[test]
fn a_local_model_the_daemon_does_not_serve_is_refused_with_the_ones_it_does() {
    // Never guessed at, and never resolved from a name written in this
    // repository: what is installed is a fact about the operator's machine.
    let (http, _) = daemon([(200, TAGS)]);

    let refused = block_on(ollama::resolve(&local_row(), "llama9:70b", &http))
        .expect_err("it is not installed");

    match refused {
        ResolveError::NotInstalled { model, installed } => {
            assert_eq!(model, "llama9:70b");
            assert_eq!(installed, vec!["gemma4:12b-mlx", "gemma4:e4b-mlx"]);
        }
        other => panic!("an uninstalled model is not {other:?}"),
    }
}

#[test]
fn a_cloud_relayed_model_is_refused_rather_than_priced_at_zero() {
    // `http.rs` documents Ollama's cloudPassthroughMiddleware proxying a
    // `:cloud` model from localhost:11434 to ollama.com. So a loopback
    // endpoint is not evidence of local execution and `auth: None` is not
    // evidence of free. Pricing it at zero would under-reserve a metered call
    // to nothing, which is the free-first table's exact failure mode.
    let (http, stub) = daemon([(200, TAGS)]);

    let refused = block_on(ollama::resolve(&local_row(), "gpt-oss:120b-cloud", &http))
        .expect_err("it is relayed upstream");

    assert_eq!(
        refused,
        ResolveError::NotLocallyServed {
            model: "gpt-oss:120b-cloud".to_string()
        }
    );
    assert!(
        stub.paths().is_empty(),
        "the identifier settles it, so the daemon is never even asked"
    );
}

#[test]
fn a_local_model_that_states_no_context_is_refused_and_no_ceiling_is_invented() {
    // The rule the whole resolver is built on: a fact the daemon does not
    // state stays unstated. Falling back to `/api/show`'s architectural
    // maximum here is precisely the bug this design exists to avoid.
    const PS_NO_CONTEXT: &str = r#"{"models":[{"name":"gemma4:e4b-mlx","context_length":null}]}"#;
    let (http, _) = daemon([
        (200, TAGS),
        (200, PS_NO_CONTEXT),
        (200, LOADED),
        (200, PS_NO_CONTEXT),
    ]);

    let refused = block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http))
        .expect_err("it never says how much context it serves");

    match refused {
        ResolveError::Undiscoverable { model, fact } => {
            assert_eq!(model, "gemma4:e4b-mlx");
            assert!(
                fact.contains("context"),
                "the refusal names the fact: {fact}"
            );
        }
        other => panic!("an unstated ceiling is not {other:?}"),
    }
}

#[test]
fn a_daemon_that_is_not_running_is_refused_naming_where_it_was_looked_for() {
    let (http, _) = daemon([]);

    let refused = block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http))
        .expect_err("nothing answered");

    match refused {
        ResolveError::DaemonUnreachable { endpoint, .. } => assert!(
            endpoint.contains("localhost:11434") && endpoint.contains("/api/tags"),
            "the refusal says where it looked: {endpoint}"
        ),
        other => panic!("an absent daemon is not {other:?}"),
    }
}

#[test]
fn the_control_plane_is_derived_from_the_row_and_not_configured_twice() {
    // Two URLs that must agree about which daemon they mean is two chances to
    // point at different ones. An operator who moves the row moves both.
    let row = ProviderRow::ollama(Url::parse("http://10.0.0.7:9999/v1/chat/completions").unwrap());
    let (http, stub) = daemon([(200, TAGS), (200, PS_LOADED)]);

    block_on(ollama::resolve(&row, "gemma4:e4b-mlx", &http)).expect("it resolves");

    let empty = stub.seen.lock().unwrap().is_empty();
    assert!(
        !empty,
        "the daemon was asked something to have a host to check"
    );
    assert_eq!(stub.paths()[0], "/api/tags");
}

// --- what the daemon says, and only that ---

#[test]
fn a_capability_the_daemon_does_not_state_is_not_inferred() {
    // `/api/tags` reports completion, tools and thinking, and says nothing
    // about parallel tool calls. `false` is the value that only forecloses a
    // fallback; `true` would break a request. And the daemon states no output
    // ceiling at all, so that stays `None` and the request must cap itself.
    let (http, _) = daemon([(200, TAGS), (200, PS_LOADED)]);

    let resolved =
        block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http)).expect("resolved");

    assert!(!resolved.capabilities.supports_parallel_tool_calls);
    assert_eq!(resolved.capabilities.max_output_tokens, None);
    assert_eq!(resolved.capabilities.max_tools, None);
}

#[test]
fn a_local_model_with_no_output_ceiling_is_refused_for_the_ceiling_and_not_the_price() {
    // The reason `UnknownCost::UnstatedCeiling` had to exist. Ollama states
    // no output ceiling, so this is the ordinary path: a request that caps
    // nothing is unbounded. The quote is complete and honest, so telling the
    // operator the *price* is unknown would send them hunting for a table
    // that is already there and already correct.
    let (http, _) = daemon([(200, TAGS), (200, PS_LOADED)]);
    let resolved =
        block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http)).expect("resolved");

    let uncapped = GenRequest {
        max_output_tokens: None,
        ..request()
    };

    assert_eq!(
        resolved
            .metering
            .upper_bound(None, &uncapped, &resolved.capabilities, usd()),
        Err(UnknownCost::UnstatedCeiling {
            ceiling: ego_core::cost::Ceiling::Output
        }),
        "the missing fact is the ceiling, and the refusal has to say so"
    );

    // And with a cap, the same model bounds cleanly — at zero, because it is
    // genuinely unmetered. Note that `Unmetered` does *not* skip the ceiling
    // check to get there: a row nobody charges for is still a row whose
    // shape the binding has to state.
    let bound = resolved
        .metering
        .upper_bound(None, &request(), &resolved.capabilities, usd())
        .expect("a capped request against an unmetered model")
        .expect("an unmetered row bounds at zero, it does not go unbounded");
    assert!(bound.amount.is_zero());
}

// --- the two rows nobody charges per token ---
//
// These two tests replace a pair that pinned the *all-zero `PricingQuote`*:
// that every category was `Some(Amount::ZERO)` and never `None`, and that
// the quote's `PricingRevision` named the policy. That subject no longer
// exists — `docs/decisions/2026-08-21-how-a-model-is-metered.md` §4 removed
// the all-zero quote precisely because "zero per token" and "no per-token
// price at all" were sharing one spelling. What survives is the property the
// pair was really defending, which is that a row nobody charges per token
// still bounds cleanly and is still attributable; both are asserted below
// against the shape that replaced it.

#[test]
fn a_row_with_no_per_token_price_carries_no_rates_at_all_rather_than_zeroed_ones() {
    // The old failure mode was a table with one category left `None`:
    // `upper_bound` consults all three input categories plus output, so a
    // single missing rate failed the gate closed before the network and ran
    // nothing. `Unmetered` cannot reach that state, because it holds no
    // rates to leave out — the bound is zero by construction, not by
    // arithmetic over four zeroes that someone has to remember to fill in.
    let metering = unmetered();
    assert!(
        !matches!(metering, Metering::Currency { .. }),
        "an unmetered row must not be spelled as a currency row: {metering:?}"
    );

    let (http, _) = daemon([(200, TAGS), (200, PS_LOADED)]);
    let resolved =
        block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http)).expect("resolved");
    assert_eq!(resolved.metering, metering, "the resolver mints that row");

    let bound = resolved
        .metering
        .upper_bound(None, &request(), &resolved.capabilities, usd())
        .expect("no category can be unpriced when there are no categories")
        .expect("an unmetered row bounds at zero, it does not go unbounded");
    assert!(bound.amount.is_zero());
}

#[test]
fn a_policy_metered_row_names_the_policy_that_produced_it_not_a_date() {
    // A revision exists to make a past figure attributable — "this zero came
    // from that decision". A date attributes a built-in policy to nothing
    // anyone can check; these strings state what was asserted and under what
    // condition. `MeteringRevision`, not `PricingRevision`: there is no
    // price list here to point at.
    assert_eq!(
        unmetered(),
        Metering::Unmetered {
            revision: MeteringRevision(LOCAL_UNMETERED.to_string())
        }
    );
    assert!(
        LOCAL_UNMETERED.contains("local") && LOCAL_UNMETERED.contains("unmetered"),
        "the revision says what it claims: {LOCAL_UNMETERED}"
    );

    // The subscription row is the operator's to name, because only the
    // operator knows which plan they are on — there is no built-in constant
    // that could be right for everybody.
    assert_eq!(
        subscription("acme/pro-plan/2026-08"),
        Metering::Subscription {
            revision: MeteringRevision("acme/pro-plan/2026-08".to_string())
        }
    );
}

/// A subscription row is supplied through the same slot a priced hosted row
/// is, and comes back out of the resolver intact. Without this, the variant
/// would be constructible and unreachable — a shape with no producer.
#[test]
fn a_subscription_hosted_row_resolves_to_the_subscription_that_was_supplied() {
    let catalogue = Catalogue::new([hosted_row()]).with_hosted(
        "openai/gpt-5.6",
        HostedModel {
            capabilities: hosted_caps(),
            metering: subscription("acme/pro-plan/2026-08"),
            rates: None,
        },
    );

    let resolved = catalogue
        .resolve_hosted(&ModelSlug::parse("openai/gpt-5.6").unwrap())
        .expect("it was supplied");

    assert_eq!(resolved.metering, subscription("acme/pro-plan/2026-08"));
    assert_eq!(
        resolved.rates, None,
        "a flat fee has no per-token rates to carry"
    );
    let bound = resolved
        .metering
        .upper_bound(None, &request(), &resolved.capabilities, usd())
        .expect("stated ceilings, and no per-token charge to bound")
        .expect("a flat fee bounds at zero, it does not go unbounded");
    assert!(
        bound.amount.is_zero(),
        "a flat fee is not spent per request, so there is nothing to reserve"
    );
}

/// The other half of that: a subscription still needs the model's shape.
/// A row nobody meters per token is not thereby a row whose ceilings may go
/// unstated — the binding has to say what was authorised, and "against a
/// model with an unknown context window" is not something anyone can check
/// later.
#[test]
fn a_subscription_with_no_stated_context_window_is_still_refused() {
    let no_window = ModelCapabilities {
        max_context_tokens: None,
        ..hosted_caps()
    };
    assert_eq!(
        subscription("acme/pro-plan/2026-08").upper_bound(None, &request(), &no_window, usd()),
        Err(UnknownCost::UnstatedCeiling {
            ceiling: ego_core::cost::Ceiling::Context
        })
    );
}

/// A provider-reported row names the decoder that will settle it, and takes
/// its rates from the same slot every other currency row does. The pairing is
/// the ADR's §1 invariant — a stated cost arrives after the tokens are spent,
/// so it can never bound them — and the two halves being separately supplied
/// is what makes that inability visible in the types.
#[test]
fn a_provider_reported_row_still_carries_the_table_that_bounds_it() {
    let decoder = XaiUsdTicks::new("2026-08-21");
    let metering = ego_ai::catalogue::provider_reported(&decoder);

    let Metering::Currency { settlement } = &metering else {
        panic!("a provider-reported row is a currency row: {metering:?}");
    };
    assert_eq!(
        *settlement,
        SettlementSource::ProviderReported {
            revision: decoder.revision().clone()
        }
    );

    let catalogue = Catalogue::new([hosted_row()]).with_hosted(
        "openai/gpt-5.6",
        HostedModel {
            capabilities: hosted_caps(),
            metering: metering.clone(),
            rates: Some(hosted_rates()),
        },
    );
    let resolved = catalogue
        .resolve_hosted(&ModelSlug::parse("openai/gpt-5.6").unwrap())
        .expect("it was supplied");
    assert_eq!(resolved.metering, metering);
    assert!(
        !resolved
            .metering
            .upper_bound(
                resolved.rates.as_ref().map(|rates| &rates.quote),
                &request(),
                &resolved.capabilities,
                usd(),
            )
            .expect("the quote bounds it")
            .expect("and rates were supplied, so there is a bound")
            .amount
            .is_zero(),
        "a stated cost settles the charge; it does not excuse the reservation"
    );
}

/// And the case §1 made legal: the same row with no rates at all. It settles
/// exactly, from the invoice figure, and simply has no bound to offer. A
/// `ProviderReported` settlement is the one arrangement where that is a
/// genuinely complete accounting rather than a gap.
#[test]
fn a_provider_reported_row_with_no_rates_has_no_bound_and_is_not_an_error() {
    let decoder = XaiUsdTicks::new("2026-08-21");
    let catalogue = Catalogue::new([hosted_row()]).with_hosted(
        "openai/gpt-5.6",
        HostedModel {
            capabilities: hosted_caps(),
            metering: ego_ai::catalogue::provider_reported(&decoder),
            rates: None,
        },
    );

    let resolved = catalogue
        .resolve_hosted(&ModelSlug::parse("openai/gpt-5.6").unwrap())
        .expect("it was supplied");

    assert_eq!(
        resolved
            .metering
            .upper_bound(None, &request(), &resolved.capabilities, usd()),
        Ok(None),
        "no rates is no bound, not a refusal"
    );
}
