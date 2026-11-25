//! Read-time cost estimates for generations whose price was unknown when they ran.
//!
//! An estimate is derived while a report is built. It is never written to the
//! session log and never reaches a budget gate: recorded history stays
//! unrepriced (`cost.rs`, "History is never repriced").

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::cost::{CurrencyCode, Money, PricingUnavailable};
use crate::event::time::Timestamp;
use crate::state::Usage;

/// Which price table produced an estimate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "table", rename_all = "snake_case")]
pub enum EstimateBasis {
    /// The newest cached table that arrived before the request.
    RequestTime { sha256: String },
    /// No cached table predates the request, so today's table priced it.
    CurrentTable { sha256: String },
}

/// One estimate for one generation, or the reason there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Estimate {
    Priced { amount: Money, basis: EstimateBasis },
    Unavailable(PricingUnavailable),
}

/// Prices recorded usage from cached tables. Implemented at the edge that
/// owns the cache; the report service performs no I/O.
pub trait CostEstimator {
    fn estimate(&self, model: &str, requested_at: Timestamp, usage: &Usage) -> Estimate;
}

/// A summed estimate. Kept apart from recorded cost: it is never added to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstimatedCost {
    pub basis: EstimateBasis,
    pub amount: Money,
}

#[derive(Default)]
pub(super) struct EstimateTotals {
    known: BTreeMap<(EstimateBasis, CurrencyCode), Money>,
    pub(super) unavailable: Vec<PricingUnavailable>,
}

impl EstimateTotals {
    pub(super) fn add(&mut self, estimate: Estimate) {
        match estimate {
            Estimate::Unavailable(reason) => self.unavailable.push(reason),
            Estimate::Priced { amount, basis } => {
                let key = (basis, amount.currency);
                let sum = match self.known.get(&key) {
                    Some(current) => current.checked_add(&amount).ok(),
                    None => Some(amount),
                };
                match sum {
                    Some(sum) => {
                        self.known.insert(key, sum);
                    }
                    None => {
                        self.known.remove(&key);
                        self.unavailable.push(PricingUnavailable::InvalidRate);
                    }
                }
            }
        }
    }

    pub(super) fn finish(self) -> (Vec<EstimatedCost>, Vec<PricingUnavailable>) {
        let known = self
            .known
            .into_iter()
            .map(|((basis, _), amount)| EstimatedCost { basis, amount })
            .collect();
        (known, self.unavailable)
    }
}
