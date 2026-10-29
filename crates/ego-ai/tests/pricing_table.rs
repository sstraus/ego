//! Reading a model's rates out of the litellm document.
//!
//! The fixture is seven real entries lifted verbatim from the 1.8 MB file,
//! trimmed to the seven fields this decoder reads. Real rather than invented,
//! because every awkward property below is the artefact's and none of them
//! would have occurred to someone writing a plausible sample:
//!
//! - keys are sometimes bare (`gpt-5`, `claude-opus-4-5`) and sometimes
//!   prefixed with *litellm's* provider name (`gemini/…`, `xai/…`), and which
//!   of the two a provider gets is not a rule anyone stated;
//! - `gpt-5` states no cache-write rate at all, which is the measured case for
//!   **0 of 94** OpenAI entries having all five categories;
//! - `gemini-exp-1206` exists under *both* spellings with **different prices**,
//!   one of them zero.
//!
//! The whole table is not committed. It is 1.8 MB, its prices change weekly,
//! and a fixture that has to be refreshed to stay passing stops being evidence
//! about the decoder.

use ego_ai::catalogue;
use ego_ai::pricing::{Table, TableError};
use ego_core::cost::{CurrencyCode, PricingRevision, RateSource};

fn table() -> Table {
    Table::decode(include_bytes!("goldens/pricing/table.json")).expect("the fixture is a table")
}

/// `Amount` compares by value; the assertions read its canonical spelling so a
/// failure prints the price rather than a coefficient and a scale.
#[track_caller]
fn rates(provider: &str, model: &str) -> [Option<String>; 5] {
    let entry = table()
        .quote(provider, model)
        .unwrap_or_else(|err| panic!("{provider}/{model}: {err}"));
    [
        entry.uncached_input,
        entry.cache_read,
        entry.cache_write,
        entry.cache_write_extended,
        entry.output,
    ]
    .map(|rate| rate.map(|amount| amount.to_string()))
}

/// The complete case, and the only provider of ego's six the table prices in
/// full.
///
/// Measured: **24 of 26** Anthropic entries state all five categories, against
/// 0 of 94 for OpenAI, 0 of 51 for Gemini and 0 of 44 for xAI. The reason is
/// not that litellm neglected the others — Anthropic is the one provider that
/// charges separately to *write* the cache, so it is the one whose table entry
/// has a fifth number to carry.
#[test]
fn a_bare_key_resolves_under_its_stated_provider() {
    assert_eq!(
        rates("anthropic", "claude-opus-4-5"),
        [
            Some("0.000005".to_string()),
            Some("0.0000005".to_string()),
            Some("0.00000625".to_string()),
            Some("0.00001".to_string()),
            Some("0.000025".to_string()),
        ]
    );
}

/// The prefixed spelling resolves to the same question asked the same way.
///
/// The caller says `gemini` and `gemini-3-pro-preview` — ego's own halves —
/// and never has to know that this provider's keys carry a prefix and
/// Anthropic's do not. That asymmetry is litellm's and it stops here.
#[test]
fn a_prefixed_key_resolves_from_the_same_two_halves() {
    assert_eq!(
        rates("gemini", "gemini-3-pro-preview"),
        [
            Some("0.000002".to_string()),
            Some("0.0000002".to_string()),
            None,
            None,
            Some("0.000012".to_string()),
        ]
    );
}

/// An absent rate decodes as **absent**, never as zero.
///
/// An absent cache-write rate stays absent, not zero. Unknown price and
/// known zero price are different facts throughout cost settlement.
///
/// What that absence *means* for a ceiling is not decided here. This decoder
/// reports what the document says; the resolver decides what follows.
#[test]
fn a_category_the_table_does_not_state_is_absent_and_not_zero() {
    let [input, read, write, extended, output] = rates("openai", "gpt-5");
    assert_eq!(input, Some("0.00000125".to_string()));
    assert_eq!(read, Some("0.000000125".to_string()));
    assert_eq!(write, None, "no cache-write rate is stated");
    assert_eq!(extended, None);
    assert_eq!(output, Some("0.00001".to_string()));
}

/// And a *stated* zero is a zero, so the rule above does not erase a fact.
///
/// litellm writes `0.0` for locally served rows. ego never asks the table
/// about one — a local row is `Metering::Unmetered` and consults no rates at
/// all — so this cannot mislead a budget today. It is pinned because the two
/// spellings must stay distinguishable at the seam, not because anything
/// downstream currently depends on it.
#[test]
fn a_stated_zero_survives_as_a_price_of_zero() {
    let [input, .., output] = rates("ollama", "llama3");
    assert_eq!(input, Some("0".to_string()));
    assert_eq!(output, Some("0".to_string()));
}

