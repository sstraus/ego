//! Fetch and decode the third-party price table without file I/O. `ego-tools` owns cache
//! storage through `StoreIo`. Runs read a usable cache before fetching.
//!
//! Foreign JSON numbers accept exponent notation at this seam without relaxing the durable
//! `Amount` grammar. Rates deeper than `MAX_SCALE` are rounded upward, never downward, so
//! reservation bounds remain pessimistic. A positive rate must not become zero; a stated zero
//! remains zero.

use ego_core::budget::PricingQuote;
use ego_core::cost::{Amount, CurrencyCode, MAX_SCALE, MoneyError, PricingRevision};

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

    let too_large = || PriceError::OutOfRange {
        found: text.to_owned(),
    };

    // The significant digits, with leading zeros gone. Stripping them first is
    // what makes the length checks below mean "how big is this number" rather
    // than "how verbosely was it written": `0e100` has 101 digits written out
    // and is zero, and refusing it for its length would be a false refusal.
    //
    // It does not disturb the rounding either — dropping k leading zeros drops
    // both the total length and the kept length by k, so the surviving digits
    // and the discarded tail are unchanged.
    let mut digits = format!("{int}{frac}");
    let significant = digits.len() - digits.bytes().take_while(|&b| b == b'0').count();
    if significant == 0 {
        return finish(0, 0, text);
    }
    digits.drain(..digits.len() - significant);

    // The scale the value would need if it were written out in full: one
    // fractional digit per digit after the point, less whatever the exponent
    // shifts the point by. Negative means the point moves right, past every
    // digit there is.
    //
    // Checked, because a hugely negative exponent overflows the subtraction —
    // and a release build would **wrap** it, minting a scale, and therefore a
    // price, with no relation to the digits.
    let scale = i32::try_from(frac.len())
        .map_err(|_| invalid())?
        .checked_sub(exponent)
        .ok_or_else(too_large)?;

    if scale <= 0 {
        // A whole number with `-scale` zeros appended. The count is refused
        // *before* the zeros are materialised: these bytes came off a network
        // from a third party, and `1e+2147483647` would otherwise ask for two
        // gigabytes of them so that the result could then be found not to fit.
        // Proving it cannot fit has to come first.
        let zeros = usize::try_from(-i64::from(scale)).map_err(|_| invalid())?;
        if digits.len().saturating_add(zeros) > U128_DIGITS {
            return Err(too_large());
        }
        digits.push_str(&"0".repeat(zeros));
        let coefficient = digits.parse::<u128>().map_err(|_| too_large())?;
        return finish(coefficient, 0, text);
    }

    let scale = u32::try_from(scale).expect("positive by the branch above");
    if scale <= MAX_SCALE {
        let coefficient = digits.parse::<u128>().map_err(|_| too_large())?;
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
        digits.parse().map_err(|_| too_large())?
    };
    // Checked: at the maximum coefficient the carry does not fit, and a wrap
    // would turn the largest representable number into the smallest — which a
    // budget gate would read as free.
    let coefficient = truncated
        .checked_add(u128::from(remainder_is_nonzero))
        .ok_or_else(too_large)?;
    finish(coefficient, MAX_SCALE, text)
}

/// How many decimal digits `u128::MAX` has.
///
/// The cheap upper bound the allocation guard uses: anything longer cannot be
/// an [`Amount`]'s coefficient, so it can be refused by counting rather than
/// by being built first.
const U128_DIGITS: usize = 39;

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

// --- the document ---

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::value::RawValue;

/// The five rate fields, in the order [`PricingQuote`] declares its own.
///
/// A constant rather than five string literals at the point of use, because
/// [`TableError::Rate`] names the field it choked on and the name it reports
/// must be the one it read.
///
/// [`PricingQuote`]: ego_core::budget::PricingQuote
const FIELDS: [&str; 5] = [
    "input_cost_per_token",
    "cache_read_input_token_cost",
    "cache_creation_input_token_cost",
    "cache_creation_input_token_cost_above_1hr",
    "output_cost_per_token",
];

/// The mode of the rows `--model` can select.
///
/// The document mixes embeddings, rerankers, image and audio rows into the
/// same map, and an image row is priced *per image* while carrying the same
/// field names — so reading one as a per-token rate yields a number wrong by
/// orders of magnitude that looks entirely plausible.
const CHAT: &str = "chat";

/// A decoded litellm document, holding its entries unparsed.
///
/// Lazy on purpose. The file carries 3111 entries and a run asks about one; a
/// decoder that parsed every rate up front would let a malformed figure on a
/// model nobody selected refuse a run that never needed it. The blast radius
/// of a bad entry is the entry.
///
/// It also keeps the rates as their original **text**. `serde_json`'s number
/// type is an `f64`, and a rate that has been through one is no longer the
/// rate — see this module's header.
#[derive(Debug)]
pub struct Table {
    entries: BTreeMap<String, Box<RawValue>>,
}

