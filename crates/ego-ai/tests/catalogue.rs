//! What `--model P/M` resolves to, and what it refuses to invent.
//!
//! See `ARCHITECTURE.md` and
//! `plans/m0b-vertical-slice.md` Step 13.5.
//!
//! The daemon is a stub that replays what a real Ollama 0.32.14 answered.
//! That is deliberate and not a shortcut: the interesting cases here are a
//! model that is *not* installed, a daemon that answers with the field
//! missing, and a cold model that has to be loaded before it can be observed
//! — and a developer's live daemon cannot be made to be in three states at
//! once. The transcripts it replays were captured from the real thing; the
//! measurement they encode is recorded in the ADR.

use std::collections::BTreeSet;

use ego_ai::catalogue::{
    Catalogue, HostedModel, LOCAL_UNMETERED, ModelSlug, ResolveError, SuppliedAuthority,
    operator_rates, subscription, tabulated, unmetered,
};
use ego_ai::ollama;
use ego_ai::provider::ProviderRow;
use ego_ai::resolver::{OllamaDaemon, Supplied};
use ego_ai::wire::openai_completions::OpenAiCompletions;
use ego_ai::wire::provider_cost::{ProviderCost, XaiUsdTicks};
use ego_core::budget::{Metering, PricingQuote, ResolvedRates, SettlementSource};
use ego_core::cost::{CurrencyCode, MeteringRevision, PricingRevision, RateSource, UnknownCost};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::generator::types::{Conversation, GenRequest, ThinkingLevel};
use futures::executor::block_on;
use url::Url;

mod support;
use support::{credential, server, unasked};

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

fn local_row() -> ProviderRow {
    ProviderRow::ollama(Url::parse("http://localhost:11434/v1/chat/completions").unwrap())
}

/// A hosted row these tests can actually resolve.
///
/// `anthropic` and not `openai`: since story `regression_case` the public OpenAI row
/// is unselectable pending live evidence, and `resolve` refuses it before a
/// resolver is chosen. Every test here is about catalogue mechanics rather
/// than about one vendor, so it uses a row the gate lets through.
fn hosted_row() -> ProviderRow {
    ProviderRow::anthropic_default()
}

/// A deliberately NON-zero hosted quote. The point of the test that uses it:
/// a catalogue that assigns zero by accident to a hosted row is exactly what
/// a free-first table hides, so one test spends real (stubbed) money.
fn hosted_quote() -> PricingQuote {
    PricingQuote {
        web_search: None,
        revision: PricingRevision("supplied-by-the-caller".to_string()),
        currency: usd(),
        uncached_input: Some("0.000003".parse().unwrap()),
        cache_read: Some("0.0000003".parse().unwrap()),
        cache_write: Some("0.00000375".parse().unwrap()),
        cache_write_extended: None,
        output: Some("0.000015".parse().unwrap()),
    }
}

fn hosted_caps() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        audio_input: ego_core::generator::capabilities::AudioInputSupport::Unknown,
        max_context_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        max_tools: None,
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::SameProviderOnly,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Long,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

static SUPPLIED_OPENAI: Supplied = Supplied::new(&OpenAiCompletions);

