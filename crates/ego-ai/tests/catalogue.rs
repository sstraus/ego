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
    Catalogue, HostedModel, LOCAL_UNMETERED, ModelSlug, ResolveError, unmetered,
};
use ego_ai::http::{Http, HttpError, HttpRequest, HttpResponse, Method};
use ego_ai::ollama;
use ego_ai::provider::{Auth, ProviderRow};
use ego_core::budget::PricingQuote;
use ego_core::cost::{Amount, CurrencyCode, PricingRevision, TokenCategory, UnknownCost};
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
        reasoning_state: None,
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

#[test]
fn a_hosted_model_with_no_supplied_figures_is_refused_by_name() {
    // The final behaviour, not a placeholder. Nothing in this repository can
    // verify a hosted price, and a remembered one is a number the budget gate
    // would trust: a stale low rate mints a reservation below what the
    // provider will actually charge.
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
            pricing: hosted_quote(),
        },
    );

    let resolved = catalogue
        .resolve_hosted(&ModelSlug::parse("openai/gpt-5.6").unwrap())
        .expect("it was supplied");

    assert_eq!(resolved.model, "gpt-5.6", "the provider half is not sent");
    assert_eq!(resolved.row.id, "openai");
    assert_eq!(resolved.capabilities, hosted_caps());
    assert_eq!(resolved.pricing, hosted_quote());
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
            pricing: hosted_quote(),
        },
    );
    let resolved = catalogue
        .resolve_hosted(&ModelSlug::parse("openai/gpt-5.6").unwrap())
        .expect("it was supplied");

    let bound = resolved
        .pricing
        .upper_bound(&request(), &resolved.capabilities)
        .expect("a fully priced model with stated ceilings can be bounded");

    assert!(
        !bound.amount.is_zero(),
        "a hosted generation that bounds to nothing is a table that forgot to charge"
    );
    assert_ne!(
        resolved.pricing.revision,
        PricingRevision(LOCAL_UNMETERED.to_string()),
        "the local-unmetered policy must never reach a hosted row"
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

    let resolved = block_on(ollama::resolve(
        &local_row(),
        "gemma4:e4b-mlx",
        usd(),
        &http,
    ))
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

    let resolved = block_on(ollama::resolve(
        &local_row(),
        "gemma4:e4b-mlx",
        usd(),
        &http,
    ))
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

    let resolved = block_on(ollama::resolve(
        &local_row(),
        "gemma4:e4b-mlx",
        usd(),
        &http,
    ))
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

    let refused = block_on(ollama::resolve(&local_row(), "llama9:70b", usd(), &http))
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

    let refused = block_on(ollama::resolve(
        &local_row(),
        "gpt-oss:120b-cloud",
        usd(),
        &http,
    ))
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

    let refused = block_on(ollama::resolve(
        &local_row(),
        "gemma4:e4b-mlx",
        usd(),
        &http,
    ))
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

    let refused = block_on(ollama::resolve(
        &local_row(),
        "gemma4:e4b-mlx",
        usd(),
        &http,
    ))
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

    block_on(ollama::resolve(&row, "gemma4:e4b-mlx", usd(), &http)).expect("it resolves");

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

    let resolved = block_on(ollama::resolve(
        &local_row(),
        "gemma4:e4b-mlx",
        usd(),
        &http,
    ))
    .expect("resolved");

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
    let resolved = block_on(ollama::resolve(
        &local_row(),
        "gemma4:e4b-mlx",
        usd(),
        &http,
    ))
    .expect("resolved");

    let uncapped = GenRequest {
        max_output_tokens: None,
        ..request()
    };

    assert_eq!(
        resolved
            .pricing
            .upper_bound(&uncapped, &resolved.capabilities),
        Err(UnknownCost::UnstatedCeiling {
            ceiling: ego_core::cost::Ceiling::Output
        }),
        "the missing fact is the ceiling, and the refusal has to say so"
    );

    // And with a cap, the same model bounds cleanly — at zero, because it is
    // genuinely unmetered.
    let bound = resolved
        .pricing
        .upper_bound(&request(), &resolved.capabilities)
        .expect("a capped request against a fully priced model");
    assert!(bound.amount.is_zero());
}

// --- the zero quote ---

#[test]
fn the_unmetered_quote_prices_every_category_and_leaves_none_unpriced() {
    // `None` means *unpriced*, not free, and `upper_bound` consults all three
    // input categories plus output — so a single `None` here would fail the
    // gate closed before the network and run nothing at all.
    let quote = unmetered(usd());

    for category in [
        TokenCategory::UncachedInput,
        TokenCategory::CacheRead,
        TokenCategory::CacheWrite,
        TokenCategory::Output,
    ] {
        let rate = match category {
            TokenCategory::UncachedInput => quote.uncached_input,
            TokenCategory::CacheRead => quote.cache_read,
            TokenCategory::CacheWrite => quote.cache_write,
            TokenCategory::Output => quote.output,
        };
        assert_eq!(
            rate,
            Some(Amount::ZERO),
            "{category:?} must be priced at zero, never left unpriced"
        );
    }
}

#[test]
fn the_unmetered_quote_names_the_policy_that_produced_it_not_a_date() {
    // `PricingRevision` exists to make a past figure attributable — "this
    // number came from that table". A date attributes a built-in policy to
    // nothing anyone can check; this string states what was asserted and
    // under what condition.
    let quote = unmetered(usd());
    assert_eq!(quote.revision, PricingRevision(LOCAL_UNMETERED.to_string()));
    assert!(
        LOCAL_UNMETERED.contains("local") && LOCAL_UNMETERED.contains("unmetered"),
        "the revision says what it claims: {LOCAL_UNMETERED}"
    );
}
