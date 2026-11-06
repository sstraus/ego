//! Deterministic usage reporting over the durable session ledger.

mod context_attribution;
pub use context_attribution::{
    ContextAttribution, ToolContextCall, ToolContextEstimate, ToolContextResult,
};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU64;

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::cost::{
    Amount, CostAttribution, CostDelta, CurrencyCode, Money, SubscriptionEquivalent, UnknownCost,
};
use crate::event::domain::{GenerationRole, OccupancyProvenance};
use crate::event::id::SessionId;
use crate::event::time::{Clock, Timestamp};
use crate::session::SessionLineage;
use crate::state::{Charge, ChargeAttribution, SessionState, Usage};
use crate::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};

/// The v5 contract adds observational tool context attribution.
/// Old report snapshots are not accepted through compatibility defaults.
pub const USAGE_REPORT_VERSION: u32 = 5;

/// A deterministic workspace or active-session usage report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    pub v: u32,
    pub as_of: Timestamp,
    pub period: UsagePeriod,
    pub sessions: Vec<SessionUsage>,
    pub totals: UsageLines,
    /// Current-credential observations, not historical session charges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_limits: Vec<ProviderLimitsObservation>,
}

/// Which original generation timestamps a report includes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsagePeriod {
    All,
    Last { seconds: NonZeroU64 },
}

/// One local session's usage and current occupancy facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub session: SessionId,
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineage: Option<SessionLineage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_capacity: Option<ContextCapacity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_limits: Option<ProviderLimitsObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<PromptCacheDeadline>,
    pub context_attribution: ContextAttribution,
    pub lines: UsageLines,
}

/// Lifetime selected by the request mapping, or unknown for implicit caching.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCachePolicy {
    pub ttl_seconds: Option<NonZeroU64>,
    pub refresh_on_read: bool,
    pub lock_ttl: bool,
}

/// Cache policy of one admitted provider request, bound to its exact model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheRequest {
    pub model: String,
    #[serde(flatten)]
    pub policy: PromptCachePolicy,
}

/// The one session-owned cache deadline. It is an estimate from request policy,
/// not proof that the provider stored or will reuse a prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheDeadline {
    pub model: String,
    pub ttl_seconds: NonZeroU64,
    pub expires_at: Timestamp,
}

impl PromptCacheDeadline {
    pub fn remaining_seconds(&self, now: Timestamp) -> u64 {
        self.expires_at
            .as_millis()
            .saturating_sub(now.as_millis())
            .div_ceil(1_000)
    }

    pub(crate) fn observe(
        previous: Option<&Self>,
        request: &PromptCacheRequest,
        now: Timestamp,
    ) -> Option<Self> {
        let ttl = request.policy.ttl_seconds?;
        let live = previous
            .filter(|previous| previous.model == request.model && previous.expires_at > now);
        let ttl = if request.policy.lock_ttl {
            live.map_or(ttl, |previous| previous.ttl_seconds)
        } else {
            ttl
        };
        if !request.policy.refresh_on_read
            && let Some(previous) = live.filter(|previous| previous.ttl_seconds == ttl)
        {
            return Some(previous.clone());
        }
        let millis = ttl.get().checked_mul(1_000)?;
        let expires_at = Timestamp::from_millis(now.as_millis().checked_add(millis)?);
        Some(Self {
            model: request.model.clone(),
            ttl_seconds: ttl,
            expires_at,
        })
    }
}

/// The latest complete context-window observation for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub used_tokens: u64,
    pub provenance: OccupancyProvenance,
}

/// A capacity stated by the generation that observed the context, never a catalogue guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCapacity {
    pub tokens: u64,
    pub source: String,
    pub observed_at: Timestamp,
}

/// One provider's point-in-time limits. These are levels, never additive usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLimitsObservation {
    pub provider: String,
    /// Present for a response from a session generation. Absent for the command's current key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub observed_at: Timestamp,
    pub limits: Vec<ProviderLimit>,
}

/// The unit and boundary of an explicitly reported remaining value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderLimitKind {
    Requests,
    Tokens,
    InputTokens,
    OutputTokens,
    Usd,
}

/// Only provider-stated values. No subtraction, reset prediction or local-spend reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLimit {
    pub kind: ProviderLimitKind,
    pub remaining: Amount,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset: Option<String>,
    pub source: String,
}

/// The fixed accounting lines under the one durable ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLines {
    pub main: UsageLine,
    pub judge: UsageLine,
    pub goal_check: UsageLine,
    pub compaction: UsageLine,
    pub subagent: UsageLine,
    pub hook: UsageLine,
    pub total: UsageLine,
}

/// Token and cost totals for one line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UsageLine {
    pub usage: Usage,
    pub cost: CostBreakdown,
}

