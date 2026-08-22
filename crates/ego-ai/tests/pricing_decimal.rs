//! The one number in `034` that a `f64` must never touch.
//!
//! litellm states every rate as a JSON *number*, and two properties of that
//! artefact decide this module's shape. Both were measured against the real
//! file (3111 entries, 2380 of them `mode: "chat"`) rather than assumed:
//!
//! 1. **The spelling is exponent notation** — `5e-06`, `1.25e-05` — and
//!    sometimes plain — `0.0`. [`Amount::from_str`] refuses an exponent on
//!    purpose (`cost.rs:314-320`: "one spelling per value, so parsing a
//!    serialised amount cannot introduce a form the serialiser would never
//!    emit"). That reasoning is about ego's *own* round trip and it stays
//!    correct; a third party's document is not ego's serialisation, so it is
//!    decoded here, in the crate that owns foreign formats, exactly as a wire
//!    body is.
//!
//! 2. **Some values carry binary-float noise.** The deepest in the file is
//!    `3.0001999999999996e-07`, which is `0.30002` per million divided by a
//!    million in Python and then printed as a `f64`. Exactly representing it
//!    needs 23 fractional digits and [`MAX_SCALE`] is 12.
//!
//! Which is why the rounding direction is a *budget* decision and not a
//! numeric convenience: a rate rounded **down** mints a reservation below what
//! the provider will really charge, and `budget.rs`'s entire premise is that
//! the reservation is pessimistic. So a value too deep to represent rounds
//! **up**, never to nearest — over-reserving by at most one unit in the
//! twelfth place is a bounded, safe error, and under-reserving is the
//! unsoundness the gate exists to prevent.

use ego_ai::pricing::{PriceError, decimal};
use ego_core::cost::{Amount, MAX_SCALE};

/// `Amount` has no public accessors for its parts, so the assertions compare
/// against its canonical decimal spelling — which is also its durable one.
#[track_caller]
fn parses(text: &str, expected: &str) {
    let got = decimal(text).unwrap_or_else(|err| panic!("{text} should parse: {err:?}"));
    assert_eq!(got.to_string(), expected, "{text}");
}

/// The ordinary case, and the one that made the module necessary.
///
/// `5e-06` is Anthropic's Opus input rate as litellm writes it. Nothing in
/// `ego-core` accepts that spelling.
#[test]
fn a_negative_exponent_is_a_plain_decimal_here() {
    parses("5e-06", "0.000005");
    parses("1.25e-05", "0.0000125");
    parses("2e-07", "0.0000002");
}

/// The other spelling in the same file, so the decoder is not tuned to one.
#[test]
fn a_number_with_no_exponent_still_parses() {
    parses("0.0", "0");
    parses("3", "3");
    parses("0.000005", "0.000005");
}

/// A positive exponent shifts the other way, and `e5` with no sign is the
/// same as `e+5`.
///
/// No rate in the table uses one. It is accepted because the *grammar* is
/// JSON's number, not a subset chosen from today's data — a table that starts
/// stating dollars per million rather than per token would otherwise be
/// rejected as malformed rather than read.
#[test]
fn a_positive_exponent_shifts_the_point_the_other_way() {
    parses("1.5e3", "1500");
    parses("1.5e+3", "1500");
    parses("2e1", "20");
}

/// The measured worst case in the file, and the whole reason rounding has a
/// direction.
///
/// `3.0001999999999996e-07` needs 23 fractional digits. At `MAX_SCALE` it
/// becomes `0.000000300020`, which `Amount`'s canonical form then trims — and
/// that is the price litellm *meant*, recovered rather than approximated. The
/// float noise is below the twelfth place, so rounding up costs nothing here;
/// the direction matters for the values where it is not free.
#[test]
fn float_noise_below_the_last_representable_place_is_rounded_away() {
    parses("3.0001999999999996e-07", "0.00000030002");
}

/// Rounding is **up**, not to nearest, and this is the test that pins it.
///
/// `1.0000000000001e-01` is `0.10000000000001`: fourteen fractional digits,
/// and the digits past the twelfth are `01`. To nearest it would round *down*
/// to `0.1`. Up, it is `0.100000000001`.
///
/// A rate is multiplied by a token count to mint a reservation
/// (`budget.rs`'s `upper_bound`), so a rate below the truth is a reservation
/// below the truth, which is the one direction the gate must never err in.
/// "Nearest" is right for a measurement and wrong for a bound.
#[test]
fn a_value_too_deep_to_represent_rounds_up_and_never_to_nearest() {
    parses("1.0000000000001e-01", "0.100000000001");
    // The same value written without an exponent, so the rounding is a
    // property of the number and not of its spelling.
    parses("0.10000000000001", "0.100000000001");
}

