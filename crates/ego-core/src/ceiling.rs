//! What a ceiling is written as, and what a resolved one holds.
//!
//! `ARCHITECTURE.md` §§4 and 8.
//!
//! ## Why the grammar lives here and not on the flag
//!
//! `--budget 2.5$` and a config file's `budget = "2.5$"` are the same ceiling
//! written in two places, and §4 makes the config key real: an administrator's
//! `/etc/ego/config.toml` is the *only* source of non-relaxable ceilings, so
//! the key has to parse the same thing the flag does. Two parsers would drift,
//! and the direction they would drift in is the dangerous one — a spelling the
//! flag accepts and the system layer rejects is a ceiling an administrator
//! believes they set.
//!
//! So there is one [`Budget::from_str`], in the crate that owns neither
//! surface, and both call it.
//!
//! ## Why a ceiling is refused rather than guessed
//!
//! A bare number names no unit, and "budget 400" is either four hundred
//! dollars or four hundred tokens depending on who wrote it. There is no
//! reading of that which is safe to pick silently: one of the two is off by
//! whatever the model charges per token.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

use crate::cost::{Amount, CurrencyCode, Money, MoneyError};
use crate::state::Usage;

/// The currency `$` is shorthand for.
///
/// A fixed alias and not a configurable one, because the notation is only
/// safe *because* it is fixed: [`BudgetGate::reserve`](crate::budget::
/// BudgetGate::reserve) refuses a ceiling in a currency the quote is not in
/// rather than converting it, so a EUR-priced row under a `$` ceiling fails
/// loudly at the first reservation instead of being silently rebased.
const DOLLAR: &str = "USD";

/// What one `budget` value was understood to mean.
///
/// Two variants rather than a `Money` plus an `Option<u64>`, because one
/// written value is one ceiling: a layer says `budget = "2.5$"` or
/// `budget = "400k"`, never both at once. Layers that disagree about *which*
/// kind are merged into [`Ceilings`], which is where both can coexist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Budget {
    /// `2.5$`, `2.5USD`, `10EUR` — enforced by the gate before each
    /// generation, because money spent cannot be taken back.
    Money(Money),
    /// `400k`, `2m`, `400000` — checked against the session's own billed
    /// totals at each generation boundary. Needs no rates, no fetch and no
    /// network, and binds on every metering class.
    Tokens(u64),
}

impl FromStr for Budget {
    type Err = BudgetSpecError;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let trimmed = spec.trim();
        let refuse = |why: String| BudgetSpecError {
            spec: spec.to_string(),
            why,
        };

        // `2.5$` and `$2.5`: both are things people type, and `CLI.md:128`
        // publishes the first.
        if let Some(digits) = trimmed
            .strip_suffix('$')
            .or_else(|| trimmed.strip_prefix('$'))
        {
            return money(digits.trim(), DOLLAR).map_err(|err| refuse(err.to_string()));
        }

        // `10EUR`: a decimal followed by an ISO 4217 code. Three uppercase
        // letters cannot collide with the token form's one-letter `k`/`m`
        // scale, so the two arms are unambiguous without lookahead.
        if let Some(code) = iso_4217_suffix(trimmed) {
            let digits = &trimmed[..trimmed.len() - code.len()];
            return money(digits.trim(), code).map_err(|err| refuse(err.to_string()));
        }

        let (digits, scale) = match trimmed.strip_suffix(['k', 'K']) {
            Some(digits) => (digits, 1_000u64),
            None => match trimmed.strip_suffix(['m', 'M']) {
                Some(digits) => (digits, 1_000_000),
                None => (trimmed, 1),
            },
        };
        if scale == 1 {
            // Deliberately including the empty string, which names no unit
            // for the same reason a bare number does not.
            return Err(refuse("it names no unit".to_string()));
        }
        let count: u64 = digits
            .trim()
            .parse()
            .map_err(|_| refuse("its token count is not a whole number".to_string()))?;
        count
            .checked_mul(scale)
            .map(Budget::Tokens)
            .ok_or_else(|| refuse("its token count does not fit in 64 bits".to_string()))
    }
}

impl fmt::Display for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Budget::Money(limit) => write!(f, "{}{}", limit.amount, limit.currency),
            Budget::Tokens(count) => write!(f, "{count} tokens"),
        }
    }
}

impl<'de> Deserialize<'de> for Budget {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// `digits` in `code`, or why that is not an amount.
///
/// The strict parser is inherited on purpose: [`Amount::from_str`] rejects
/// signs, exponents and thousands separators, so there is exactly one
/// spelling per value and `1,000$` is refused rather than read as one dollar.
fn money(digits: &str, code: &str) -> Result<Budget, MoneyError> {
    let amount: Amount = digits.parse()?;
    Ok(Budget::Money(Money::new(amount, CurrencyCode::new(code)?)))
}

/// The trailing three-letter currency code, if `spec` ends in one.
///
/// Shape only — this does not know which codes exist, because
/// [`CurrencyCode`] does not either and a list of live ISO 4217 codes
/// vendored here would go stale in a way nothing in this repository could
/// detect.
fn iso_4217_suffix(spec: &str) -> Option<&str> {
    let (head, tail) = spec.split_at_checked(spec.len().checked_sub(3)?)?;
    let named = tail.len() == 3 && tail.bytes().all(|b| b.is_ascii_uppercase());
    // A code with nothing in front of it is a currency, not a ceiling.
    (named && !head.trim().is_empty()).then_some(tail)
}

/// Why a written ceiling is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetSpecError {
    /// What was written, quoted back so the operator can see what was read —
    /// including whitespace they did not intend.
    pub spec: String,
    /// The specific defect, never a generic "invalid".
    pub why: String,
}