impl UsageLine {
    /// Token-weighted cache reads from the corrected durable input categories.
    pub fn cache_hit(&self) -> Option<f64> {
        let input = u128::from(self.usage.uncached_input_tokens)
            + u128::from(self.usage.cache_read_tokens)
            + u128::from(self.usage.cache_write_tokens)
            + u128::from(self.usage.cache_write_extended_tokens);
        (input != 0).then(|| self.usage.cache_read_tokens as f64 / input as f64)
    }
}

impl Serialize for UsageLine {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Line<'a> {
            usage: &'a Usage,
            cost: &'a CostBreakdown,
            #[serde(skip_serializing_if = "Option::is_none")]
            cache_hit: Option<f64>,
        }
        Line {
            usage: &self.usage,
            cost: &self.cost,
            cache_hit: self.cache_hit(),
        }
        .serialize(serializer)
    }
}

/// Exact per-currency totals plus every durable unknown-cost reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub known: Vec<Money>,
    pub unknown: Vec<UnknownCost>,
    /// Display-only standard API equivalents, never monetary charges.
    pub subscription_equivalent: Vec<Money>,
    /// Missing or overflowing policy estimates (including unmetered usage), not monetary unknowns.
    pub subscription_unknown: Vec<UnknownCost>,
}

/// A report cannot truthfully place legacy spend on a generation-role line.
#[derive(Debug)]
pub enum UsageReportError {
    Store(StoreError),
    UnattributedCharge {
        session: SessionId,
        charge_index: usize,
    },
}

impl fmt::Display for UsageReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(source) => write!(formatter, "{source}"),
            Self::UnattributedCharge {
                session,
                charge_index,
            } => write!(
                formatter,
                "session {session} has unattributed legacy spend at charge {charge_index}"
            ),
        }
    }
}

impl std::error::Error for UsageReportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(source) => Some(source),
            Self::UnattributedCharge { .. } => None,
        }
    }
}

impl From<StoreError> for UsageReportError {
    fn from(source: StoreError) -> Self {
        Self::Store(source)
    }
}

/// Builds reports from the existing durable-session and clock ports.
pub struct UsageReportService<'a> {
    store: &'a dyn SessionStore,
    clock: &'a dyn Clock,
}

impl<'a> UsageReportService<'a> {
    pub fn new(store: &'a dyn SessionStore, clock: &'a dyn Clock) -> Self {
        Self { store, clock }
    }

    /// Report the supplied local sessions without traversing their lineage.
    pub async fn report(
        &self,
        metadata: &[SessionMeta],
        period: UsagePeriod,
    ) -> Result<UsageReport, UsageReportError> {
        let as_of = self.clock.now();
        let mut metadata = metadata.to_vec();
        metadata.sort_by_key(|meta| meta.id);
        metadata.dedup_by_key(|meta| meta.id);

        let mut totals = LinesAccumulator::new();
        let mut sessions = Vec::with_capacity(metadata.len());

        for meta in metadata {
            let mut state = SessionState::default();
            let mut entries = self.store.read(&meta.id)?;
            let mut attribution = context_attribution::AttributionBuilder::default();
            let mut context_capacity = None;
            let mut provider_limits = None;
            while let Some(entry) = entries.next().await {
                if let SessionEntry::Event(event) = entry? {
                    if event.env.ts <= as_of {
                        attribution.observe(&event, period.includes(event.env.ts, as_of));
                    }
                    match &event.kind {
                        crate::event::domain::EventKind::ContextWindowUpdated {
                            window, ..
                        } => {
                            context_capacity =
                                window
                                    .filter(|tokens| *tokens > 0)
                                    .map(|tokens| ContextCapacity {
                                        tokens,
                                        source: "context_window_updated".to_owned(),
                                        observed_at: event.env.ts,
                                    });
                        }
                        // A judge or fallback may use a different provider. Do not lend its quota
                        // to the session's displayed model. Sequence order, not wall time, wins.
                        crate::event::domain::EventKind::ProviderLimitsObserved {
                            provider,
                            model,
                            limits,
                        } if meta.model.as_deref() == Some(model.as_str())
                            && model.split_once('/').is_some_and(|(id, _)| id == provider) =>
                        {
                            provider_limits = Some(ProviderLimitsObservation {
                                provider: provider.clone(),
                                model: Some(model.clone()),
                                observed_at: event.env.ts,
                                limits: limits.clone(),
                            });
                        }
                        _ => {}
                    }
                    state.apply(&event);
                }
            }

            if let Some(charge_index) = state.charges.iter().position(|charge| {
                matches!(charge.attribution, ChargeAttribution::LegacyUnattributed)
            }) {
                return Err(UsageReportError::UnattributedCharge {
                    session: meta.id,
                    charge_index,
                });
            }

            let mut lines = LinesAccumulator::new();
            for charge in &state.charges {
                let ChargeAttribution::Generation {
                    role, started_at, ..
                } = charge.attribution
                else {
                    continue;
                };
                if period.includes(started_at, as_of) {
                    lines.add(role, charge);
                    totals.add(role, charge);
                }
            }

            let context = match (state.context.used_tokens, state.context.provenance) {
                (Some(used_tokens), Some(provenance)) => Some(ContextUsage {
                    used_tokens,
                    provenance,
                }),
                _ => None,
            };
            let prompt_cache = state
                .prompt_cache
                .filter(|cache| meta.model.as_deref() == Some(cache.model.as_str()));
            sessions.push(SessionUsage {
                session: meta.id,
                model: meta.model,
                lineage: meta.lineage,
                context,
                context_capacity,
                provider_limits,
                prompt_cache,
                context_attribution: attribution.finish(),
                lines: lines.finish(),
            });
        }

        Ok(UsageReport {
            v: USAGE_REPORT_VERSION,
            as_of,
            period,
            sessions,
            totals: totals.finish(),
            provider_limits: Vec::new(),
        })
    }
}

