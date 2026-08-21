//! What xAI's published price list becomes, and the four ways it lies.
//!
//! `docs/decisions/2026-08-21-grok-as-a-provider.md` §3. xAI is the one
//! provider of the six that publishes what it will charge **the key that
//! asks**, which is why it gets a resolver at all — a figure from the party
//! that sends the invoice is better evidence than a rate this repository
//! copied down and hopes is current.
//!
//! It is better evidence *only if its sentinels are decoded*, and that is
//! what this file is for. Four of these tests would pass against a decoder
//! that under-reserves by a factor of two, or by all of it:
//!
//! - the unit, if the scale were wrong;
//! - the tier, if the base figure were taken because it is listed first;
//! - the two zeros, if `0` were read as a price rather than as "absent";
//! - the cache write, which xAI neither reports nor prices.
//!
//! The transport is a stub. Nothing here is a claim that xAI answers this
//! way: `crates/ego-ai/tests/goldens/profile/xai/` records what the schema
//! documents, and the row stays out of `crates/ego-cli/src/model.rs` until a
//! live recording exists
//! (`2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §9).

use ego_ai::catalogue::{
    Catalogue, HostedModel, ModelSlug, ResolveError, operator_rates, tabulated,
};
use ego_ai::provider::ProviderRow;
use ego_core::budget::{PricingQuote, ResolvedRates};
use ego_core::cost::{Amount, CurrencyCode, PricingRevision, RateSource};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use futures::executor::block_on;
use url::Url;

mod support;
use support::{TEST_KEY, credential, server};

/// The slug every test resolves.
const SLUG: &str = "xai/grok-4";

/// A price list in the shape `GET /v1/language-models` documents, with a
/// long-context tier at every category.
///
/// The figures are Grok 4's published ones in the endpoint's own unit — USD
/// cents per 100M tokens — so `30000` is $3 per million input tokens and the
/// arithmetic below is checkable against a public price page rather than
/// against itself.
const PRICED: &str = r#"{"models":[
  {"id":"grok-4",
   "prompt_text_token_price":30000,
   "cached_prompt_text_token_price":7500,
   "completion_text_token_price":150000,
   "prompt_text_token_price_long_context":60000,
   "cached_prompt_text_token_price_long_context":15000,
   "completion_text_token_price_long_context":300000,
   "long_context_threshold":128000,
   "fingerprint":"fp-77","version":"3"}
]}"#;

/// The same list with no long-context tier at all, spelled the way the schema
/// spells it: zeros.
const FLAT: &str = r#"{"models":[
  {"id":"grok-4",
   "prompt_text_token_price":30000,
   "cached_prompt_text_token_price":7500,
   "completion_text_token_price":150000,
   "long_context_threshold":0,
   "fingerprint":"fp-77","version":"3"}
]}"#;

/// Figures with no threshold behind them — the *third* combination, and the
/// only one in which the zero-threshold sentinel is load-bearing.
///
/// `FLAT` zeroes both, so a decoder that had dropped the threshold check
/// entirely would still take the base figures there, on the strength of the
/// twin check alone. This list is what tells the two apart.
const FIGURES_WITHOUT_THRESHOLD: &str = r#"{"models":[
  {"id":"grok-4",
   "prompt_text_token_price":30000,
   "cached_prompt_text_token_price":7500,
   "completion_text_token_price":150000,
   "prompt_text_token_price_long_context":60000,
   "cached_prompt_text_token_price_long_context":15000,
   "completion_text_token_price_long_context":300000,
   "long_context_threshold":0,
   "fingerprint":"fp-77","version":"3"}
]}"#;

/// A threshold with no figures behind it — the other zero, and a different
/// fact from the one above.
const THRESHOLD_WITHOUT_FIGURES: &str = r#"{"models":[
  {"id":"grok-4",
   "prompt_text_token_price":30000,
   "cached_prompt_text_token_price":7500,
   "completion_text_token_price":150000,
   "prompt_text_token_price_long_context":0,
   "cached_prompt_text_token_price_long_context":0,
   "completion_text_token_price_long_context":0,
   "long_context_threshold":128000}
]}"#;

/// A list this decoder does not understand: the input price is simply absent.
const NO_INPUT_PRICE: &str = r#"{"models":[
  {"id":"grok-4",
   "cached_prompt_text_token_price":7500,
   "completion_text_token_price":150000,
   "long_context_threshold":0}
]}"#;

