//! The litellm price table: fetched, decoded, and never trusted with a float.
//!
//! `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §5 makes this the
//! *third* and last rate source — behind an operator's own quote and behind the
//! provider's published prices — "because it is a third party's summary of
//! someone else's prices". §6 puts the fetching and decoding here, in the crate
//! that owns `Http` and the wire formats, and the reading and writing in
//! `ego-tools`, which owns `StoreIo`.
//!
//! What lives in this file is only the decode. Nothing here opens a socket or a
//! file: it turns bytes that arrived from somewhere into values `ego-core` can
//! consume, which is what every other decoder in this crate does.
//!
//! # Why the numbers get their own parser
//!
//! [`Amount::from_str`](std::str::FromStr) refuses an exponent, and the reason
//! it gives is right: "one spelling per value, so parsing a serialised amount
//! cannot introduce a form the serialiser would never emit" (`cost.rs:314-320`).
//! That is a rule about ego's own round trip. litellm's document is not ego's
//! serialisation — it is a foreign format, and a foreign format is decoded at
//! the seam rather than by relaxing the durable type's grammar.
//!
//! The grammar accepted here is JSON's number, minus the sign. Measured against
//! the real artefact (3111 entries, 2380 of them `mode: "chat"`) the file uses
//! two spellings — `5e-06` and `0.0` — and both appear for the same field, so a
//! decoder tuned to either one alone would drop rates silently.
//!
//! # Why the rounding has a direction
//!
//! Some values in the file carry binary-float noise. The deepest is
//! `3.0001999999999996e-07`, which is `0.30002` per million divided by a
//! million in Python and printed as an `f64`; representing it exactly needs 23
//! fractional digits and [`MAX_SCALE`] is 12.
//!
//! So a value can be too deep to represent, and *which way it is bent* is a
//! budget decision rather than a numeric one. A rate is multiplied by a token
//! count to mint a reservation, and `budget.rs`'s premise is that the
//! reservation is pessimistic — so a rate below the truth is a reservation
//! below the truth, which is the one direction the gate must never err in.
//! Rounding is therefore **up**, never to nearest: over-reserving by at most a
//! unit in the twelfth place is bounded and safe, and "nearest" is right for a
//! measurement and wrong for a bound.
//!
//! Rounding up stops at the smallest representable amount rather than at zero,
//! for the reason `cost.rs:335-339` gives: "a price list saying zero per token
//! and a row that has no per-token price at all are different facts". A rate
//! that fell to `0` here would publish the first while meaning the second. A
//! *stated* zero — which litellm writes for locally served rows — is untouched,
//! because its digits are zero rather than merely small.

use ego_core::cost::{Amount, MAX_SCALE, MoneyError};

/// Why a figure in the table could not become an [`Amount`].
///
/// Both arms quote what was found. The file is 1.8 MB of machine-generated
/// JSON, and a refusal that does not carry the offending text sends an
/// operator to search it by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriceError {
    /// Not a JSON number, or a negative one.
    ///
    /// The two share an arm because [`Amount`] is unsigned: a minus sign has
    /// no representation, so `-1e-06` is not a small price but a document
    /// saying something this type cannot mean. Clamping it to zero would
    /// publish "these tokens are free" on the strength of a typo.
    NotANumber { found: String },
    /// A well-formed number larger than any [`Amount`].
    ///
    /// Named rather than saturated, for `checked_add`'s reason
    /// (`cost.rs:243-247`): a figure pinned at the maximum reads as a real
    /// number to a budget gate.
    OutOfRange { found: String },
}

impl std::fmt::Display for PriceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PriceError::NotANumber { found } => {
                write!(f, "{found:?} is not a non-negative decimal number")
            }
            PriceError::OutOfRange { found } => {
                write!(f, "{found} is larger than any amount ego can represent")
            }
        }
    }
}

impl std::error::Error for PriceError {}

/// One JSON number from the table, as an exact [`Amount`].
///
/// Takes the number's *text* rather than a parsed value, which is the whole
/// point: a rate that has been through an `f64` is no longer the rate, and
/// `serde_json`'s default number type is one. Callers hand this the raw token
/// (a `serde_json::value::RawValue`'s `get()`).
///
/// See the module doc for the grammar and for why rounding is upward.
pub fn decimal(text: &str) -> Result<Amount, PriceError> {
    let invalid = || PriceError::NotANumber {
        found: text.to_owned(),
    };

    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, parse_exponent(exponent).ok_or_else(invalid)?),
        None => (text, 0i32),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((int, frac)) => (int, frac),
        None => (mantissa, ""),
    };
    let digits_only = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if !digits_only(int) || (mantissa.contains('.') && !digits_only(frac)) {
        return Err(invalid());
    }

    // The scale the value would need if it were written out in full: one
    // fractional digit per digit after the point, less whatever the exponent
    // shifts the point by. Negative means the point moves right, past every
    // digit there is.
    let scale = i32::try_from(frac.len()).map_err(|_| invalid())? - exponent;
    let mut digits = format!("{int}{frac}");

    if scale <= 0 {
        // A whole number with `-scale` zeros to append. `10^40` is a legal
        // JSON number and not a legal `Amount`, so the multiplication is the
        // one that can overflow.
        let zeros = usize::try_from(-scale).map_err(|_| invalid())?;
        digits.push_str(&"0".repeat(zeros));
        let coefficient = digits.parse::<u128>().map_err(|_| PriceError::OutOfRange {
            found: text.to_owned(),
        })?;
        return finish(coefficient, 0, text);
    }

    let scale = u32::try_from(scale).expect("positive by the branch above");
    if scale <= MAX_SCALE {
        let coefficient = digits.parse::<u128>().map_err(|_| PriceError::OutOfRange {
            found: text.to_owned(),
        })?;
        return finish(coefficient, scale, text);
    }

    // Too deep to represent. Drop the digits past `MAX_SCALE` and round up if
    // any of them was nonzero — see the module doc on why the direction is
    // not "to nearest".
    let dropped = usize::try_from(scale - MAX_SCALE).expect("a u32 difference fits a usize");
    let keep = digits.len().saturating_sub(dropped);
    let remainder_is_nonzero = digits[keep..].bytes().any(|b| b != b'0');
    digits.truncate(keep);
    let truncated: u128 = if digits.is_empty() {
        0
    } else {
        digits.parse().map_err(|_| PriceError::OutOfRange {
            found: text.to_owned(),
        })?
    };
    let coefficient = truncated + u128::from(remainder_is_nonzero);
    finish(coefficient, MAX_SCALE, text)
}

/// The exponent's own grammar: an optional sign, then digits.
///
/// Returns `None` for `1e`, `1e+` and anything else JSON would reject, so the
/// caller can name the whole number rather than the tail.
fn parse_exponent(text: &str) -> Option<i32> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let magnitude: i32 = digits.parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// Build the [`Amount`], mapping its own refusal onto this module's.
///
/// [`Amount::new`] can still refuse a scale this function believes is legal —
/// it owns [`MAX_SCALE`] and this module only reads it — so the check is not
/// duplicated here, it is deferred to the type that publishes the constant.
fn finish(coefficient: u128, scale: u32, text: &str) -> Result<Amount, PriceError> {
    Amount::new(coefficient, scale).map_err(|err| match err {
        MoneyError::Overflow | MoneyError::ScaleTooLarge { .. } => PriceError::OutOfRange {
            found: text.to_owned(),
        },
        _ => PriceError::NotANumber {
            found: text.to_owned(),
        },
    })
}
