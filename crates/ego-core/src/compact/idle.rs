//! Idle compaction before the prompt-cache deadline.
//!
//! Compaction reads the full context. While the provider cache is warm those
//! reads cost the cache-read price; after expiry the next turn writes the full
//! context again. This policy only decides *when* the existing idle compaction
//! operation runs; it never estimates a lifetime. Without a known deadline
//! (provider-managed TTL and no profile value) it never fires.

use std::num::NonZeroU64;
use std::time::Duration;

use serde::Deserialize;

use crate::event::time::Timestamp;
use crate::state::SessionState;

/// One layer's `[compaction]` table. Absent fields keep lower authority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionLayer {
    pub threshold: Option<ContextPercent>,
    pub idle: Option<bool>,
    pub headless: Option<bool>,
    pub idle_margin_seconds: Option<NonZeroU64>,
    pub idle_threshold: Option<ContextPercent>,
}

/// Resolved compaction policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    /// Measured context, in percent of the measured window, at which a turn
    /// yields and an interactive or headless host compacts. Default 90.
    pub threshold: ContextPercent,
    /// Compact an idle session before its prompt cache expires. Off by default:
    /// every optional feature is opt-in.
    pub idle: bool,
    /// Compact a headless `ego run` that reaches `threshold` and continue in
    /// the successor. Off by default: an optional feature is an option the
    /// user turns on. Off, such a run ends
    /// with `context_overflow`, exit 4.
    pub headless: bool,
    /// Seconds before the cache deadline at which the idle compaction starts.
    pub idle_margin_seconds: NonZeroU64,
    /// Minimum measured context, in percent of the measured window, for an
    /// idle compaction. A small context costs little to
    /// write again.
    pub idle_threshold: ContextPercent,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            threshold: ContextPercent(90),
            idle: false,
            headless: false,
            idle_margin_seconds: NonZeroU64::new(30).expect("non-zero default"),
            idle_threshold: ContextPercent(50),
        }
    }
}

impl CompactionConfig {
    /// Apply one higher layer field by field.
    pub fn merge(&mut self, layer: &CompactionLayer) {
        if let Some(threshold) = layer.threshold {
            self.threshold = threshold;
        }
        if let Some(idle) = layer.idle {
            self.idle = idle;
        }
        if let Some(headless) = layer.headless {
            self.headless = headless;
        }
        if let Some(margin) = layer.idle_margin_seconds {
            self.idle_margin_seconds = margin;
        }
        if let Some(threshold) = layer.idle_threshold {
            self.idle_threshold = threshold;
        }
    }

    /// Refuse an idle threshold above the turn threshold: idle compaction
    /// would then never run before pressure compaction.
    pub fn validate(&self) -> Result<(), String> {
        if self.idle_threshold > self.threshold {
            return Err(format!(
                "compaction.idle_threshold ({}) must not exceed compaction.threshold ({})",
                self.idle_threshold.get(),
                self.threshold.get()
            ));
        }
        Ok(())
    }
}

/// A percentage from 1 to 99 of the measured context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "u8")]
pub struct ContextPercent(u8);

impl ContextPercent {
    pub fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for ContextPercent {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if (1..=99).contains(&value) {
            Ok(Self(value))
        } else {
            Err(format!("a compaction percent must be 1-99, got {value}"))
        }
    }
}

/// What the idle input owner does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleCompaction {
    /// No idle compaction applies to the current state.
    Never,
    /// Wait for input at most this long, then decide again.
    After(Duration),
    /// Compact now. `expires_at` names the deadline this compaction serves.
    Now { expires_at: Timestamp },
}

impl CompactionConfig {
    /// Decide the idle compaction for `state` at `now`.
    ///
    /// `model` is the current selection: a deadline of another model cannot be
    /// read by the compaction request. `attempted` is the deadline of the last
    /// idle compaction attempt; one deadline gets at most one attempt.
    pub fn idle_compaction(
        &self,
        state: &SessionState,
        model: &str,
        attempted: Option<Timestamp>,
        now: Timestamp,
    ) -> IdleCompaction {
        if !self.idle
            || state.pending_question().is_some()
            || !state.unacknowledged_prompts().is_empty()
            || !state.live_attempts.is_empty()
            // The successor seed does not carry a detached exec or a background
            // agent task: its result would never reach the model.
            || state.detached_execs.values().any(Option::is_none)
            || state
                .background_tasks
                .keys()
                .any(|id| !state.delivered_subagents.contains_key(id))
            || !self.context_is_large(state)
        {
            return IdleCompaction::Never;
        }
        let Some(cache) = state.prompt_cache.as_ref() else {
            return IdleCompaction::Never;
        };
        if cache.model != model || attempted == Some(cache.expires_at) {
            return IdleCompaction::Never;
        }
        // A margin that covers the whole lifetime would compact after every turn.
        if self.idle_margin_seconds >= cache.ttl_seconds {
            return IdleCompaction::Never;
        }
        let expires = cache.expires_at.as_millis();
        if now.as_millis() >= expires {
            return IdleCompaction::Never;
        }
        let due = expires.saturating_sub(self.idle_margin_seconds.get().saturating_mul(1_000));
        match due.checked_sub(now.as_millis()) {
            Some(wait) if wait > 0 => IdleCompaction::After(Duration::from_millis(wait)),
            _ => IdleCompaction::Now {
                expires_at: cache.expires_at,
            },
        }
    }

    fn context_is_large(&self, state: &SessionState) -> bool {
        match (state.context.used_tokens, state.context.window) {
            (Some(used), Some(window)) if window > 0 => {
                u128::from(used) * 100 >= u128::from(window) * u128::from(self.idle_threshold.get())
            }
            _ => false,
        }
    }
}