/// What one model costs per token, as the table states it.
///
/// Each category is optional and an absent figure stays absent. This type
/// reports document data; ceiling policy decides what an absence means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableEntry {
    /// Which key in the document this came from, so a figure stays traceable
    /// to the line that stated it.
    pub key: String,
    pub uncached_input: Option<Amount>,
    pub cache_read: Option<Amount>,
    pub cache_write: Option<Amount>,
    pub cache_write_extended: Option<Amount>,
    pub output: Option<Amount>,
}

/// Why the table could not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    /// The bytes are not a JSON object of entries at all — a proxy's error
    /// page, a truncated download, a rate-limit body.
    NotATable { message: String },
    /// No chat row under this provider and model.
    ///
    /// Distinct from a row that exists and prices nothing, so that
    /// `UnknownCost::NoPricingQuote` can answer only its own question.
    NoEntry { provider: String, model: String },
    /// One of the five rate fields is not a number.
    Rate {
        key: String,
        field: &'static str,
        source: PriceError,
    },
    /// Refuse conflicting prices for two spellings of one model. Choosing a zero-priced alias
    /// can under-reserve; choosing the dearer entry invents an authoritative price. Operator
    /// rates can override an unusable third-party table.
    Contradiction { model: String, keys: Vec<String> },
}

impl std::fmt::Display for TableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TableError::NotATable { message } => {
                write!(f, "the pricing table is not a table: {message}")
            }
            TableError::NoEntry { provider, model } => {
                write!(f, "the pricing table has no chat model {provider}/{model}")
            }
            TableError::Rate { key, field, source } => {
                write!(f, "{key}: {field} is not a rate: {source}")
            }
            TableError::Contradiction { model, keys } => write!(
                f,
                "the pricing table states different prices for {model} under \
                 {} — it contradicts itself, so no ceiling can rest on it; \
                 supply the rates in models.toml instead",
                keys.join(" and ")
            ),
        }
    }
}

impl std::error::Error for TableError {}

/// The fields of one entry this decoder reads, and nothing else.
///
/// `deny_unknown_fields` is deliberately **absent**, which is the opposite of
/// what every ego-owned schema does. This document is a third party's and it
/// carries ~30 fields per entry that ego has no opinion about; refusing an
/// entry because litellm added a `supports_web_search` flag would make an
/// upstream addition break every ceiling.
#[derive(Deserialize)]
struct Row<'a> {
    #[serde(borrow, default)]
    litellm_provider: Option<&'a str>,
    #[serde(borrow, default)]
    mode: Option<&'a str>,
    #[serde(borrow, default)]
    input_cost_per_token: Option<&'a RawValue>,
    #[serde(borrow, default)]
    cache_read_input_token_cost: Option<&'a RawValue>,
    #[serde(borrow, default)]
    cache_creation_input_token_cost: Option<&'a RawValue>,
    #[serde(borrow, default)]
    cache_creation_input_token_cost_above_1hr: Option<&'a RawValue>,
    #[serde(borrow, default)]
    output_cost_per_token: Option<&'a RawValue>,
}

impl Table {
    /// Decode the fetched bytes into entries, without reading a single rate.
    pub fn decode(bytes: &[u8]) -> Result<Table, TableError> {
        serde_json::from_slice(bytes)
            .map(|entries| Table { entries })
            .map_err(|err| TableError::NotATable {
                message: err.to_string(),
            })
    }

    /// The rates for one model, or why there are none.
    ///
    /// `provider` is litellm's own provider name and `model` is the model half
    /// of ego's slug. Both spellings the document uses are tried — the
    /// prefixed `provider/model` and the bare `model` — and an entry answers
    /// only if it *states* that provider, so one provider's bare key cannot
    /// pick up another's prices among the 576 bare keys the file carries.
    pub fn quote(&self, provider: &str, model: &str) -> Result<TableEntry, TableError> {
        self.quote_in(provider, provider, model)
    }

