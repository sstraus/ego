//! Compose resolved budgets with process-owned rate sources. Monetary ceilings use `BudgetGate`
//! before generation; token ceilings are checked between generations. Both CLI and config use
//! `ego_core::ceiling::Budget` and shared configuration precedence.
//!
//! This process boundary supplies the cache, clock, transport, and egress posture. Libraries
//! receive those dependencies rather than reading process globals or opening files implicitly.

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

/// Construct the gate from resolved ceilings. Token-only runs use an unlimited money ledger and
/// enforce tokens at generation boundaries. Preserve `clamped_from` so reporting distinguishes
/// the requested limit from the administrator's binding cap. An absent limit is not a
/// largest-representable amount.
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

/// Capture third-party rate dependencies together: cache location, clock, age bound, egress
/// posture, and fetch transport must belong to the same run.
pub struct TableSource<'a> {
    pub io: &'a dyn StoreIo,
    pub ego_home: &'a Path,
    pub http: &'a Arc<dyn Http>,
    /// Whether `--offline` was asked for. Kept explicitly so the notice can
    /// distinguish a forbidden fetch from a failed online exchange.
    pub offline: bool,
    pub now: Timestamp,
    /// Maximum usable table age, supplied by the caller. `ego_tools::pricing::MAX_AGE` is the
    /// default.
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
    /// Resolve rates only when the ceiling needs them. Prefer already resolved rates; otherwise
    /// use a fresh cache and fetch only when it cannot answer. No required rate lookup means no
    /// pricing file or socket access. A fresh cache is not revalidated on every run.
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