fn request() -> GenRequest {
    GenRequest {
        web_search: None,
        cache: None,
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
    // `ARCHITECTURE.md` §1: the model's
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
    let slug = ModelSlug::parse("anthropic/claude-opus-5").unwrap();

    let refused = catalogue
        .resolve_hosted(&slug, &OpenAiCompletions)
        .expect_err("no figures were supplied for it");

    assert_eq!(
        refused,
        ResolveError::HostedModelNotConfigured {
            slug: "anthropic/claude-opus-5".to_string()
        }
    );
    assert!(
        refused.to_string().contains("anthropic/claude-opus-5"),
        "the refusal names the model: {refused}"
    );
}

#[test]
fn a_supplied_hosted_model_resolves_to_exactly_what_was_supplied() {
    let catalogue = Catalogue::new([hosted_row()]).with_hosted(
        "anthropic/claude-opus-5",
        HostedModel {
            authority: SuppliedAuthority::System,
            capabilities: hosted_caps(),
            metering: tabulated(),
            rates: Some(hosted_rates()),
        },
    );

    let resolved = catalogue
        .resolve_hosted(
            &ModelSlug::parse("anthropic/claude-opus-5").unwrap(),
            &OpenAiCompletions,
        )
        .expect("it was supplied");

    assert_eq!(
        resolved.model, "claude-opus-5",
        "the provider half is not sent"
    );
    assert_eq!(resolved.row.id(), "anthropic");
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
        "anthropic/claude-opus-5",
        HostedModel {
            authority: SuppliedAuthority::System,
            capabilities: hosted_caps(),
            metering: tabulated(),
            rates: None,
        },
    );

    let resolved = catalogue
        .resolve_hosted(
            &ModelSlug::parse("anthropic/claude-opus-5").unwrap(),
            &OpenAiCompletions,
        )
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
        "anthropic/claude-opus-5",
        HostedModel {
            authority: SuppliedAuthority::System,
            capabilities: hosted_caps(),
            metering: tabulated(),
            rates: Some(hosted_rates()),
        },
    );
    let resolved = catalogue
        .resolve_hosted(
            &ModelSlug::parse("anthropic/claude-opus-5").unwrap(),
            &OpenAiCompletions,
        )
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
        .row("acme")
        .expect_err("no such row is configured");

    match refused {
        ResolveError::UnknownProvider { provider, known } => {
            assert_eq!(provider, "acme");
            assert_eq!(known, vec!["anthropic".to_string(), "ollama".to_string()]);
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
    let (http, stub) = server([(200, TAGS), (200, PS_LOADED)]);

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
    let (http, _stub) = server([(200, TAGS), (200, PS_LOADED)]);

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
    let (http, stub) = server([
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
    let (http, _) = server([(200, TAGS)]);

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
fn an_embedding_only_local_model_is_neither_offered_nor_resolved_for_chat() {
    // Catches: a daemon's installed embedding model becomes a chat choice.
    const TAGS_WITH_EMBEDDING: &str = r#"{"models":[
      {"name":"qwen3:latest","capabilities":["completion","tools"]},
      {"name":"embeddinggemma:300m-qat-q8_0","capabilities":["embedding"]}
    ]}"#;
    let (http, stub) = server([(200, TAGS_WITH_EMBEDDING)]);
    let report = block_on(ollama::enumerate(&local_row(), &http)).expect("daemon model list");
    assert_eq!(
        report
            .models()
            .iter()
            .map(|row| row.slug.model.as_str())
            .collect::<Vec<_>>(),
        ["qwen3:latest"]
    );
    assert_eq!(stub.paths(), ["/api/tags"]);

    let (http, stub) = server([(200, TAGS_WITH_EMBEDDING)]);
    let refusal = block_on(ollama::resolve(
        &local_row(),
        "embeddinggemma:300m-qat-q8_0",
        &http,
    ))
    .expect_err("embedding-only model cannot serve chat");
    assert!(refusal.to_string().contains("embedding"), "{refusal}");
    assert_eq!(stub.paths(), ["/api/tags"]);
}

/// Catches completion-capable models being hidden or refused because they also embed.
#[test]
fn a_completion_capable_embedding_model_remains_available_for_chat() {
    // Counterfactual capability combination at the daemon boundary, not a live recording.
    const DUAL_CAPABILITY_TAGS: &str = r#"{"models":[
      {"name":"gemma4:e4b-mlx","capabilities":["embedding","completion"]}
    ]}"#;
    let (http, stub) = server([(200, DUAL_CAPABILITY_TAGS)]);
    let report = block_on(ollama::enumerate(&local_row(), &http)).expect("daemon model list");
    assert_eq!(
        report
            .models()
            .iter()
            .map(|entry| entry.slug.model.as_str())
            .collect::<Vec<_>>(),
        ["gemma4:e4b-mlx"],
        "embedding support must not hide a completion-capable chat choice"
    );
    assert_eq!(stub.paths(), ["/api/tags"]);

    let (http, stub) = server([(200, DUAL_CAPABILITY_TAGS), (200, PS_LOADED)]);
    let resolved = block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http))
        .expect("completion capability permits chat even when embedding is also supported");
    assert_eq!(resolved.capabilities.max_context_tokens, Some(32_768));
    assert_eq!(stub.paths(), ["/api/tags", "/api/ps"]);
}