/// A model the table does not price is named, not silently unpriced.
///
/// `UnknownCost::NoPricingQuote` must answer only "you asked for a ceiling on
/// a model I cannot price" (034's criterion 4), and it can only do that if the
/// layer below distinguishes "not in the table" from "in the table with no
/// rates". This is that distinction.
#[test]
fn a_model_the_table_does_not_carry_is_named() {
    let err = table()
        .quote("openai", "gpt-99")
        .expect_err("no such entry");
    let TableError::NoEntry { provider, model } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(provider, "openai");
    assert_eq!(model, "gpt-99");
}

/// The provider half is part of the lookup, not decoration.
///
/// `claude-opus-4-5` is a real key and its entry says `litellm_provider:
/// "anthropic"`. Asking for it under `openai` must fail: a bare key matched on
/// its name alone would let one provider's slug pick up another's prices, and
/// the table has 576 bare keys for that to happen among.
#[test]
fn a_bare_key_belonging_to_another_provider_does_not_match() {
    let err = table()
        .quote("openai", "claude-opus-4-5")
        .expect_err("that key is anthropic's");
    assert!(matches!(err, TableError::NoEntry { .. }), "{err}");
}

/// Refuse contradictory prices for two keys naming the same model. A zero alias can
/// under-reserve; selecting a dearer entry invents authority. Name both keys so the operator
/// can correct the table or supply explicit rates.
#[test]
fn two_spellings_of_one_model_that_disagree_are_refused_rather_than_picked() {
    let err = table()
        .quote("gemini", "gemini-exp-1206")
        .expect_err("the table contradicts itself here");
    let TableError::Contradiction { model, keys } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(model, "gemini-exp-1206");
    assert!(
        keys.contains(&"gemini/gemini-exp-1206".to_string())
            && keys.contains(&"gemini-exp-1206".to_string()),
        "the refusal must name both spellings: {keys:?}"
    );
}

/// A rate that is not a number refuses *that model*, not the whole table.
///
/// Decoding is lazy on purpose. The document carries 3111 entries and ego asks
/// about one; a decoder that parsed every rate up front would let a malformed
/// figure on a model nobody selected refuse a run that never needed it. The
/// blast radius of a bad entry is the entry.
#[test]
fn a_malformed_rate_refuses_its_own_model_and_leaves_the_rest_readable() {
    let bytes = br#"{
      "good": {"input_cost_per_token": 1e-06, "output_cost_per_token": 2e-06,
               "litellm_provider": "acme", "mode": "chat"},
      "bad":  {"input_cost_per_token": "free", "output_cost_per_token": 2e-06,
               "litellm_provider": "acme", "mode": "chat"}
    }"#;
    let table = Table::decode(bytes).expect("the document itself is well-formed");

    let err = table
        .quote("acme", "bad")
        .expect_err("that rate is a string");
    let TableError::Rate { key, field, .. } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(key, "bad");
    assert_eq!(field, "input_cost_per_token");

    assert!(
        table.quote("acme", "good").is_ok(),
        "one bad entry must not cost the table its other rows"
    );
}

/// A document that is not JSON is refused as a document.
#[test]
fn a_body_that_is_not_json_is_refused_before_any_model_is_asked_for() {
    let err = Table::decode(b"<html>rate limited</html>").expect_err("not a table");
    assert!(matches!(err, TableError::NotATable { .. }), "{err}");
}

/// The document carries entries that are not models, and they are tolerated.
///
/// Re-measured on 2026-08-22 rather than left as written: the real file's one
/// non-model key today is `fallback_generalizations`, whose value is a
/// `{"rules": [...]}` object sharing not one field with a price row. An
/// earlier version of this test named `sample_spec` — litellm's old
/// field-listing stub — and that key is **no longer in the artefact**, so the
/// comment was making a false claim about a file nobody re-read.
///
/// What is pinned is unchanged and is the point: a key with no
/// `litellm_provider` cannot be reached by a provider-matched lookup anyway,
/// so the decoder must ignore it rather than choke on an entry shaped unlike
/// the rest. `#[serde(deny_unknown_fields)]` is deliberately absent from `Row`
/// for the same reason.
#[test]
fn an_entry_with_no_provider_is_ignored_rather_than_fatal() {
    let bytes = br#"{
      "fallback_generalizations": {"rules": [{"name": "bedrock-claude-ids"}]},
      "real": {"input_cost_per_token": 1e-06, "litellm_provider": "acme", "mode": "chat"}
    }"#;
    let table = Table::decode(bytes).expect("an odd entry is not a broken document");
    assert!(table.quote("acme", "real").is_ok());
    assert!(matches!(
        table.quote("", "fallback_generalizations"),
        Err(TableError::NoEntry { .. })
    ));
}

