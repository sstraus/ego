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

use ego_ai::pricing::{Table, TableError};

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
/// `gpt-5` states no cache-write rate. `docs/decisions/2026-08-21-how-a-model-is-metered.md`
/// §9 requires an absent figure to decode as absent, and `cost.rs:335-339`
/// says why: a price list saying zero per token and a row that has no
/// per-token price at all are different facts, and once they share a spelling
/// nothing downstream can tell them apart.
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

/// **The finding this test exists for.** Two spellings of one model that
/// disagree are refused, not resolved by preference.
///
/// `gemini-exp-1206` appears twice in the real table. `gemini/gemini-exp-1206`
/// prices it at **zero**; the bare `gemini-exp-1206` prices it at `3e-07` in
/// and `2.5e-06` out. Four models in the file disagree with themselves this
/// way, and this one disagrees in the direction that matters: a run that took
/// the prefixed entry would reserve **nothing** against a money ceiling and
/// the ceiling would admit everything.
///
/// So neither preference is available. "Prefer the prefixed key" picks the
/// zero here and is unsafe; "take the dearer of the two" publishes a price no
/// source stated, which is the fabrication `cost.rs` refuses one field over.
/// A third party's table that contradicts itself about a price is not evidence
/// a guarantee can rest on, and the operator's remedy already exists and
/// outranks it: `models.toml` is §5's first source.
///
/// The refusal names both keys, because "your table disagrees with itself"
/// is unactionable without them.
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

/// `sample_spec` is documentation living in the data, and it is not a model.
///
/// The real file carries an entry by that name listing every field litellm
/// supports. It has no `litellm_provider`, so the provider-matched lookup
/// already cannot reach it — this pins that the decoder tolerates its presence
/// rather than choking on an entry shaped unlike the rest.
#[test]
fn an_entry_with_no_provider_is_ignored_rather_than_fatal() {
    let bytes = br#"{
      "sample_spec": {"input_cost_per_token": 0.0, "notes": "the fields we support"},
      "real": {"input_cost_per_token": 1e-06, "litellm_provider": "acme", "mode": "chat"}
    }"#;
    let table = Table::decode(bytes).expect("an odd entry is not a broken document");
    assert!(table.quote("acme", "real").is_ok());
    assert!(matches!(
        table.quote("", "sample_spec"),
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
