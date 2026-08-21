//! `--budget`: the one place a ceiling written on a command line becomes a
//! ceiling the run can be stopped by.
//!
//! `CLI.md` §3 spells it as "'2.5$' or '400k' tokens → exit 5 when hit", which
//! is two ceilings in one flag, and they are enforced in two places for a
//! reason that is not going to change:
//!
//! - **Money goes to [`BudgetGate`]**, which is the only thing that can refuse
//!   a generation *before* it is paid for. That is where a spending cap
//!   belongs; anything checked afterwards is a report, not a cap.
//! - **Tokens go to a between-turn ceiling**, because nothing in the workspace
//!   can price a token count against a limit expressed in tokens without
//!   inventing a rate. It is coarser and it says so: a turn already in flight
//!   is not interrupted by it.
//!
//! ## This file is the seam the metering rewrite lands on
//!
//! `PricingQuote` is being replaced by a `Metering` sum type on another
//! branch, which will change what a gate is constructed from. Everything this
//! binary knows about that is [`gate`] and [`ceiling_reached`] — two functions,
//! one file. When `Metering` lands, this is the edit; nothing else in
//! `ego-cli` reads a quote, a rate or a limit.

use ego_core::budget::BudgetGate;
use ego_core::cost::{Amount, CurrencyCode, Money, MoneyError};
use ego_core::state::SessionState;

use crate::cli::UsageError;

/// What `--budget` was understood to mean.
///
/// Two variants rather than a `Money` plus an `Option<u64>`, because the flag
/// takes one value: a run has a money cap or a token cap, never both, and a
/// shape that could hold both would need a rule about what happens when it
/// does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Budget {
    /// `--budget 2.5$`, enforced by the gate before each generation.
    Money(Money),
    /// `--budget 400k`, checked against the session's own totals between
    /// turns.
    Tokens(u64),
}

/// The limit a gate is built with when the caller named none.
///
/// `CLI.md` §7: hard ceilings come only from an administrator's `/etc/ego`,
/// and "ego itself imposes none". `TurnLoop` requires a gate regardless — the
/// reservation is what makes a generation payable at all — so the honest way
/// to impose nothing is the largest amount `Amount` can represent.
fn no_ceiling(currency: CurrencyCode) -> Money {
    Money::new(
        Amount::new(u128::MAX, 0).expect("scale zero is always in range"),
        currency,
    )
}

/// Parse `--budget`, or say why it is not a budget.
///
/// `2.5$` and `$2.5` are both accepted because both are things people type;
/// `400k`, `400K` and `400000` are the token form. A bare number with no
/// marker is refused rather than guessed — "budget 400" is either forty cents
/// or four hundred tokens depending on who wrote it, and picking one silently
/// is how a run costs a hundred times what its author intended.
pub fn parse(spec: &str, currency: CurrencyCode) -> Result<Budget, UsageError> {
    let spec = spec.trim();
    let refuse = |why: &str| {
        UsageError(format!(
            "`{spec}` is not a budget: {why}. Write money as `2.5$` or tokens \
             as `400k`"
        ))
    };

    if let Some(digits) = spec.strip_suffix('$').or_else(|| spec.strip_prefix('$')) {
        let amount: Amount = digits
            .trim()
            .parse()
            .map_err(|err: MoneyError| refuse(&err.to_string()))?;
        return Ok(Budget::Money(Money::new(amount, currency)));
    }

    let (digits, scale) = match spec.strip_suffix(['k', 'K']) {
        Some(digits) => (digits, 1_000),
        None => match spec.strip_suffix(['m', 'M']) {
            Some(digits) => (digits, 1_000_000),
            None => (spec, 1),
        },
    };
    if scale == 1 && !spec.is_empty() {
        return Err(refuse("it names no unit"));
    }
    let count: u64 = digits
        .trim()
        .parse()
        .map_err(|_| refuse("its token count is not a whole number"))?;
    count
        .checked_mul(scale)
        .map(Budget::Tokens)
        .ok_or_else(|| refuse("its token count does not fit in 64 bits"))
}

/// The gate this run generates under.
///
/// **The one function that turns a command-line budget into a
/// [`BudgetGate`].** A token budget still gets a gate — the loop cannot run
/// without one — and the gate is unbounded, because the ceiling that run is
/// actually subject to is [`ceiling_reached`]'s.
pub fn gate(budget: Option<&Budget>, currency: CurrencyCode) -> BudgetGate {
    match budget {
        Some(Budget::Money(limit)) => BudgetGate::new(*limit),
        Some(Budget::Tokens(_)) | None => BudgetGate::new(no_ceiling(currency)),
    }
}

/// Whether a token budget has been reached, checked between turns.
///
/// Reads `SessionState`'s own running totals rather than counting anything
/// itself: `Usage::total_tokens` is the sum core already folds, and a second
/// count here would be a second answer to how much this session has spent.
pub fn ceiling_reached(budget: Option<&Budget>, state: &SessionState) -> bool {
    match budget {
        Some(Budget::Tokens(limit)) => state.usage.total_tokens() >= *limit,
        Some(Budget::Money(_)) | None => false,
    }
}
