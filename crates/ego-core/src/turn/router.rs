//! One per-turn policy boundary; tiers come from the existing ordered candidates.
use crate::config::Probability;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// Closed routing classifications contain no judged text or provider errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterStatus {
    Cheap,
    Strong,
    Grey,
    Off,
    Uncalibrated,
    Timeout,
    Error,
}

/// A routing observation; absent cost is unknown, never zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterOutcome {
    pub status: RouterStatus,
    pub probability: Option<Probability>,
    pub cost_usd: Option<String>,
}
impl RouterOutcome {
    /// A fallback outcome without an answered judge request.
    pub fn fallback(status: RouterStatus) -> Self {
        Self {
            status,
            probability: None,
            cost_usd: None,
        }
    }
}

/// Bounded external routing judgment; adapters enforce the 800 ms hop deadline.
/// The turn retains candidate and cancellation authority.
pub trait TurnRouter: Send + Sync {
    fn route<'a>(&'a self, text: &'a str) -> BoxFuture<'a, RouterOutcome>;
}
