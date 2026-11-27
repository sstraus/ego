//! Read-time cost estimates from the cached price tables.
//!
//! The newest cached blob that arrived before the request prices the usage;
//! with none, today's table does and the figure says so. Nothing is written.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use ego_ai::pricing::Table;
use ego_ai::provider::ProviderRow;
use ego_core::cost::{CostDelta, Money, PricingRevision, PricingUnavailable};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::state::Usage;
use ego_core::usage::{CostEstimator, Estimate, EstimateBasis};
use ego_tools::pricing::{ArrivedBlob, CacheError, PricingCache};
use ego_tools::store::OsIo;

pub(super) struct CachedTableEstimator {
    rows: Vec<ProviderRow>,
    blobs: Vec<ArrivedBlob>,
    /// Today's table, whatever its age, or why there is none.
    current: Result<String, PricingUnavailable>,
    ego_home: PathBuf,
    tables: RefCell<HashMap<String, Option<Table>>>,
}

impl CachedTableEstimator {
    pub(super) fn open(ego_home: PathBuf, clock: &dyn Clock) -> Self {
        let cache = PricingCache::new(&OsIo, &ego_home);
        let current = cache
            .current(clock.now())
            .map(|cached| sha(&cached.revision))
            .map_err(|error| match error {
                CacheError::Empty => PricingUnavailable::OfflineNoCache,
                _ => PricingUnavailable::CacheUnavailable,
            });
        Self {
            rows: crate::model::rows(&HashMap::new()).unwrap_or_default(),
            blobs: cache.arrived().unwrap_or_default(),
            current,
            ego_home,
            tables: RefCell::default(),
        }
    }

    fn price(&self, sha256: &str, model: &str, usage: &Usage) -> Result<Money, PricingUnavailable> {
        let (provider, name) = model
            .split_once('/')
            .ok_or(PricingUnavailable::MissingRow)?;
        let row = self
            .rows
            .iter()
            .find(|row| row.id() == provider)
            .ok_or(PricingUnavailable::MissingRow)?;
        let mut tables = self.tables.borrow_mut();
        let table = tables
            .entry(sha256.to_owned())
            .or_insert_with(|| {
                let bytes = PricingCache::new(&OsIo, &self.ego_home)
                    .blob_bytes(sha256)
                    .ok()?;
                Table::decode(&bytes).ok()
            })
            .as_ref()
            .ok_or(PricingUnavailable::InvalidTable)?;
        let (table_provider, key_prefix) = row
            .price_table_name()
            .ok_or(PricingUnavailable::MissingRow)?;
        let entry = table
            .quote_in(table_provider, key_prefix, name)
            .map_err(|_| PricingUnavailable::MissingRow)?;
        match entry
            .into_quote(PricingRevision(sha256.to_owned()))
            .price(usage)
        {
            CostDelta::Known { amount, .. } => Ok(amount),
            CostDelta::Unknown(_) => Err(PricingUnavailable::MissingRow),
        }
    }
}

fn sha(revision: &PricingRevision) -> String {
    revision
        .0
        .rsplit_once('#')
        .map_or_else(|| revision.0.clone(), |(_, sha)| sha.to_owned())
}

impl CostEstimator for CachedTableEstimator {
    fn estimate(&self, model: &str, requested_at: Timestamp, usage: &Usage) -> Estimate {
        let request_time = self
            .blobs
            .iter()
            .filter(|blob| blob.arrived_at <= requested_at)
            .max_by_key(|blob| blob.arrived_at);
        let (sha256, at_request) = match (request_time, &self.current) {
            (Some(blob), _) => (blob.sha256.as_str(), true),
            (None, Ok(sha256)) => (sha256.as_str(), false),
            (None, Err(reason)) => return Estimate::Unavailable(*reason),
        };
        match self.price(sha256, model, usage) {
            Ok(amount) => Estimate::Priced {
                amount,
                basis: if at_request {
                    EstimateBasis::RequestTime {
                        sha256: sha256.to_owned(),
                    }
                } else {
                    EstimateBasis::CurrentTable {
                        sha256: sha256.to_owned(),
                    }
                },
            },
            Err(reason) => Estimate::Unavailable(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use ego_core::digest::sha256_hex;
    use ego_tools::clock::SystemClock;

    use super::*;

    fn table(input: &str) -> Vec<u8> {
        format!(
            r#"{{"claude-x":{{"input_cost_per_token":{input},"output_cost_per_token":{input},"litellm_provider":"anthropic","mode":"chat"}}}}"#
        )
        .into_bytes()
    }

    fn store(dir: &std::path::Path, bytes: &[u8], arrived_ms: u64) -> String {
        let sha = sha256_hex(bytes);
        let path = dir.join("pricing").join(format!("{sha}.json"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(arrived_ms))
            .unwrap();
        sha
    }

    // Catches: an estimate priced from the newest table instead of the one in force at request time.
    #[test]
    fn estimate_uses_the_table_that_arrived_before_the_request() {
        let home = tempfile::tempdir().unwrap();
        let old = store(home.path(), &table("0.000001"), 1_000);
        store(home.path(), &table("0.000009"), 3_000);
        let estimator = CachedTableEstimator::open(home.path().to_owned(), &SystemClock);
        let usage = Usage {
            uncached_input_tokens: 1_000_000,
            ..Usage::IDENTITY
        };
        let Estimate::Priced { amount, basis } =
            estimator.estimate("anthropic/claude-x", Timestamp::from_millis(2_000), &usage)
        else {
            panic!("the older table prices claude-x");
        };
        assert_eq!(amount.amount.to_string(), "1");
        assert_eq!(basis, EstimateBasis::RequestTime { sha256: old });
    }
}
