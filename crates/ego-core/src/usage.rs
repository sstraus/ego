//! Deterministic usage reporting over the durable session ledger.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU64;

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::cost::{CostDelta, CurrencyCode, Money, UnknownCost};
use crate::event::domain::{GenerationRole, OccupancyProvenance};
use crate::event::id::SessionId;
use crate::event::time::{Clock, Timestamp};
use crate::session::SessionLineage;
use crate::state::{Charge, ChargeAttribution, SessionState, Usage};
use crate::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};

/// The externally serialized usage-report contract.
pub const USAGE_REPORT_VERSION: u32 = 1;

/// A deterministic workspace or active-session usage report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    pub v: u32,
    pub as_of: Timestamp,
    pub period: UsagePeriod,
    pub sessions: Vec<SessionUsage>,
    pub totals: UsageLines,
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
    pub lines: UsageLines,
}

/// The latest complete context-window observation for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub used_tokens: u64,
    pub provenance: OccupancyProvenance,
}

/// The fixed accounting lines under the one durable ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLines {
    pub main: UsageLine,
    pub judge: UsageLine,
    pub compaction: UsageLine,
    pub total: UsageLine,
}

/// Token and cost totals for one line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLine {
    pub usage: Usage,
    pub cost: CostBreakdown,
}

/// Exact per-currency totals plus every durable unknown-cost reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub known: Vec<Money>,
    pub unknown: Vec<UnknownCost>,
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
            while let Some(entry) = entries.next().await {
                if let SessionEntry::Event(event) = entry? {
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
            sessions.push(SessionUsage {
                session: meta.id,
                model: meta.model,
                lineage: meta.lineage,
                context,
                lines: lines.finish(),
            });
        }

        Ok(UsageReport {
            v: USAGE_REPORT_VERSION,
            as_of,
            period,
            sessions,
            totals: totals.finish(),
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
    compaction: LineAccumulator,
    total: LineAccumulator,
}

impl LinesAccumulator {
    fn new() -> Self {
        Self {
            main: LineAccumulator::new(),
            judge: LineAccumulator::new(),
            compaction: LineAccumulator::new(),
            total: LineAccumulator::new(),
        }
    }

    fn add(&mut self, role: GenerationRole, charge: &Charge) {
        match role {
            GenerationRole::Main => self.main.add(charge),
            GenerationRole::Judge => self.judge.add(charge),
            GenerationRole::Compaction => self.compaction.add(charge),
        }
        self.total.add(charge);
    }

    fn finish(self) -> UsageLines {
        UsageLines {
            main: self.main.finish(),
            judge: self.judge.finish(),
            compaction: self.compaction.finish(),
            total: self.total.finish(),
        }
    }
}

struct LineAccumulator {
    usage: Usage,
    known: BTreeMap<CurrencyCode, Money>,
    overflowed: BTreeSet<CurrencyCode>,
    unknown: Vec<UnknownCost>,
}

impl LineAccumulator {
    fn new() -> Self {
        Self {
            usage: Usage::IDENTITY,
            known: BTreeMap::new(),
            overflowed: BTreeSet::new(),
            unknown: Vec::new(),
        }
    }

    fn add(&mut self, charge: &Charge) {
        self.usage.add(&charge.usage);
        match &charge.cost {
            CostDelta::Unknown(reason) => self.unknown.push(reason.clone()),
            CostDelta::Known { amount, .. } if !self.overflowed.contains(&amount.currency) => {
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
            CostDelta::Known { .. } => {}
        }
    }

    fn finish(self) -> UsageLine {
        UsageLine {
            usage: self.usage,
            cost: CostBreakdown {
                known: self.known.into_values().collect(),
                unknown: self.unknown,
            },
        }
    }
}