/// A list that prices something else.
const ANOTHER_MODEL: &str = r#"{"models":[
  {"id":"grok-3-mini",
   "prompt_text_token_price":3000,
   "cached_prompt_text_token_price":750,
   "completion_text_token_price":50000,
   "long_context_threshold":0}
]}"#;

// --- fixtures ---

fn usd() -> CurrencyCode {
    CurrencyCode::new("USD").expect("USD is valid")
}

/// A rate per single token, from the endpoint's cents-per-100M figure.
///
/// Spelled as a decimal string rather than as `Amount::new(figure, 10)`,
/// which would be the decoder's own arithmetic asserting itself. `30000`
/// cents per 100M tokens is $3 per million is `0.000003` per token, and that
/// last step is the one a reader can check.
fn rate(per_token: &str) -> Amount {
    per_token.parse().expect("a well-formed decimal")
}

/// A row pointed at the stub, on a base carrying the `/v1/` prefix a real
/// deployment does.
fn row() -> ProviderRow {
    ProviderRow::xai(Url::parse("http://127.0.0.1:9/v1/").expect("a well-formed base"))
}

fn caps(max_context_tokens: Option<u64>) -> ModelCapabilities {
    ModelCapabilities {
        schema_version: 1,
        max_context_tokens,
        max_output_tokens: Some(32_000),
        max_tools: None,
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::SameProviderOnly,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
    }
}

/// A catalogue holding the xAI row and one supplied model, with no rates —
/// the case the resolver exists for.
fn catalogue(max_context_tokens: Option<u64>) -> Catalogue {
    supplied(max_context_tokens, None)
}

fn supplied(max_context_tokens: Option<u64>, rates: Option<ResolvedRates>) -> Catalogue {
    Catalogue::new([row()]).with_hosted(
        SLUG,
        HostedModel {
            capabilities: caps(max_context_tokens),
            metering: tabulated(),
            rates,
        },
    )
}

/// Resolve `SLUG` against a scripted answer, returning the quote.
fn quote_from(answer: &'static str, max_context_tokens: Option<u64>) -> PricingQuote {
    let (http, _) = server([(200, answer)]);
    let resolved = block_on(catalogue(max_context_tokens).resolve(
        &ModelSlug::parse(SLUG).unwrap(),
        &http,
        &credential(),
    ))
    .expect("the list prices it");
    resolved.rates.expect("a price was discovered").quote
}

// --- the unit ---

#[test]
fn a_published_figure_becomes_a_rate_per_single_token() {
    // The endpoint publishes USD cents per 100M tokens and a `PricingQuote`
    // holds a rate per *one* token, which is a factor of 10^10. Getting it
    // wrong in either direction is silent: the gate still admits requests, it
    // just admits the wrong ones.
    let quote = quote_from(FLAT, Some(256_000));

    assert_eq!(quote.currency, usd());
    assert_eq!(quote.uncached_input, Some(rate("0.000003")));
    assert_eq!(quote.cache_read, Some(rate("0.00000075")));
    assert_eq!(quote.output, Some(rate("0.000015")));
}

#[test]
fn the_conversion_is_a_scale_and_never_a_division() {
    // A figure that does not divide evenly is the case a `/ 100_000_000`
    // would lose, and `Amount` holds twelve places, so an odd cent count has
    // to survive exactly.
    const ODD: &str = r#"{"models":[
      {"id":"grok-4","prompt_text_token_price":1,
       "cached_prompt_text_token_price":3,"completion_text_token_price":7,
       "long_context_threshold":0}
    ]}"#;

    let quote = quote_from(ODD, Some(256_000));

    assert_eq!(quote.uncached_input, Some(rate("0.0000000001")));
    assert_eq!(quote.cache_read, Some(rate("0.0000000003")));
    assert_eq!(quote.output, Some(rate("0.0000000007")));
}

// --- the tier ---

#[test]
fn a_model_that_can_reach_the_long_tier_is_priced_at_it() {
    // The window is above the threshold, so every request this model can make
    // *may* be a long-context one, and a quote holds one rate per category.
    // `PricingQuote::upper_bound` already reserves against a full window at
    // the dearest rate; the tier that matches that assumption is the long one.
    let quote = quote_from(PRICED, Some(256_000));

    assert_eq!(quote.uncached_input, Some(rate("0.000006")));
    assert_eq!(quote.cache_read, Some(rate("0.0000015")));
    assert_eq!(quote.output, Some(rate("0.00003")));
}