#[test]
fn a_cloud_relayed_model_is_refused_rather_than_priced_at_zero() {
    // `http.rs` documents Ollama's cloudPassthroughMiddleware proxying a
    // `:cloud` model from localhost:11434 to ollama.com. So a loopback
    // endpoint is not evidence of local execution and `auth: None` is not
    // evidence of free. Pricing it at zero would under-reserve a metered call
    // to nothing, which is the free-first table's exact failure mode.
    let (http, stub) = server([(200, TAGS)]);

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
    let (http, _) = server([
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
    let (http, _) = unasked();

    let refused = block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http))
        .expect_err("nothing answered");

    match refused {
        ResolveError::EndpointUnreachable { endpoint, .. } => assert!(
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
    let (http, stub) = server([(200, TAGS), (200, PS_LOADED)]);

    block_on(ollama::resolve(&row, "gemma4:e4b-mlx", &http)).expect("it resolves");

    assert!(
        !stub.asked().is_empty(),
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
    // ceiling at all, so that stays `None` without an invented wire cap.
    let (http, _) = server([(200, TAGS), (200, PS_LOADED)]);

    let resolved =
        block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http)).expect("resolved");

    assert!(!resolved.capabilities.supports_parallel_tool_calls);
    assert_eq!(resolved.capabilities.max_output_tokens, None);
    assert_eq!(resolved.capabilities.max_tools, None);
}

#[test]
fn an_unmetered_local_model_needs_no_output_ceiling_to_bound_zero_spend() {
    // Catches: applying a paid output-reservation requirement to a model
    // whose policy guarantees zero monetary spend.
    let (http, _) = server([(200, TAGS), (200, PS_LOADED)]);
    let resolved =
        block_on(ollama::resolve(&local_row(), "gemma4:e4b-mlx", &http)).expect("resolved");

    let uncapped = GenRequest {
        web_search: None,
        cache: None,
        max_output_tokens: None,
        ..request()
    };

    assert_eq!(
        resolved
            .metering
            .upper_bound(None, &uncapped, &resolved.capabilities, usd()),
        Ok(Some(ego_core::cost::Money::zero(usd()))),
        "an unmetered request reserves zero without inventing an output cap"
    );
    assert_eq!(uncapped.max_output_tokens, None);
    assert_eq!(resolved.capabilities.max_output_tokens, None);

    // The same missing output ceiling must still refuse currency metering:
    // no finite paid output reservation can be proved without it.
    assert_eq!(
        tabulated().upper_bound(
            Some(&hosted_quote()),
            &uncapped,
            &resolved.capabilities,
            usd()
        ),
        Err(UnknownCost::UnstatedCeiling {
            ceiling: ego_core::cost::Ceiling::Output
        })
    );

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
// exists — `ARCHITECTURE.md` §4 removed
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

    let (http, _) = server([(200, TAGS), (200, PS_LOADED)]);
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
            revision: MeteringRevision("acme/pro-plan/2026-08".to_string()),
            equivalent: None
        }
    );
}

