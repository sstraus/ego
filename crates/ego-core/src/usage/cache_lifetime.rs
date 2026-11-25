//! Passive, session-local cache lifetime evidence reduced from the journal.

use serde::{Deserialize, Serialize};

use crate::event::id::AttemptId;
use crate::event::time::Timestamp;
use crate::generator::attempt::{AttemptRef, Outcome, UsageReport};
use crate::generator::types::StopReason;

use super::{PromptCacheDeadline, PromptCacheRequest};

/// Actual dispatch selected by the adapter, never inferred from logical input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTransport {
    HttpFull,
    WebSocketFresh,
    WebSocketReusedFull,
    /// Connection-local continuation has no full-prefix retention proof.
    WebSocketContinuation,
}

impl CacheTransport {
    fn proves_full_input(self) -> bool {
        !matches!(self, Self::WebSocketContinuation)
    }
}

/// Opaque identity of an actual encoded request. No prompt or credential bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRequestIdentity {
    pub scope: String,
    pub frame: String,
    pub prefix: String,
    pub previous_prefix: Option<String>,
    pub input_items: usize,
}

impl CacheRequestIdentity {
    fn valid(&self) -> bool {
        let digest = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
        self.input_items > 0
            && digest(&self.scope)
            && digest(&self.frame)
            && digest(&self.prefix)
            && self.previous_prefix.as_deref().is_none_or(digest)
    }

    fn follows(&self, previous: &Self) -> bool {
        self.scope == previous.scope
            && self.frame == previous.frame
            && self.input_items >= previous.input_items
            && self.previous_prefix.as_ref() == Some(&previous.prefix)
    }
}

/// Three independent loss intervals, not a provider promise of retention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheLifetimeEstimate {
    pub samples: u8,
    pub min_seconds: u64,
    pub max_seconds: u64,
    /// Largest confirmed idle hit gap in this comparable cohort.
    pub confirmed_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Completed {
    model: String,
    identity: CacheRequestIdentity,
    at: Timestamp,
    started_at: Timestamp,
    hit: bool,
    input_tokens: u64,
    transport: CacheTransport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Pending {
    attempt: AttemptId,
    model: String,
    identity: CacheRequestIdentity,
    at: Timestamp,
}

/// Constant-size evidence for one durable session. Other sessions start empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheLifetimeLearning {
    last: Option<Completed>,
    pending: Option<Pending>,
    losses: Vec<u64>,
    longest_hit_seconds: u64,
}

impl CacheLifetimeLearning {
    pub fn is_empty(&self) -> bool {
        self.last.is_none()
            && self.pending.is_none()
            && self.losses.is_empty()
            && self.longest_hit_seconds == 0
    }

