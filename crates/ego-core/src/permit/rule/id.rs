//! `RuleId`: what makes `ego rules revoke` possible without the original
//! context.
//!
//! `docs/decisions/2026-08-21-a-rule-that-means-exactly-one-thing.md` §8.

use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

/// The generation of the canonicalisation rules this build implements.
///
/// It goes **inside** the ID, and that is what makes an ID stable across
/// versions of ego rather than merely within one. If canonicalisation ever
/// changes — a new head, a Unicode rule, a different tokenisation — rules
/// written under these rules keep their `r1:` IDs and keep meaning what `r1`
/// meant. A bare hash has exactly one failure mode and it is the unacceptable
/// one: an upgrade silently changes what a stored ID denotes, and a deny
/// somebody revoked years ago is not the deny that was there.
pub const GRAMMAR_VERSION: u32 = 1;

/// Crockford's alphabet, lowercased, written out here rather than taken from
/// a crate.
///
/// An ID is a durable identifier in an operator's config file. If the
/// alphabet came from a dependency, a `cargo update` that changed it would
/// silently change what every stored ID denotes — the same objection §5
/// raises to inheriting glob semantics, applied to the other end of the
/// pipeline.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// The identity of a rule's *predicate*.
///
/// Neither the effect nor the scope is in it. Allow and deny of one predicate
/// share an ID and are told apart by which list holds them, so
/// `ego rules revoke <id>` removes the rule wherever it is — which is what
/// "revocable without the original context" has to mean. The same predicate
/// at project and user scope is one rule in two places; where it came from is
/// metadata that `rules ls` shows.
/// Serialised as the string [`Display`](fmt::Display) writes and
/// [`FromStr`] reads — `r1:yhntjjh06f3x` — rather than as its two fields.
/// One representation, and it is the one an operator already sees in a
/// refusal and hands to `ego rules revoke`.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct RuleId {
    generation: u32,
    digest: String,
}

impl RuleId {
    /// Compute an ID over a rule's canonical AST bytes.
    ///
    /// Over the AST and never the input text: that is what makes
    /// `read( src/** )` and `read(src/**)` one ID.
    /// `sha256` and not a second hash crate: nothing is authorised *by* an ID
    /// (see [`DIGEST_CHARS`]), so the properties that would justify one buy
    /// nothing here, and `sha2` is already a dependency for instruction
    /// digests.
    pub fn of(canonical: &[u8]) -> Self {
        let digest = Sha256::digest(canonical);
        Self {
            generation: GRAMMAR_VERSION,
            digest: base32(&digest),
        }
    }

    /// Which generation of the grammar computed this ID.
    ///
    /// An ID from a generation this build does not know is reported as
    /// unknown and the rule is refused, never guessed at: an `r2:` ID on a
    /// build that only knows `r1` cannot be verified to mean anything.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn is_known(&self) -> bool {
        self.generation == GRAMMAR_VERSION
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}:{}", self.generation, self.digest)
    }
}

/// Why a string is not a rule ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRuleId(String);

impl fmt::Display for BadRuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not a rule id. A rule id is `r<generation>:<digest>`, \
             for example `r{GRAMMAR_VERSION}:9f2kq7x4mzt8`.",
            self.0
        )
    }
}

impl std::error::Error for BadRuleId {}

impl FromStr for RuleId {
    type Err = BadRuleId;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let bad = || BadRuleId(raw.to_string());
        let (generation, digest) = raw
            .strip_prefix('r')
            .and_then(|rest| rest.split_once(':'))
            .ok_or_else(bad)?;

        let generation: u32 = generation.parse().map_err(|_| bad())?;
        if digest.len() != DIGEST_CHARS || !digest.bytes().all(|b| ALPHABET.contains(&b)) {
            return Err(bad());
        }

        Ok(Self {
            generation,
            digest: digest.to_string(),
        })
    }
}

/// 60 bits, which is 12 characters of 5 bits each.
///
/// A display and lookup key, not a security token: nothing is authorised *by*
/// an ID, so a collision is a `revoke` that hits the wrong rule and is
/// reported as ambiguous, never a permission that is forged. Said plainly
/// because truncating a hash usually deserves an objection, and this one has
/// an answer rather than a shrug.
const DIGEST_CHARS: usize = 12;

fn base32(digest: &[u8]) -> String {
    let mut out = String::with_capacity(DIGEST_CHARS);
    let mut bits = 0u32;
    let mut held = 0u32;
    let mut bytes = digest.iter();

    while out.len() < DIGEST_CHARS {
        if held < 5 {
            let next = *bytes.next().expect("sha256 supplies 32 bytes for 60 bits");
            bits = (bits << 8) | u32::from(next);
            held += 8;
        }
        held -= 5;
        let index = (bits >> held) & 0b11111;
        out.push(char::from(ALPHABET[index as usize]));
    }

    out
}

impl From<RuleId> for String {
    fn from(id: RuleId) -> Self {
        id.to_string()
    }
}

impl TryFrom<String> for RuleId {
    type Error = BadRuleId;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}