/// A subscription row is supplied through the same slot a priced hosted row
/// is, and comes back out of the resolver intact. Without this, the variant
/// would be constructible and unreachable — a shape with no producer.
#[test]
fn a_subscription_hosted_row_resolves_to_the_subscription_that_was_supplied() {
    let catalogue = Catalogue::new([hosted_row()]).with_hosted(
        "anthropic/claude-opus-5",
        HostedModel {
            authority: SuppliedAuthority::System,
            capabilities: hosted_caps(),
            metering: subscription("acme/pro-plan/2026-08"),
            rates: None,
        },
    );

    let resolved = catalogue
        .resolve_hosted(
            &ModelSlug::parse("anthropic/claude-opus-5").unwrap(),
            &OpenAiCompletions,
        )
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

#[test]
fn a_built_in_subscription_row_cannot_be_repriced_as_currency() {
    let catalogue = Catalogue::new([ProviderRow::xai_oauth_default()]).with_hosted(
        "xai-oauth/grok-code-fast-1",
        HostedModel {
            authority: SuppliedAuthority::System,
            capabilities: hosted_caps(),
            metering: tabulated(),
            rates: Some(operator_rates(hosted_quote())),
        },
    );
    let (http, server) = unasked();
    let resolved = block_on(catalogue.resolve(
        &ModelSlug::parse("xai-oauth/grok-code-fast-1").unwrap(),
        &http,
        &credential(),
    ))
    .expect("subscription capabilities were supplied");

    assert_eq!(
        resolved.metering,
        subscription("ego/builtin/xai-oauth-subscription/v1")
    );
    assert_eq!(resolved.rates, None);
    assert!(server.asked().is_empty());
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
        "anthropic/claude-opus-5",
        HostedModel {
            authority: SuppliedAuthority::System,
            capabilities: hosted_caps(),
            metering: metering.clone(),
            rates: Some(hosted_rates()),
        },
    );
    let resolved = catalogue
        .resolve_hosted(
            &ModelSlug::parse("anthropic/claude-opus-5").unwrap(),
            &OpenAiCompletions,
        )
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
        "anthropic/claude-opus-5",
        HostedModel {
            authority: SuppliedAuthority::System,
            capabilities: hosted_caps(),
            metering: ego_ai::catalogue::provider_reported(&decoder),
            rates: None,
        },
    );

    let resolved = catalogue
        .resolve_hosted(
            &ModelSlug::parse("anthropic/claude-opus-5").unwrap(),
            &OpenAiCompletions,
        )
        .expect("it was supplied");

    assert_eq!(
        resolved
            .metering
            .upper_bound(None, &request(), &resolved.capabilities, usd()),
        Ok(None),
        "no rates is no bound, not a refusal"
    );
}

// --- the row decides what resolves it ---
//
// `ARCHITECTURE.md` §4.
// `crates/ego-cli/src/model.rs` used to ask `if slug.provider == OLLAMA` and
// call one of two functions, so a second daemon meant a second arm in a crate
// that should not have needed touching. These four are what make that branch
// unnecessary rather than merely absent.

/// A hosted slug resolves without opening a socket.
///
/// The transport is scripted with **nothing**, so the stub errors on the first
/// request. That is the assertion: `Supplied` reads a table, and a resolution
/// that reached for the network would fail here instead of passing.
#[test]
fn a_hosted_slug_resolves_from_the_table_and_asks_nothing() {
    let (http, stub) = unasked();
    let catalogue = Catalogue::new([hosted_row()]).with_hosted(
        "anthropic/claude-opus-5",
        HostedModel {
            authority: SuppliedAuthority::System,
            capabilities: hosted_caps(),
            metering: tabulated(),
            rates: Some(hosted_rates()),
        },
    );

    let resolved = block_on(catalogue.resolve(
        &ModelSlug::parse("anthropic/claude-opus-5").unwrap(),
        &http,
        &credential(),
    ))
    .expect("it was supplied");

    assert_eq!(resolved.model, "claude-opus-5");
    assert_eq!(resolved.capabilities, hosted_caps());
    assert!(
        stub.paths().is_empty(),
        "a supplied model was resolved by asking a server: {:?}",
        stub.paths()
    );
}

/// A local slug resolves by asking the daemon, through the same call.
///
/// Same method, same arguments, opposite behaviour — which is the whole claim:
/// the caller passes a slug and a transport and does not choose.
#[test]
fn a_local_slug_resolves_by_asking_the_daemon_through_the_same_call() {
    let (http, stub) = server([(200, TAGS), (200, PS_LOADED)]);
    let catalogue = Catalogue::new([local_row()]);

    let resolved = block_on(catalogue.resolve(
        &ModelSlug::parse("ollama/gemma4:e4b-mlx").unwrap(),
        &http,
        &credential(),
    ))
    .expect("the daemon serves it");

    assert_eq!(resolved.model, "gemma4:e4b-mlx");
    assert_eq!(
        resolved.capabilities.max_context_tokens,
        Some(32_768),
        "the served context is the measured one"
    );
    assert_eq!(stub.paths(), vec!["/api/tags", "/api/ps"]);
}