#[test]
fn a_window_provably_below_the_threshold_is_priced_at_the_base_tier() {
    // The one case that takes the cheaper figure on purpose: this model
    // cannot reach the long tier, so reserving against it would refuse
    // requests the provider would have accepted.
    let quote = quote_from(PRICED, Some(32_768));

    assert_eq!(quote.uncached_input, Some(rate("0.000003")));
    assert_eq!(quote.cache_read, Some(rate("0.00000075")));
    assert_eq!(quote.output, Some(rate("0.000015")));
}

#[test]
fn a_window_exactly_at_the_threshold_is_long_context() {
    // The boundary, decided rather than left to whichever comparison got
    // typed: `long_context_threshold` is the length at which the long rate
    // starts, so a window that reaches it reaches the tier. Off by one here
    // under-reserves every request of exactly that length.
    let quote = quote_from(PRICED, Some(128_000));

    assert_eq!(quote.uncached_input, Some(rate("0.000006")));
}

#[test]
fn an_unknown_window_takes_the_dearer_tier() {
    // "We do not know how big this model's context is" is not evidence that
    // it is small. An unsupplied window cannot actually reach here — hosted
    // capabilities are required — but the collapse must not be the place that
    // assumption is load-bearing.
    let quote = quote_from(PRICED, None);

    assert_eq!(quote.uncached_input, Some(rate("0.000006")));
}

// --- the zeros ---

#[test]
fn a_zero_threshold_is_no_long_tier_and_not_a_free_one() {
    // `FLAT` carries no long-context figures and a zero threshold. Decoding
    // that as "the long rate is zero" would price every request on a model
    // with a large window at nothing at all.
    let quote = quote_from(FLAT, Some(256_000));

    assert_eq!(quote.uncached_input, Some(rate("0.000003")));
    assert_ne!(quote.uncached_input, Some(Amount::ZERO));
    assert_ne!(quote.output, Some(Amount::ZERO));
}

#[test]
fn a_zero_threshold_beats_a_long_figure_that_is_published_anyway() {
    // The case that makes the threshold check load-bearing rather than
    // redundant, and it was missing: `FLAT` zeroes the threshold *and* the
    // twins, so a decoder that consulted only the twins passed it. Here the
    // twins are real and the threshold says there is no tier for them to
    // apply at — a length no request can reach, since a threshold of zero
    // would otherwise mean "every request qualifies" and reserve double for
    // every model that publishes rates it never charges.
    let quote = quote_from(FIGURES_WITHOUT_THRESHOLD, Some(256_000));

    assert_eq!(quote.uncached_input, Some(rate("0.000003")));
    assert_eq!(quote.cache_read, Some(rate("0.00000075")));
    assert_eq!(quote.output, Some(rate("0.000015")));
}

#[test]
fn a_zero_long_figure_means_the_base_applies_at_every_length() {
    // The other zero, and a different fact: there *is* a threshold, and above
    // it the same figures apply. A decoder that trusted the twin because the
    // threshold was non-zero would price this model's dearest requests at
    // nothing.
    let quote = quote_from(THRESHOLD_WITHOUT_FIGURES, Some(256_000));

    assert_eq!(quote.uncached_input, Some(rate("0.000003")));
    assert_eq!(quote.cache_read, Some(rate("0.00000075")));
    assert_eq!(quote.output, Some(rate("0.000015")));
}

// --- the cache write ---

#[test]
fn a_cache_write_is_billed_as_uncached_input() {
    // xAI neither reports cache-write tokens nor prices them. `None` would
    // fail the gate closed for every xAI request, because
    // `dearest_input_rate` consults all three input categories
    // unconditionally; zero would assert xAI writes caches for free, which
    // nothing published says. The uncached rate is what a write actually is.
    let quote = quote_from(FLAT, Some(256_000));

    assert_eq!(quote.cache_write, quote.uncached_input);
    assert_eq!(quote.cache_write, Some(rate("0.000003")));
    assert_eq!(
        quote.cache_write_extended, None,
        "nothing on this wire distinguishes an extended write"
    );
}

#[test]
fn the_cache_write_follows_the_tier_the_input_was_priced_at() {
    // Not a separate lookup: if the input collapsed upward, so does the write
    // that is billed as input. A write pinned to the base figure while the
    // input took the long one would under-reserve exactly the long requests.
    let quote = quote_from(PRICED, Some(256_000));

    assert_eq!(quote.cache_write, Some(rate("0.000006")));
}

// --- provenance ---