    pub(crate) fn tracks(&self, attempt: AttemptRef) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.attempt == attempt.attempt())
    }

    pub fn previous_input_items(&self) -> Option<usize> {
        self.last.as_ref().map(|last| last.identity.input_items)
    }

    pub fn estimate(&self) -> Option<CacheLifetimeEstimate> {
        if self.losses.len() != 3 {
            return None;
        }
        let min_seconds = *self.losses.iter().min()?;
        let max_seconds = *self.losses.iter().max()?;
        let confirmed_seconds = self.longest_hit_seconds;
        (min_seconds > 0
            && max_seconds <= min_seconds.saturating_add(min_seconds / 10)
            && confirmed_seconds > 0
            && confirmed_seconds < min_seconds
            && confirmed_seconds.saturating_add(confirmed_seconds / 10) >= min_seconds)
            .then_some(CacheLifetimeEstimate {
                samples: 3,
                min_seconds,
                max_seconds,
                confirmed_seconds,
            })
    }

    pub(crate) fn start(
        &mut self,
        request: Option<&PromptCacheRequest>,
        attempt: AttemptRef,
        at: Timestamp,
    ) -> Option<PromptCacheDeadline> {
        let Some(request) = request else {
            *self = Self::default();
            return None;
        };
        // A declared request TTL is authoritative, including a configured estimate.
        // Never carry an empirical cohort across a policy change.
        if request.policy.ttl_seconds.is_some() {
            *self = Self::default();
            return None;
        }
        let Some(identity) = request
            .identity
            .as_ref()
            .filter(|identity| identity.valid())
        else {
            *self = Self::default();
            return None;
        };
        if self.pending.is_some()
            || self.last.as_ref().is_some_and(|last| {
                last.model != request.model || !identity.follows(&last.identity) || at < last.at
            })
        {
            *self = Self::default();
        }
        let deadline = self.deadline();
        self.pending = Some(Pending {
            attempt: attempt.attempt(),
            model: request.model.clone(),
            identity: identity.clone(),
            at,
        });
        deadline
    }

    pub(crate) fn settle(
        &mut self,
        attempt: AttemptRef,
        outcome: &Outcome,
        at: Timestamp,
        accounting_agrees: bool,
    ) -> Option<PromptCacheDeadline> {
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.attempt != attempt.attempt())
        {
            return self.deadline();
        }
        let pending = self
            .pending
            .take()
            .expect("matching pending cache observation");
        let usage = match outcome.usage() {
            UsageReport::Reported(usage)
                if accounting_agrees
                    && matches!(
                        outcome.stop(),
                        StopReason::Stop
                            | StopReason::Length
                            | StopReason::ToolUse
                            | StopReason::Continue
                    )
                    && at >= pending.at
                    && [
                        usage.uncached_input_tokens,
                        usage.cache_read_tokens,
                        usage.cache_write_tokens,
                        usage.cache_write_extended_tokens,
                    ]
                    .into_iter()
                    .any(|tokens| tokens > 0) =>
            {
                usage
            }
            _ => {
                // Without a last identity, later requests cannot prove the cohort scope.
                *self = Self::default();
                return None;
            }
        };
        let Some(transport) = outcome
            .cache_transport()
            .filter(|mode| mode.proves_full_input())
        else {
            *self = Self::default();
            return None;
        };
        if self
            .last
            .as_ref()
            .is_some_and(|last| last.transport != transport)
        {
            *self = Self::default();
        }
        let Some(input_tokens) = usage
            .uncached_input_tokens
            .checked_add(usage.cache_read_tokens)
            .and_then(|total| total.checked_add(usage.cache_write_tokens))
            .and_then(|total| total.checked_add(usage.cache_write_extended_tokens))
        else {
            *self = Self::default();
            return None;
        };
        // A shared tools/instructions head is not proof that the preceding
        // conversation input survived. No guessed provider block tolerance.
        let hit = usage.cache_read_tokens > 0
            && self
                .last
                .as_ref()
                .is_some_and(|last| usage.cache_read_tokens >= last.input_tokens);
        if usage.cache_read_tokens > 0 && self.last.is_some() && !hit {
            self.losses.clear();
            self.longest_hit_seconds = 0;
        }
        let gap = self
            .last
            .as_ref()
            .filter(|last| last.hit && pending.identity.input_items > last.identity.input_items)
            .and_then(|last| {
                pending
                    .at
                    .as_millis()
                    .checked_sub(last.at.as_millis())
                    .filter(|millis| {
                        last.at.as_millis() - last.started_at.as_millis() <= millis / 10
                            && at.as_millis() - pending.at.as_millis() <= millis / 10
                    })
                    .map(|millis| millis / 1_000)
            });
        if let Some(gap) = gap {
            if hit {
                // A confirmed hit beyond an observed loss contradicts that cohort.
                if self.losses.iter().min().is_some_and(|loss| gap >= *loss) {
                    self.losses.clear();
                }
                self.longest_hit_seconds = self.longest_hit_seconds.max(gap);
            } else if usage.cache_read_tokens == 0 && gap > 0 {
                self.losses.push(gap);
                let min = *self.losses.iter().min().expect("one loss");
                let max = *self.losses.iter().max().expect("one loss");
                if max > min.saturating_add(min / 10) || self.longest_hit_seconds >= min {
                    self.losses.clear();
                    self.longest_hit_seconds = 0;
                    self.losses.push(gap);
                } else if self.losses.len() > 3 {
                    self.losses.remove(0);
                }
            }
        }
        self.last = Some(Completed {
            model: pending.model,
            identity: pending.identity,
            at,
            started_at: pending.at,
            hit,
            input_tokens,
            transport,
        });
        self.deadline()
    }

    fn deadline(&self) -> Option<PromptCacheDeadline> {
        let last = self.last.as_ref().filter(|last| last.hit)?;
        let estimate = self.estimate()?;
        let ttl_seconds = std::num::NonZeroU64::new(estimate.confirmed_seconds)?;
        let expires_at = Timestamp::from_millis(
            last.started_at
                .as_millis()
                .checked_add(ttl_seconds.get().checked_mul(1_000)?)?,
        );
        Some(PromptCacheDeadline {
            model: last.model.clone(),
            ttl_seconds,
            expires_at,
            estimate: Some(estimate),
        })
    }
}