/// The row's resolver decides, independently of its constructor identity.
/// An OpenAI-shaped row can use the daemon resolver when an operator points it
/// at a local Ollama endpoint.
#[test]
fn a_row_that_is_not_called_ollama_can_still_be_a_daemon() {
    let (http, stub) = server([(200, TAGS), (200, PS_LOADED)]);
    let mut acme = ProviderRow::openrouter(
        Url::parse("http://localhost:11434/v1/").expect("a valid local base"),
    );
    acme.resolver = &OllamaDaemon;
    let catalogue = Catalogue::new([acme]);

    let resolved = block_on(catalogue.resolve(
        &ModelSlug::parse("openrouter/gemma4:e4b-mlx").unwrap(),
        &http,
        &credential(),
    ))
    .expect("the daemon serves it");

    assert_eq!(resolved.row.id(), "openrouter");
    assert_eq!(resolved.capabilities.max_context_tokens, Some(32_768));
    assert_eq!(stub.paths(), vec!["/api/tags", "/api/ps"]);
}

/// And the converse: a row *called* ollama that says its models are supplied
/// is resolved from the table.
///
/// Without this the test above is satisfied by a dispatch that reads the row
/// for daemons and falls back to the id for everything else. Together they pin
/// that the id is consulted for nothing at all.
#[test]
fn a_row_called_ollama_that_names_the_supplied_resolver_asks_nothing() {
    let (http, stub) = unasked();
    let mut supplied_locally = local_row();
    supplied_locally.resolver = &SUPPLIED_OPENAI;
    let catalogue = Catalogue::new([supplied_locally]).with_hosted(
        "ollama/gemma4:e4b-mlx",
        HostedModel {
            authority: SuppliedAuthority::System,
            capabilities: hosted_caps(),
            metering: unmetered(),
            rates: None,
        },
    );

    let resolved = block_on(catalogue.resolve(
        &ModelSlug::parse("ollama/gemma4:e4b-mlx").unwrap(),
        &http,
        &credential(),
    ))
    .expect("it was supplied");

    assert_eq!(resolved.capabilities, hosted_caps());
    assert!(
        stub.paths().is_empty(),
        "the id was consulted instead of the row: {:?}",
        stub.paths()
    );
}

/// An unknown provider is refused before any resolver runs, listing what does
/// exist.
///
/// The refusal belongs to the lookup and not to a resolver, which is why it
/// keeps the shape it had: a slug naming no row has no resolver to ask.
#[test]
fn an_unknown_provider_is_refused_before_a_resolver_is_chosen() {
    let (http, stub) = unasked();
    let catalogue = Catalogue::new([hosted_row(), local_row()]);

    let refused = block_on(catalogue.resolve(
        &ModelSlug::parse("acme/whatever").unwrap(),
        &http,
        &credential(),
    ))
    .expect_err("there is no acme row");

    assert_eq!(
        refused,
        ResolveError::UnknownProvider {
            provider: "acme".to_string(),
            known: vec!["anthropic".to_string(), "ollama".to_string()],
        }
    );
    assert!(stub.paths().is_empty());
}

/// Catches a second inventory GET aborting resolution after a successful tool step.
#[test]
fn inventory_outage_after_first_resolution_does_not_abort_next_step() {
    let catalogue = Catalogue::new([ProviderRow::openrouter_default()]);
    let (http, stub) = server([
        (
            200,
            include_str!("goldens/openrouter/models-2026-10-15.json"),
        ),
        (503, "inventory temporarily unavailable"),
    ]);
    for model in [
        "openai/gpt-6-luna",
        "anthropic/claude-sonnet-5.5",
        "openai/gpt-6-luna",
    ] {
        let resolved = block_on(catalogue.resolve(
            &ModelSlug::parse(&format!("openrouter/{model}")).unwrap(),
            &http,
            &credential(),
        ))
        .expect("the bound inventory survives an outage, including a model change");
        assert_eq!(resolved.model, model);
        assert!(resolved.capabilities.max_context_tokens.is_some());
    }
    assert_eq!(stub.paths(), vec!["/api/v1/models"]);
}