#[test]
fn a_discovered_price_says_the_provider_published_it() {
    // A ceiling enforced against an operator's negotiated table and one
    // enforced against a public price page are different guarantees, and a
    // consumer that cannot tell them apart cannot report honestly on either.
    let (http, _) = server([(200, PRICED)]);

    let resolved = block_on(catalogue(Some(256_000)).resolve(
        &ModelSlug::parse(SLUG).unwrap(),
        &http,
        &credential(),
    ))
    .expect("the list prices it");

    assert_eq!(
        resolved.rates.expect("discovered").source,
        RateSource::ProviderPublished
    );
}

#[test]
fn the_revision_names_the_published_table_it_came_from() {
    // A date says when a figure was copied; a fingerprint and a version say
    // *which table* it was read from, which is what a revision always wanted
    // to mean.
    let quote = quote_from(PRICED, Some(256_000));

    assert_eq!(
        quote.revision,
        PricingRevision("xai/language-models/grok-4/fp-77/3".to_string())
    );
}

#[test]
fn an_unstamped_table_says_so_rather_than_inventing_a_pin() {
    // A revision that claimed to be pinned when the endpoint stamped nothing
    // would be worse than one that admits it is not: the first is a false
    // attribution, the second is a true one.
    let quote = quote_from(THRESHOLD_WITHOUT_FIGURES, Some(256_000));

    assert_eq!(
        quote.revision,
        PricingRevision("xai/language-models/grok-4/unstamped".to_string())
    );
}

// --- the credential, and the request ---

#[test]
fn the_credential_this_run_resolved_is_what_asks_for_the_price() {
    // The endpoint publishes prices *per key*, so asking it without the
    // credential the generation will use would discover somebody else's
    // rates. Reading the environment again inside the resolver would be a
    // second read of a variable that may have been rotated
    // (`2026-08-21-a-shared-shape-is-not-a-shared-contract.md` §6), so it
    // arrives as a borrow of the value the caller already has.
    let (http, stub) = server([(200, PRICED)]);

    block_on(catalogue(Some(256_000)).resolve(
        &ModelSlug::parse(SLUG).unwrap(),
        &http,
        &credential(),
    ))
    .expect("the list prices it");

    let asked = stub.asked();
    assert_eq!(asked.len(), 1, "one question, one request");
    assert_eq!(
        asked[0].path, "/v1/language-models",
        "the path is joined onto the row's base like every other path"
    );
    assert!(
        asked[0]
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value.contains(TEST_KEY)),
        "the resolved credential did not reach the wire: {:?}",
        asked[0].headers
    );
}

#[test]
fn an_operator_who_wrote_the_rates_down_is_not_second_guessed() {
    // `2026-08-21-a-run-with-no-ceiling.md` §5 orders the three sources with
    // the operator first, first hit wins — and the reason survives contact
    // with a live endpoint: a published figure is what xAI charges the
    // public, and an operator may hold a negotiated one no endpoint admits
    // to. The stub is scripted with nothing, so a resolution that asked
    // anyway would fail here.
    let (http, stub) = server([]);
    let theirs = operator_rates(PricingQuote {
        revision: PricingRevision("negotiated".to_string()),
        currency: usd(),
        uncached_input: Some(rate("0.0000001")),
        cache_read: Some(rate("0.0000001")),
        cache_write: Some(rate("0.0000001")),
        cache_write_extended: None,
        output: Some(rate("0.0000005")),
    });

    let resolved = block_on(supplied(Some(256_000), Some(theirs.clone())).resolve(
        &ModelSlug::parse(SLUG).unwrap(),
        &http,
        &credential(),
    ))
    .expect("the operator supplied rates");

    assert_eq!(resolved.rates, Some(theirs));
    assert!(
        stub.asked().is_empty(),
        "the endpoint was asked to second-guess an operator: {:?}",
        stub.paths()
    );
}

#[test]
fn a_model_with_no_supplied_capabilities_is_refused_before_the_request() {
    // Capabilities are not discoverable here — `language-models` publishes no
    // context window, no output ceiling and no reasoning field — so a slug
    // nobody configured could not have been used whatever it costs. Refusing
    // first names the missing configuration instead of spending a round trip
    // to find out.
    let (http, stub) = server([(200, PRICED)]);
    let bare = Catalogue::new([row()]);

    let refused = block_on(bare.resolve(&ModelSlug::parse(SLUG).unwrap(), &http, &credential()))
        .expect_err("nothing was supplied for it");

    assert_eq!(
        refused,
        ResolveError::HostedModelNotConfigured {
            slug: SLUG.to_string()
        }
    );
    assert!(
        stub.asked().is_empty(),
        "a price was fetched for a model that could not have been used"
    );
}

// --- what a refusal has to say ---