impl UsagePeriod {
    fn includes(self, started_at: Timestamp, as_of: Timestamp) -> bool {
        if started_at > as_of {
            return false;
        }
        match self {
            Self::All => true,
            Self::Last { seconds } => {
                let window_millis = seconds.get().saturating_mul(1_000);
                let lower = as_of.as_millis().saturating_sub(window_millis);
                started_at.as_millis() >= lower
            }
        }
    }
}

struct LinesAccumulator {
    main: LineAccumulator,
    judge: LineAccumulator,
    goal_check: LineAccumulator,
    compaction: LineAccumulator,
    subagent: LineAccumulator,
    hook: LineAccumulator,
    total: LineAccumulator,
}

impl LinesAccumulator {
    fn new() -> Self {
        Self {
            main: LineAccumulator::new(),
            judge: LineAccumulator::new(),
            goal_check: LineAccumulator::new(),
            compaction: LineAccumulator::new(),
            subagent: LineAccumulator::new(),
            hook: LineAccumulator::new(),
            total: LineAccumulator::new(),
        }
    }

    fn add(&mut self, role: GenerationRole, charge: &Charge) {
        match role {
            GenerationRole::Main => self.main.add(charge),
            GenerationRole::Judge => self.judge.add(charge),
            GenerationRole::GoalCheck => self.goal_check.add(charge),
            GenerationRole::Compaction => self.compaction.add(charge),
            GenerationRole::Subagent => self.subagent.add(charge),
            GenerationRole::Hook => self.hook.add(charge),
        }
        self.total.add(charge);
    }

    fn finish(self) -> UsageLines {
        UsageLines {
            main: self.main.finish(),
            judge: self.judge.finish(),
            goal_check: self.goal_check.finish(),
            compaction: self.compaction.finish(),
            subagent: self.subagent.finish(),
            hook: self.hook.finish(),
            total: self.total.finish(),
        }
    }
}

struct LineAccumulator {
    usage: Usage,
    monetary: CostTotals,
    subscription: CostTotals,
}

impl LineAccumulator {
    fn new() -> Self {
        Self {
            usage: Usage::IDENTITY,
            monetary: CostTotals::default(),
            subscription: CostTotals::default(),
        }
    }

    fn add(&mut self, charge: &Charge) {
        self.usage.add(&charge.usage);
        match &charge.cost {
            CostDelta::Unknown(reason) => self.monetary.unknown.push(reason.clone()),
            CostDelta::Known {
                attribution: CostAttribution::MeteringPolicy { equivalent, .. },
                ..
            } => match equivalent.as_deref() {
                Some(SubscriptionEquivalent::V1 { amount, .. }) => self.subscription.add(amount),
                None => self.subscription.unknown.push(UnknownCost::NoPricingQuote),
            },
            CostDelta::Known { amount, .. } => self.monetary.add(amount),
        }
    }

    fn finish(self) -> UsageLine {
        UsageLine {
            usage: self.usage,
            cost: CostBreakdown {
                known: self.monetary.known.into_values().collect(),
                unknown: self.monetary.unknown,
                subscription_equivalent: self.subscription.known.into_values().collect(),
                subscription_unknown: self.subscription.unknown,
            },
        }
    }
}

#[derive(Default)]
struct CostTotals {
    known: BTreeMap<CurrencyCode, Money>,
    overflowed: BTreeSet<CurrencyCode>,
    unknown: Vec<UnknownCost>,
}

impl CostTotals {
    fn add(&mut self, amount: &Money) {
        if self.overflowed.contains(&amount.currency) {
            return;
        }
        if let Some(current) = self.known.get(&amount.currency).copied() {
            match current.checked_add(amount) {
                Ok(total) => {
                    self.known.insert(amount.currency, total);
                }
                Err(_) => {
                    self.known.remove(&amount.currency);
                    self.overflowed.insert(amount.currency);
                    self.unknown.push(UnknownCost::Overflow);
                }
            }
        } else {
            self.known.insert(amount.currency, *amount);
        }
    }
}
