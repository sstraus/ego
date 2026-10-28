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
//! Not from here. `ARCHITECTURE.md` §4:
//! `CLI.md:296-299` makes an administrator's `/etc/ego/config.toml` "the only
//! source of ceilings", so `--budget` is not parsed on the side — it is
//! written into the flags layer (`config::flags_layer`) as the `budget` key
//! and resolved by ordinary precedence, then clamped by any `system` layer's
//! own. A flag parsed here would be a second precedence rule, and the layer
//! it would silently outrank is the administrator's.
//!
//! What is left in this file is the two things that are genuinely process
//! knowledge: turning a *resolved* ceiling into the gate a run generates
//! under, and finding the rates that ceiling will rest on. The grammar itself
//! is `ego_core::ceiling::Budget`, shared with the config key so the two
//! surfaces cannot drift.
//!
//! ## Why the table lookup is here and not in a library
//!
//! [`TableSource`] is the only place in the workspace where the three crates
//! that own the pieces meet. `ego-tools` owns the cache because it owns
//! `StoreIo`; `ego-ai` owns the fetch and the decode because it owns `Http`;
//! `ego-core` owns the types and opens nothing. None of the three may depend
//! on another, and §6 drew the split that way on purpose — so the composition
//! belongs to the binary that already holds all three, beside the ceiling that
//! is the reason to perform it at all.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ego_ai::catalogue::{ResolvedModel, table_rates};
use ego_ai::http::Http;
use ego_ai::pricing::{self, FetchError, Fetched, Table, TableError};
use ego_core::budget::{BudgetGate, Ledger, ResolvedRates};
use ego_core::ceiling::Ceilings;
use ego_core::cost::{CurrencyCode, Money, PricingRevision, PricingUnavailable};
use ego_core::event::time::Timestamp;
use ego_tools::pricing::{CacheError, Cached, PricingCache};
use ego_tools::store::StoreIo;

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

/// Everything §5's third rate source needs, which a run with no ceiling never
/// builds.
///
/// A struct rather than six parameters because five of the six are the *same*
/// fact from different angles — where the cache lives, what time it is, how
/// old is too old, whether egress is allowed, and what to ask if it is — and a
/// caller that could pass them separately could pass one run's clock with
/// another run's posture.
pub struct TableSource<'a> {
    pub io: &'a dyn StoreIo,
    pub ego_home: &'a Path,
    pub http: &'a Arc<dyn Http>,
    /// Whether `--offline` was asked for. Kept explicitly so the notice can
    /// distinguish a forbidden fetch from a failed online exchange.
    pub offline: bool,
    pub now: Timestamp,
    /// How old a table may be and still bound a ceiling.
    ///
    /// Supplied, because §6 makes it configurable and this file is not where a
    /// policy is decided. `ego_tools::pricing::MAX_AGE` is the default.
    pub max_age: Duration,
}

/// Why a ceiling could not be given rates to rest on.
///
/// Model resolution turns these errors into a safe notice cause, not a refusal.
/// Raw source errors remain local; only the closed category crosses the notice boundary.
#[derive(Debug)]
pub enum NoRates {
    MissingRow {
        provider: String,
        model: String,
    },
    /// `--offline`, and nothing usable is cached. The cache category tells
    /// the operator whether to fetch a first table, refresh it, or repair it.
    Offline {
        cache: CacheError,
    },
    /// Online, and the fetch did not produce a table. A fresh cache avoids
    /// fetching altogether.
    Fetch {
        source: FetchError,
    },
    /// A table was found and cannot be read for this model.
    ///
    /// Not an unlisted model. This is the document being wrong:
    /// bytes that are not a table, a rate that is not a number, or a model
    /// listed twice under two spellings with different prices.
    Table {
        source: TableError,
    },
    /// The cache itself could not be read or written.
    Cache {
        source: CacheError,
    },
}

impl NoRates {
    /// Retain the actionable category, never raw source text or identifiers.
    pub fn cause(&self) -> PricingUnavailable {
        fn table(source: &TableError) -> PricingUnavailable {
            match source {
                TableError::NotATable { .. } => PricingUnavailable::InvalidTable,
                TableError::NoEntry { .. } => PricingUnavailable::MissingRow,
                TableError::Rate { .. } => PricingUnavailable::InvalidRate,
                TableError::Contradiction { .. } => PricingUnavailable::ConflictingRates,
            }
        }
        match self {
            Self::MissingRow { .. } => PricingUnavailable::MissingRow,
            Self::Offline {
                cache: CacheError::Empty,
            } => PricingUnavailable::OfflineNoCache,
            Self::Offline {
                cache: CacheError::Stale { .. },
            } => PricingUnavailable::OfflineStaleCache,
            Self::Offline { .. } => PricingUnavailable::OfflineInvalidCache,
            Self::Fetch {
                source: FetchError::Unreachable { .. },
            } => PricingUnavailable::FetchFailed,
            Self::Fetch {
                source: FetchError::NotATable { source },
            } => table(source),
            Self::Fetch {
                source: FetchError::Unidentified { .. },
            } => PricingUnavailable::UnidentifiedTable,
            Self::Table { source } => table(source),
            Self::Cache { .. } => PricingUnavailable::CacheUnavailable,
        }
    }
}

impl fmt::Display for NoRates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.cause(), f)
    }
}