/// Catches losing the bound inventory when the CLI rebuilds operator facts.
#[test]
fn rebuilt_catalogues_share_inventory_only_inside_the_bound_scope() {
    use ego_ai::catalogue::ResolutionInventory;
    let inventory = ResolutionInventory::default();
    let (http, stub) = server([
        (
            200,
            include_str!("goldens/openrouter/models-2026-10-15.json"),
        ),
        (503, "inventory temporarily unavailable"),
    ]);
    let slug = ModelSlug::parse("openrouter/openai/gpt-6-luna").unwrap();
    for _ in 0..2 {
        let catalogue = Catalogue::new([ProviderRow::openrouter_default()])
            .with_resolution_inventory(inventory.clone());
        assert!(block_on(catalogue.resolve(&slug, &http, &credential())).is_ok());
    }
    // A new scope must discover again rather than carrying stale capabilities forever.
    let next = Catalogue::new([ProviderRow::openrouter_default()]);
    assert!(matches!(
        block_on(next.resolve(&slug, &http, &credential())),
        Err(ResolveError::EndpointUnreachable { .. })
    ));
    assert_eq!(stub.paths(), vec!["/api/v1/models", "/api/v1/models"]);
}

/// Catches reusing account-scoped model facts after the credential changes.
#[test]
fn changed_credential_refetches_inventory_instead_of_reusing_previous_account_facts() {
    use ego_ai::provider::CredentialStrategy;
    use support::FakeCredentials;

    let catalogue = Catalogue::new([ProviderRow::openrouter_default()]);
    let (http, stub) = server([
        (
            200,
            include_str!("goldens/openrouter/models-2026-10-15.json"),
        ),
        (503, "the new account inventory is unavailable"),
    ]);
    let slug = ModelSlug::parse("openrouter/openai/gpt-6-luna").unwrap();
    assert!(block_on(catalogue.resolve(&slug, &http, &credential())).is_ok());
    let next_credential = CredentialStrategy::BearerFromEnv {
        var: "EGO_TEST_KEY".into(),
    }
    .resolve(&FakeCredentials::holding("different-account-key"))
    .unwrap();

    assert!(
        matches!(
            block_on(catalogue.resolve(&slug, &http, &next_credential)),
            Err(ResolveError::EndpointUnreachable { .. })
        ),
        "changed credentials must not resolve using the previous account's inventory"
    );
    assert_eq!(stub.paths(), vec!["/api/v1/models", "/api/v1/models"]);
}

/// Catches poisoning the scope with a failed first fetch or inventing absent models.
#[test]
fn failed_inventory_is_not_retained_and_missing_models_still_refuse() {
    let catalogue = Catalogue::new([ProviderRow::openrouter_default()]);
    let (http, stub) = server([
        (503, "inventory temporarily unavailable"),
        (
            200,
            include_str!("goldens/openrouter/models-2026-10-15.json"),
        ),
    ]);
    let slug = ModelSlug::parse("openrouter/openai/gpt-6-luna").unwrap();
    assert!(matches!(
        block_on(catalogue.resolve(&slug, &http, &credential())),
        Err(ResolveError::EndpointUnreachable { .. })
    ));
    assert!(block_on(catalogue.resolve(&slug, &http, &credential())).is_ok());
    let absent = ModelSlug::parse("openrouter/fixture/not-in-inventory").unwrap();
    assert!(matches!(
        block_on(catalogue.resolve(&absent, &http, &credential())),
        Err(ResolveError::NotInstalled { .. })
    ));
    assert_eq!(stub.paths(), vec!["/api/v1/models", "/api/v1/models"]);
}

/// Catches reusing inventory when an effective account or routing header changes.
#[test]
fn changed_inventory_headers_refetch_even_with_the_same_endpoint_and_credential() {
    use ego_ai::catalogue::ResolutionInventory;
    let inventory = ResolutionInventory::default();
    let (http, stub) = server([
        (
            200,
            include_str!("goldens/openrouter/models-2026-10-15.json"),
        ),
        (503, "the new header scope inventory is unavailable"),
    ]);
    let slug = ModelSlug::parse("openrouter/openai/gpt-6-luna").unwrap();
    let catalogue = Catalogue::new([ProviderRow::openrouter_default()])
        .with_resolution_inventory(inventory.clone());
    assert!(block_on(catalogue.resolve(&slug, &http, &credential())).is_ok());
    let mut row = ProviderRow::openrouter_default();
    row.extra_headers
        .push(("x-account".into(), "other-account".into()));
    let changed = Catalogue::new([row]).with_resolution_inventory(inventory);
    assert!(matches!(
        block_on(changed.resolve(&slug, &http, &credential())),
        Err(ResolveError::EndpointUnreachable { .. })
    ));
    assert_eq!(stub.paths(), vec!["/api/v1/models", "/api/v1/models"]);
}