/// And rounding up does not silently turn a real price into zero.
///
/// A rate far below the twelfth place — `1e-30` — is not representable at all.
/// It rounds up to the smallest representable positive amount rather than to
/// `0`, because a zero rate is a *statement that the tokens are free*, and
/// `cost.rs:335-339` refuses exactly that conflation: "a price list saying
/// zero per token and a row that has no per-token price at all are different
/// facts".
#[test]
fn a_rate_below_the_last_place_rounds_up_to_the_smallest_amount_not_to_zero() {
    parses("1e-30", "0.000000000001");
}

/// A true zero stays zero, so the guard above does not invent a price.
///
/// litellm writes `0.0` for locally served rows, and zero is a *stated* rate
/// there rather than an unrepresentable one. It must not be nudged upward.
///
/// The digits are what decide it, not the depth: every one of these is zero
/// however far past [`MAX_SCALE`] it is written, which is what separates them
/// from the `1e-30` above. An earlier draft of this test asserted
/// `0.0000000000000001` — that is `1e-16`, a *nonzero* rate below the last
/// place, and by the rule above it must round **up** exactly as `1e-30` does.
/// Asserting it was zero would have made the two tests contradict each other
/// and pinned the rounding-to-zero this file exists to forbid.
#[test]
fn a_stated_zero_is_zero_however_deep_it_is_written() {
    parses("0e-30", "0");
    parses("0.000000000000000", "0");
    parses("0.0000000000000000000000000", "0");
}

/// The scale ceiling is `ego-core`'s, read rather than restated.
#[test]
fn the_ceiling_this_module_rounds_at_is_the_one_amount_publishes() {
    let smallest = decimal("1e-30").expect("rounds up rather than refusing");
    let expected = Amount::new(1, MAX_SCALE).expect("one unit in the last place");
    assert_eq!(smallest, expected);
}

/// A negative rate is refused rather than clamped.
///
/// `Amount` is unsigned, so a minus sign has no representation at all — and a
/// negative price is not a small price, it is a document saying something this
/// type cannot mean. Refusing names the value; clamping to zero would publish
/// "these tokens are free" on the strength of a typo.
#[test]
fn a_negative_number_is_refused_and_not_clamped() {
    let err = decimal("-1e-06").expect_err("a negative rate has no meaning here");
    assert!(matches!(err, PriceError::NotANumber { .. }), "{err:?}");
}

/// Anything that is not a JSON number is refused, and the refusal carries the
/// text so an operator can find it in a 1.8 MB file.
#[test]
fn a_value_that_is_not_a_number_is_refused_with_what_was_found() {
    for text in [
        "", "null", "1.2.3", "e5", "1e", "1e+", "0x10", " 1 ", "1_000",
    ] {
        let err = decimal(text).expect_err("{text} is not a number");
        let PriceError::NotANumber { found } = err else {
            panic!("{text}: wrong error {err:?}");
        };
        assert_eq!(found, text, "the refusal must quote what it found");
    }
}

/// An exponent large enough to overflow the shift is refused, not wrapped.
///
/// `1e+40` is a perfectly good JSON number and not a representable `Amount`.
/// The failure is named rather than saturated for `checked_add`'s reason
/// (`cost.rs:243-247`): a total pinned at the maximum reads as a real number
/// to a budget gate.
#[test]
fn a_value_too_large_to_represent_is_refused_rather_than_saturated() {
    let err = decimal("1e+40").expect_err("larger than any amount");
    assert!(matches!(err, PriceError::OutOfRange { .. }), "{err:?}");
}

/// Every distinct rate spelling in the real table, against an exact expected
/// value computed independently.
///
/// The tests above are hand-picked cases and prove the rules; this one proves
/// the *artefact* has no spelling those rules do not cover. 408 distinct
/// strings, taken from the five rate fields of every entry in the 1.8 MB
/// document, each paired with what an arbitrary-precision decimal says it is
/// after rounding up at [`MAX_SCALE`] — computed by Python's `decimal` module
/// rather than by this parser, so the fixture is a second opinion and not a
/// recording of the thing under test.
///
/// The spellings are committed rather than the table: the file is 1.8 MB of
/// prices that change weekly, and what this test is about is *number syntax*,
/// which does not. A new spelling arriving upstream is a real change and
/// should be a deliberate fixture update.
#[test]
fn every_rate_spelling_in_the_real_table_decodes_to_the_exact_value() {
    let fixture = include_str!("goldens/pricing/rate_spellings.json");
    let rows: Vec<(String, String)> =
        serde_json::from_str(fixture).expect("the committed fixture is JSON");
    assert_eq!(rows.len(), 408, "the fixture lost rows");
    for (text, expected) in rows {
        parses(&text, &expected);
    }
}