/// Resolve against a scripted answer and return the refusal.
fn refusal_from(answers: Vec<(u16, &'static str)>) -> ResolveError {
    let (http, _) = server(answers);
    block_on(catalogue(Some(256_000)).resolve(
        &ModelSlug::parse(SLUG).unwrap(),
        &http,
        &credential(),
    ))
    .expect_err("the list does not price it")
}

#[test]
fn an_endpoint_that_does_not_answer_is_named_in_the_refusal() {
    // An operator whose price discovery failed needs a URL to go and try. The
    // variant is `EndpointUnreachable` rather than a daemon-shaped one for
    // exactly this: two resolvers ask two different kinds of server, and both
    // refusals have to name an address.
    let refused = refusal_from(vec![]);

    match &refused {
        ResolveError::EndpointUnreachable { endpoint, .. } => {
            assert_eq!(endpoint, "http://127.0.0.1:9/v1/language-models");
        }
        other => panic!("expected an unreachable endpoint, got {other:?}"),
    }
    assert!(
        refused.to_string().contains("127.0.0.1:9"),
        "the refusal prints the address: {refused}"
    );
}

#[test]
fn a_price_list_that_does_not_parse_is_a_refusal_and_not_a_free_model() {
    let refused = refusal_from(vec![(200, "not json at all")]);

    assert!(
        matches!(refused, ResolveError::EndpointUnreachable { .. }),
        "got {refused:?}"
    );
    assert!(
        refused.to_string().contains("did not parse"),
        "the refusal says what went wrong: {refused}"
    );
}

#[test]
fn a_403_is_a_refusal_that_carries_what_the_endpoint_said() {
    // The likeliest real failure: a key that cannot read the price list. The
    // status and the body are what tell an operator whether to fix the key or
    // the account, so neither is swallowed.
    let refused = refusal_from(vec![(403, r#"{"error":"insufficient scope"}"#)]);

    assert!(
        refused.to_string().contains("403") && refused.to_string().contains("insufficient scope"),
        "the refusal relays the answer: {refused}"
    );
}

#[test]
fn a_response_missing_a_base_price_is_refused_rather_than_read_as_free() {
    // The three base figures deliberately carry no `#[serde(default)]`: a
    // response without them is a response this decoder does not understand,
    // and defaulting would turn "we no longer speak this schema" into "this
    // model is free".
    let refused = refusal_from(vec![(200, NO_INPUT_PRICE)]);

    assert!(
        matches!(refused, ResolveError::EndpointUnreachable { .. }),
        "got {refused:?}"
    );
}

#[test]
fn a_model_the_endpoint_does_not_price_is_refused_naming_what_it_does() {
    // The list is the operator's evidence about what they can actually
    // select, and a refusal that withheld it would send them to a dashboard
    // to find out what this request already knows.
    let refused = refusal_from(vec![(200, ANOTHER_MODEL)]);

    assert!(
        refused.to_string().contains("grok-3-mini"),
        "the refusal lists what is priced: {refused}"
    );
}

#[test]
fn an_empty_price_list_is_refused_and_says_so() {
    let refused = refusal_from(vec![(200, r#"{"models":[]}"#)]);

    assert!(
        refused.to_string().contains("no price for `grok-4`"),
        "got {refused}"
    );
}

// --- what the row is, and is not ---

#[test]
fn the_xai_row_speaks_the_family_mapping_and_states_its_own_charge() {
    // §7 puts xAI on `openai-completions` from its published schema, and its
    // row carries the decoder for `usage.cost_in_usd_ticks` — the figure that
    // *settles* a generation, which is a different thing from the quote that
    // bounds one. Both exist here, and the pair is the point: reading a cost
    // is not believing it, and whether it settles is the gate's decision.
    let row = row();

    assert_eq!(row.id, "xai");
    assert!(row.cost.is_some(), "xAI states a charge per request");
    assert_eq!(
        row.cost.as_ref().unwrap().revision().field,
        "usage.cost_in_usd_ticks"
    );
}

#[test]
fn the_openrouter_row_is_built_and_deliberately_not_discovering() {
    // OpenRouter's models endpoint publishes a *public* table rather than
    // what this key is charged, which is the property that earned xAI a
    // resolver. A public table read live is a third-party table with extra
    // steps, and §5 ranks it below the operator's figures either way — so
    // this row supplies, and it is not an oversight.
    let row = ProviderRow::openrouter_default();

    assert_eq!(row.id, "openrouter");
    assert_eq!(
        row.cost
            .as_ref()
            .expect("it states a charge")
            .revision()
            .field,
        "usage.cost"
    );
}
