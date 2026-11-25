//! Subscription quota shared by passive observations and idle reads.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitWindow {
    pub name: String,
    pub used_percentage: serde_json::Number,
    pub resets_at: Option<u64>,
    pub window_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaCredits {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimits {
    pub provider: String,
    /// Stable named-account ID; absent for legacy provider-keyed credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    pub credential: String,
    pub windows: Vec<RateLimitWindow>,
    pub plan: Option<String>,
    pub credits: Option<QuotaCredits>,
    /// Unix milliseconds, consistent with the usage report clock.
    pub observed_at: u64,
    pub source: QuotaSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    Passive,
    Active,
}

/// A host-owned local snapshot source. Reading never sends a network request.
pub trait RateLimitsReader: Send + Sync + std::fmt::Debug {
    fn rate_limits(&self) -> Vec<RateLimits>;
}

/// Classify provider windows by duration, not by primary/secondary position.
pub fn window_name(seconds: u64, fallback: &str) -> String {
    match seconds {
        10_800..=28_800 => "five_hour",
        432_000..=777_600 => "seven_day",
        2_332_800..=2_764_800 => "monthly",
        _ => fallback,
    }
    .to_owned()
}