/// Only chat rows answer, because only chat rows are what `--model` selects.
///
/// The file mixes embeddings, rerankers, image and audio rows into the same
/// map, and an image row is priced per *image* while carrying the same field
/// names. Reading one as a per-token rate would produce a number that is
/// wrong by orders of magnitude and looks entirely plausible.
#[test]
fn a_row_that_is_not_a_chat_model_does_not_answer() {
    let bytes = br#"{
      "acme/embed": {"input_cost_per_token": 1e-07, "litellm_provider": "acme",
                     "mode": "embedding"}
    }"#;
    let table = Table::decode(bytes).expect("a table");
    assert!(matches!(
        table.quote("acme", "embed"),
        Err(TableError::NoEntry { .. })
    ));
}

/// Every provider id this build ships is litellm's own namespace, verbatim.
///
/// Measured against the whole 1.8 MB artefact before it was believed: all six
/// of ego's ids — `openai`, `ollama`, `xai`, `openrouter`, `anthropic`,
/// `gemini` — appear unchanged as `litellm_provider` values. So there is **no
/// local-id-to-table-namespace map**, and this test is why one must not be
/// added on a hunch: it would be a translation layer with nothing to
/// translate, and the first wrong entry in it would silently mis-price a row.
///
/// The fixture carries one real entry per id for that reason, and the
/// openrouter one is `anthropic/claude-opus-4.1` — a model half that itself
/// contains a slash, which `ModelSlug::parse` splits on the *first* separator
/// only. A lookup that re-split it would ask openrouter for `anthropic`.
///
/// It fails the day litellm renames one of the six, which is the day a map
/// becomes justified — with one measured entry in it rather than six guessed.
#[test]
fn every_provider_id_this_build_ships_is_the_tables_own_namespace() {
    let table = table();
    for (provider, model) in [
        ("openai", "gpt-5"),
        ("ollama", "llama3"),
        ("xai", "grok-4"),
        ("openrouter", "anthropic/claude-opus-4.1"),
        ("anthropic", "claude-opus-4-5"),
        ("gemini", "gemini-3-pro-preview"),
    ] {
        assert!(
            table.quote(provider, model).is_ok(),
            "{provider}/{model} did not resolve under ego's own provider id"
        );
    }
}

/// The entry becomes a quote denominated in dollars and attributed to the
/// table revision it was read from.
///
/// The currency is not in the artefact — no entry states one — so it is this
/// crate's assertion about the document rather than a decoded field, written
/// down once where a table in another currency would have one place to be
/// wrong instead of every call site.
#[test]
fn a_table_entry_becomes_a_dollar_quote_that_names_the_table_it_came_from() {
    let revision = PricingRevision("litellm@1.79.3#9f86d081".to_string());
    let quote = table()
        .quote("anthropic", "claude-opus-4-5")
        .expect("the fixture prices it")
        .into_quote(revision.clone());

    assert_eq!(quote.revision, revision);
    assert_eq!(
        quote.currency,
        CurrencyCode::new("USD").expect("USD is a code")
    );
    assert_eq!(
        quote.uncached_input.map(|a| a.to_string()),
        Some("0.000005".to_string())
    );
    assert_eq!(
        quote.cache_read.map(|a| a.to_string()),
        Some("0.0000005".to_string())
    );
    assert_eq!(
        quote.cache_write.map(|a| a.to_string()),
        Some("0.00000625".to_string())
    );
    assert_eq!(
        quote.cache_write_extended.map(|a| a.to_string()),
        Some("0.00001".to_string())
    );
    assert_eq!(
        quote.output.map(|a| a.to_string()),
        Some("0.000025".to_string())
    );
}

/// Keep omitted categories absent. A money ceiling with an unpriced input category cannot
/// establish a bound; operator configuration can supply it. Do not infer model-specific
/// cache-write rates from provider identity.
#[test]
fn a_rate_the_table_omits_is_absent_in_the_quote_and_bounds_nothing() {
    let quote = table()
        .quote("openai", "gpt-5")
        .expect("the fixture prices it")
        .into_quote(PricingRevision("litellm@1.79.3#9f86d081".to_string()));

    assert_eq!(quote.cache_write, None, "unstated, and therefore unpriced");
    assert_eq!(quote.cache_write_extended, None);
    assert!(
        quote.uncached_input.is_some(),
        "what was stated is still there"
    );
}

/// The provenance and the quote name one table, because one mints the other.
///
/// `RateSource::Table` takes its revision from the table itself. Constructing
/// the attribution independently could let persisted source and revision
/// disagree about which prices constrained the run.
#[test]
fn the_rate_source_cannot_disagree_with_its_quote_about_which_table() {
    let revision = PricingRevision("litellm@1.79.3#9f86d081".to_string());
    let quote = table()
        .quote("anthropic", "claude-opus-4-5")
        .expect("the fixture prices it")
        .into_quote(revision.clone());
    let resolved = catalogue::table_rates(quote);

    assert_eq!(resolved.source, RateSource::Table { revision });
    assert_eq!(
        resolved.source,
        RateSource::Table {
            revision: resolved.quote.revision.clone()
        }
    );
}