impl std::error::Error for NoRates {}

impl TableSource<'_> {
    /// The rates this run's ceiling will rest on, or the reason there are
    /// none.
    ///
    /// §5's order, first hit wins, and the gate in front of all of it:
    ///
    /// 1. **No ceiling that needs rates?** Nothing is consulted — no file is
    ///    opened and no socket. That is §7's first row and it is a privacy
    ///    property rather than an optimisation: an ordinary run contacts
    ///    nobody but the model provider, and a run whose row is metered by
    ///    policy rather than per token gets the same answer, because a limit
    ///    that admits every generation cannot be changed by any table.
    /// 2. **Already resolved?** An operator's own figures, or the provider's
    ///    published ones, are never second-guessed by a third party's summary.
    /// 3. Otherwise the cache, and the network only if the cache cannot
    ///    answer.
    ///
    /// The fetch is attempted **only** when nothing usable is cached, which
    /// makes §7's fifth row — "online, fetch fails, cache fresh" — unreachable
    /// rather than unhandled. That row distinguishes itself from the fourth
    /// only in a design that revalidates on every priced run, and this one
    /// does not: a fresh table is used as it stands.
    pub async fn rates(
        &self,
        model: &ResolvedModel,
        ledger: &Ledger,
    ) -> Result<Option<ResolvedRates>, NoRates> {
        if !model.metering.ceiling_needs_rates(ledger) {
            return Ok(None);
        }
        if model.rates.is_some() {
            return Ok(model.rates.clone());
        }

        let cache = PricingCache::new(self.io, self.ego_home);
        // Read whatever is here regardless of age, then judge it. The stale
        // copy is not useless: its validator is what makes the refresh below
        // cost one round trip instead of the whole document.
        let held = cache.current(self.now);
        if let Ok(fresh) = &held
            && self.age(fresh) <= self.max_age
        {
            return self.price(model, &fresh.bytes, &fresh.revision);
        }

        if self.offline {
            return Err(NoRates::Offline {
                cache: match held {
                    Ok(cached) => CacheError::Stale {
                        age: self.age(&cached),
                        max_age: self.max_age,
                    },
                    Err(error) => error,
                },
            });
        }
        self.refresh(model, held.ok()).await
    }

    fn age(&self, cached: &Cached) -> Duration {
        Duration::from_millis(
            self.now
                .as_millis()
                .saturating_sub(cached.fetched_at.as_millis()),
        )
    }

    /// Fetch, store, and price — offering `stale` back so an unchanged table
    /// costs no body.
    ///
    /// The lease is taken *before* the fetch rather than around the write, so
    /// two ego processes that both notice a stale table do not both download
    /// 1.8 MB. It is `PricingCache::lease`'s stated reason for being public.
    async fn refresh(
        &self,
        model: &ResolvedModel,
        stale: Option<Cached>,
    ) -> Result<Option<ResolvedRates>, NoRates> {
        let cache = PricingCache::new(self.io, self.ego_home);
        let held = cache
            .lease(self.now, std::process::id(), &crate::session::assume_alive)
            .map_err(|source| NoRates::Cache { source })?;

        let known = stale.as_ref().map(|cached| cached.upstream_version.clone());
        let fetched = pricing::fetch(self.http, known.as_deref())
            .await
            .map_err(|source| NoRates::Fetch { source })?;

        // Both arms publish. `Unchanged` republishes the bytes already on disk
        // under a fresh timestamp, because the freshness gate is about when
        // ego last *checked* and not about when the document last changed —
        // and the blob is named by its content hash, so no second copy of
        // identical bytes is written.
        let (bytes, version) = match (fetched, stale) {
            (
                Fetched::Table {
                    bytes,
                    upstream_version,
                },
                _,
            ) => (bytes, upstream_version),
            (Fetched::Unchanged, Some(cached)) => (cached.bytes, cached.upstream_version),
            // A `304` with nothing cached would mean the server answered a
            // conditional request nobody made conditional. Refused rather than
            // guessed at: there are no bytes to price from either way.
            (Fetched::Unchanged, None) => {
                return Err(NoRates::Fetch {
                    source: FetchError::Unreachable {
                        endpoint: pricing::UPSTREAM.to_string(),
                        message: "it answered 304 to an unconditional request".to_string(),
                    },
                });
            }
        };

        let revision = cache
            .publish_held(&bytes, &version, self.now)
            .map_err(|source| NoRates::Cache { source })?;
        drop(held);
        self.price(model, &bytes, &revision)
    }

    /// What `bytes` say this model costs, or nothing if they do not say.
    fn price(
        &self,
        model: &ResolvedModel,
        bytes: &[u8],
        revision: &PricingRevision,
    ) -> Result<Option<ResolvedRates>, NoRates> {
        let table = Table::decode(bytes).map_err(|source| NoRates::Table { source })?;
        match table.quote(model.row.id(), &model.model) {
            Ok(entry) => {
                let rates = table_rates(entry.into_quote(revision.clone()));
                Ok(Some(rates))
            }
            // Model resolution carries this cause into the durable anomaly notice
            // while leaving admission to the permissive budget gate.
            Err(TableError::NoEntry { provider, model }) => {
                Err(NoRates::MissingRow { provider, model })
            }
            Err(source) => Err(NoRates::Table { source }),
        }
    }
}