impl fmt::Display for BudgetSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not a budget: {}. Write money as `2.5$`, `2.5USD` or \
             `10EUR`, or tokens as `400k`",
            self.spec, self.why
        )
    }
}

impl std::error::Error for BudgetSpecError {}

/// The ceilings a run is actually subject to, after every layer has had its
/// say.
///
/// Both fields, independently, because §4's clamp is per-field: an
/// administrator who caps money must not be escaped by a user who names a
/// token ceiling instead, and the reverse. `None` is the ordinary case —
/// `CLI.md:296-299` says ego imposes no ceiling of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ceilings {
    pub money: Option<Money>,
    pub tokens: Option<u64>,
}

impl Ceilings {
    /// Whether anything here needs rates to enforce, and therefore whether
    /// §5's rate sources are consulted at all.
    ///
    /// Only the money ceiling does. §9's token ceiling is checked against
    /// token counts the session already has, so a run bounded only in tokens
    /// looks up no price — which is exactly why §9 calls it the ceiling an
    /// air-gapped operator should reach for.
    ///
    /// §5 is emphatic that this is a *gate* and not an optimisation: with no
    /// money ceiling, no rate source is consulted, so an ordinary run
    /// contacts nobody but the model provider. That is a privacy property,
    /// and it has to be true before there is a third-party fetch to leak
    /// through rather than after.
    pub fn needs_rates(&self) -> bool {
        self.money.is_some()
    }

    /// Fold one layer's written ceiling in, overwriting the field it names
    /// and leaving the other alone.
    ///
    /// Overwriting rather than taking a minimum is right for the ordinary
    /// merge: a higher-precedence layer that names a *larger* money ceiling
    /// is an operator raising their own limit, which is theirs to do. Only
    /// [`clamp`](Self::clamp) is a floor-through, and only a `system` layer
    /// reaches it.
    pub fn apply(&mut self, budget: &Budget) {
        match budget {
            Budget::Money(limit) => self.money = Some(*limit),
            Budget::Tokens(count) => self.tokens = Some(*count),
        }
    }

    /// Reduce these ceilings to `ceiling` where `ceiling` is stricter,
    /// reporting each reduction.
    ///
    /// `CLI.md:311` — "system ceilings binding everyone" — so the operation
    /// is `min` and not "the administrator wins outright": an operator who
    /// asked for *less* than the administrator's cap keeps their own figure.
    ///
    /// A money ceiling in a currency the administrator's is not in cannot be
    /// compared, and is therefore replaced outright rather than let through.
    /// Converting would need a rate, and a rate would make the administrator's
    /// cap depend on a table they did not write.
    ///
    /// Returns what was reduced, because §4 requires the clamp be **stated,
    /// not silent** — otherwise `--budget 20$` reads as binding at $20 when
    /// it binds at $5.
    pub fn clamp(&mut self, ceiling: &Ceilings) -> Clamped {
        let mut clamped = Clamped::default();

        if let Some(cap) = ceiling.money {
            match self.money {
                Some(asked) if asked.currency == cap.currency && asked.amount <= cap.amount => {}
                Some(asked) => {
                    self.money = Some(cap);
                    clamped.money = Some(asked);
                }
                None => self.money = Some(cap),
            }
        }
        if let Some(cap) = ceiling.tokens {
            match self.tokens {
                Some(asked) if asked <= cap => {}
                Some(asked) => {
                    self.tokens = Some(cap);
                    clamped.tokens = Some(asked);
                }
                None => self.tokens = Some(cap),
            }
        }
        clamped
    }
}

/// Whether a session's billed tokens have reached `ceiling`.
///
/// One definition, called from both places that ask — the generation
/// boundary inside `TurnLoop` and the turn boundary in the binary — because
/// two spellings of "has the ceiling been reached" is two answers to how much
/// this session has spent.
///
/// The quantity is [`Usage::total_tokens`], which recounts prior history as
/// input on every generation. That is deliberate and it is the *billed*
/// figure rather than the occupancy one: `--budget` is cost control, and what
/// a provider bills is what is recounted.
pub fn tokens_reached(ceiling: Option<u64>, usage: &Usage) -> bool {
    ceiling.is_some_and(|limit| usage.total_tokens() >= limit)
}

/// What an administrator's ceiling reduced, per field.
///
/// `None` on a field means it was not reduced — either nothing was asked for,
/// or what was asked for was already within the cap. It deliberately does not
/// record the resulting figure: that is in [`Ceilings`], and reporting it
/// twice would let the two disagree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Clamped {
    /// The higher money ceiling that was asked for.
    pub money: Option<Money>,
    /// The higher token ceiling that was asked for.
    pub tokens: Option<u64>,
}

impl Clamped {
    /// Whether anything was reduced at all.
    pub fn any(&self) -> bool {
        self.money.is_some() || self.tokens.is_some()
    }
}
