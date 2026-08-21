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
//! - **Tokens go to a between-generation ceiling** inside `TurnLoop`, because
//!   nothing in the workspace can price a token count against a limit
//!   expressed in tokens without inventing a rate.
//!
//! ## Where a ceiling comes from
//!
//! Not from here. `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §4:
//! `CLI.md:296-299` makes an administrator's `/etc/ego/config.toml` "the only
//! source of ceilings", so `--budget` is not parsed on the side — it is
//! written into the flags layer (`config::flags_layer`) as the `budget` key
//! and resolved by ordinary precedence, then clamped by any `system` layer's
//! own. A flag parsed here would be a second precedence rule, and the layer
//! it would silently outrank is the administrator's.
//!
//! What is left in this file is the one thing that is genuinely process
//! knowledge: turning a *resolved* ceiling into the gate a run generates
//! under. The grammar itself is `ego_core::ceiling::Budget`, shared with the
//! config key so the two surfaces cannot drift.

use ego_core::budget::BudgetGate;
use ego_core::ceiling::Ceilings;
use ego_core::cost::{CurrencyCode, Money};

/// The gate this run generates under.
///
/// **The one function that turns a resolved ceiling into a [`BudgetGate`].**
/// A run with only a token ceiling still gets a gate — the loop cannot run
/// without one — and that gate is [`unlimited`](BudgetGate::unlimited),
/// because the ceiling such a run is actually subject to is checked at the
/// generation boundary instead.
///
/// `clamped_from` is the higher figure an administrator's ceiling reduced,
/// when one did. It is passed rather than dropped because §4 requires the
/// clamp be *stated, not silent*: the gate carries it into
/// `CeilingEffect::Binds`, so a run under a reduced ceiling says which figure
/// binds and which one was asked for.
///
/// `CLI.md` §7: hard ceilings come only from an administrator's `/etc/ego`,
/// and "ego itself imposes none". Until `Ledger::Unlimited` existed the
/// honest-sounding way to impose nothing was the largest amount `Amount` can
/// represent, which made "nobody set a budget" indistinguishable from
/// "somebody set an absurd one".
pub fn gate(
    ceilings: &Ceilings,
    clamped_from: Option<Money>,
    currency: CurrencyCode,
) -> BudgetGate {
    match (ceilings.money, clamped_from) {
        (Some(limit), Some(requested)) => BudgetGate::clamped(limit, requested),
        (Some(limit), None) => BudgetGate::new(limit),
        (None, _) => BudgetGate::unlimited(currency),
    }
}