    /// [`quote`](Self::quote) for a provider whose prefixed keys do not use
    /// its namespace name: the table files `vertex_ai-anthropic_models` rows
    /// as `vertex_ai/<model>`.
    pub fn quote_in(
        &self,
        provider: &str,
        key_prefix: &str,
        model: &str,
    ) -> Result<TableEntry, TableError> {
        let mut found: Vec<TableEntry> = Vec::new();
        for key in [format!("{key_prefix}/{model}"), model.to_string()] {
            let Some(raw) = self.entries.get(&key) else {
                continue;
            };
            let Ok(row) = serde_json::from_str::<Row>(raw.get()) else {
                // An entry shaped unlike the rest — `sample_spec` is one — is
                // not a model and is not a broken document either.
                continue;
            };
            if row.litellm_provider != Some(provider) || row.mode != Some(CHAT) {
                continue;
            }
            let entry = row.into_entry(&key)?;
            if !found.iter().any(|seen| seen.same_rates(&entry)) {
                found.push(entry);
            }
        }
        match found.len() {
            0 => Err(TableError::NoEntry {
                provider: provider.to_string(),
                model: model.to_string(),
            }),
            1 => Ok(found.remove(0)),
            _ => Err(TableError::Contradiction {
                model: model.to_string(),
                keys: found.into_iter().map(|entry| entry.key).collect(),
            }),
        }
    }
}

impl TableEntry {
    /// Whether two entries state the same five prices.
    ///
    /// Compares the rates and not the key: two spellings of one model that
    /// agree are one fact written twice, and refusing them would refuse 15 of
    /// the 19 duplicated models in the real file for no reason.
    fn same_rates(&self, other: &TableEntry) -> bool {
        self.uncached_input == other.uncached_input
            && self.cache_read == other.cache_read
            && self.cache_write == other.cache_write
            && self.cache_write_extended == other.cache_write_extended
            && self.output == other.output
    }

    /// Project the stated rate categories into a quote without completing missing values. An
    /// unpriced input category leaves a money bound unknown. Operator configuration can supply
    /// missing rates; a provider-wide cache-write assumption would invent model-specific
    /// prices. The caller supplies the fetch revision.
    pub fn into_quote(self, revision: PricingRevision) -> PricingQuote {
        PricingQuote {
            web_search: None,
            revision,
            currency: currency(),
            uncached_input: self.uncached_input,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            cache_write_extended: self.cache_write_extended,
            output: self.output,
        }
    }
}

/// The currency every rate in the litellm document is expressed in.
///
/// **An assertion about the artefact, not a decoded field.** No entry carries
/// a currency of any kind — the figures are US dollars per token by the file's
/// own convention, which is stated in its documentation and nowhere in its
/// data. Written down once, so a day when that convention changes, or a
/// second table in another currency arrives, has one place to be wrong instead
/// of one per call site.
fn currency() -> CurrencyCode {
    CurrencyCode::new("USD").expect("three ASCII letters")
}

impl Row<'_> {
    fn into_entry(self, key: &str) -> Result<TableEntry, TableError> {
        let rate = |raw: Option<&RawValue>, field: &'static str| match raw {
            None => Ok(None),
            Some(raw) => decimal(raw.get())
                .map(Some)
                .map_err(|source| TableError::Rate {
                    key: key.to_string(),
                    field,
                    source,
                }),
        };
        Ok(TableEntry {
            key: key.to_string(),
            uncached_input: rate(self.input_cost_per_token, FIELDS[0])?,
            cache_read: rate(self.cache_read_input_token_cost, FIELDS[1])?,
            cache_write: rate(self.cache_creation_input_token_cost, FIELDS[2])?,
            cache_write_extended: rate(self.cache_creation_input_token_cost_above_1hr, FIELDS[3])?,
            output: rate(self.output_cost_per_token, FIELDS[4])?,
        })
    }
}

// --- the fetch ---

use std::fmt;
use std::sync::{Arc, OnceLock};

use url::Url;

use crate::http::{
    AdmittedHttpProfile, Exchange, Http, HttpRequest, Method, exchange, public_header,
};

/// The document itself, on the branch litellm publishes from.
///
/// The raw artefact and not a page about it: this is the file litellm's own
/// client reads, so what ego prices against and what litellm prices against are
/// the same bytes rather than two renderings of them.
///
/// `main` and not a release tag, because the file is not released — it changes
/// on merge, several times a week, and there is no tag whose contents are
/// promised to be any particular version of it. Which *fetch* a figure came
/// from is answered by the content hash, which is exact where a tag would only
/// have been suggestive.
pub const UPSTREAM: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// [`UPSTREAM`], parsed.
pub fn upstream() -> Url {
    Url::parse(UPSTREAM).expect("a constant this file controls")
}

fn http_profile() -> &'static AdmittedHttpProfile {
    static PROFILE: OnceLock<AdmittedHttpProfile> = OnceLock::new();
    PROFILE.get_or_init(|| {
        AdmittedHttpProfile::from_urls("aux/pricing/litellm/v1", &[upstream()])
            .expect("the built-in pricing URL is an admitted HTTP route")
    })
}

/// The response header that identifies which upstream artefact these bytes are.
const ETAG: &str = "etag";
/// The request header that offers a known identity back, so the server can
/// answer [`NOT_MODIFIED`] instead of resending 1.8 MB.
const IF_NONE_MATCH: &str = "if-none-match";
/// "What you have is still current."
const NOT_MODIFIED: u16 = 304;

/// What a conditional fetch came back with.
#[derive(Debug)]
pub enum Fetched {
    /// A table, and the identity the server served it under.
    Table {
        bytes: Vec<u8>,
        /// The `ETag`, verbatim — quotes, weak-validator prefix and all.
        ///
        /// **An upstream identity, and deliberately not called a version
        /// anywhere it could be mistaken for one.** litellm publishes no
        /// version for this file (see [`UPSTREAM`]); what the server states is
        /// an opaque validator, and its two uses are exactly the two a
        /// validator has: offering it back on the next fetch, and naming in a
        /// [`PricingRevision`] which upstream artefact a past figure came
        /// from. Verbatim because it is an opaque token — normalising it would
        /// be inventing structure the specification says it has not got, and a
        /// re-spelled validator no longer matches on `If-None-Match`.
        upstream_version: String,
    },
    /// The copy already cached is still current, on the server's own word.
    ///
    /// The good outcome of a refresh: nothing crossed the network but a
    /// header, and the cached blob's clock can be moved forward without a
    /// second copy of identical bytes
    /// (`ego_tools::pricing::PricingCache::publish`).
    Unchanged,
}

/// Why no table came back.
#[derive(Debug)]
pub enum FetchError {
    /// The exchange did not deliver: transport failure, or a status that is
    /// neither success nor [`NOT_MODIFIED`].
    ///
    /// If an online pricing fetch fails and no usable cache exists, report a retryable
    /// runtime failure.
    Unreachable { endpoint: String, message: String },
    /// Bytes arrived but are not a price table. Distinguish a captive portal or HTML error from
    /// a failed fetch before it becomes a hash-named cache artifact.
    NotATable { source: TableError },
    /// Nothing upstream identifies these bytes.
    /// A pricing revision needs source, upstream version and content hash.
    /// If a proxy strips the validator, refuse with the missing header named
    /// rather than fabricate an upstream version. Operator rates remain available.
    Unidentified { header: &'static str },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Unreachable { endpoint, message } => {
                write!(f, "{endpoint} did not answer with a price table: {message}")
            }
            FetchError::NotATable { source } => {
                write!(f, "what {UPSTREAM} answered is not a price table: {source}")
            }
            FetchError::Unidentified { header } => write!(
                f,
                "{UPSTREAM} answered with no `{header}`, so the table it sent \
                 cannot be pinned to an upstream artefact"
            ),
        }
    }
}

impl std::error::Error for FetchError {}

/// Fetch with the cached validator so an unchanged response refreshes age without another body.
/// The caller decides whether a ceiling needs rates; the injected transport enforces offline
/// confinement. This function opens a socket only after those boundaries admit it.
pub async fn fetch(http: &Arc<dyn Http>, known: Option<&str>) -> Result<Fetched, FetchError> {
    let unreachable = |message: String| FetchError::Unreachable {
        endpoint: UPSTREAM.to_string(),
        message,
    };
    let answer = exchange(
        http,
        http_profile(),
        HttpRequest {
            url: upstream(),
            method: Method::Get,
            headers: known
                .map(|known| vec![public_header(IF_NONE_MATCH, known.as_bytes().to_vec())])
                .unwrap_or_default(),
            body: None,
        },
    )
    .await
    .map_err(unreachable)?;

    if answer.status == NOT_MODIFIED {
        return Ok(Fetched::Unchanged);
    }
    if !(200..300).contains(&answer.status) {
        return Err(unreachable(format!(
            "it answered {}: {}",
            answer.status,
            summarise(&answer)
        )));
    }
    // Decoded before the header is looked at, so that bytes which are not a
    // table say so rather than being reported as a header problem: an
    // interception page usually has neither, and "this is not the file" is the
    // more actionable of the two.
    Table::decode(&answer.bytes).map_err(|source| FetchError::NotATable { source })?;
    let upstream_version = answer
        .header(ETAG)
        .ok_or(FetchError::Unidentified { header: ETAG })?
        .to_string();
    Ok(Fetched::Table {
        bytes: answer.bytes,
        upstream_version,
    })
}

/// An error body, short enough to put in a message.
///
/// A failing origin can answer with anything, including something the size of
/// the table itself, and an error string is going to be rendered to a terminal.
fn summarise(answer: &Exchange) -> String {
    const KEPT: usize = 200;
    let text = String::from_utf8_lossy(&answer.bytes);
    match text.char_indices().nth(KEPT) {
        None => text.into_owned(),
        Some((cut, _)) => format!("{}…", &text[..cut]),
    }
}
