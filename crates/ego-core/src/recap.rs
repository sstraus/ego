//! When an interactive session shows a recap after the user was away.
//!
//! Off by default: every optional feature is opt-in. When
//! on, the first prompt after `after_minutes` without a completed turn gets a
//! short recap before it runs. The recap is a model call; this module only
//! decides when one is due.

use std::num::NonZeroU64;

use serde::Deserialize;

use crate::event::time::Timestamp;
use crate::state::SessionState;

/// One layer's `[recap]` table. Absent fields keep lower authority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecapLayer {
    pub enabled: Option<bool>,
    pub after_minutes: Option<NonZeroU64>,
}

/// Resolved recap policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecapConfig {
    /// Show a recap after an absence. Off by default.
    pub enabled: bool,
    /// Minutes since the last completed turn after which a recap is due.
    pub after_minutes: NonZeroU64,
}

impl Default for RecapConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            after_minutes: NonZeroU64::new(30).expect("non-zero default"),
        }
    }
}

impl RecapConfig {
    /// Apply one higher layer field by field.
    pub fn merge(&mut self, layer: &RecapLayer) {
        if let Some(enabled) = layer.enabled {
            self.enabled = enabled;
        }
        if let Some(minutes) = layer.after_minutes {
            self.after_minutes = minutes;
        }
    }

    /// Whether the next prompt of `state` at `now` gets a recap first.
    ///
    /// A session with no completed turn has nothing to recap. The absence is
    /// measured from the last completed turn, which survives a restart, so a
    /// session resumed the next day is due.
    pub fn due(&self, state: &SessionState, now: Timestamp) -> bool {
        let Some(last) = state.last_turn_completed_at else {
            return false;
        };
        self.enabled
            && now.as_millis().saturating_sub(last.as_millis())
                >= self.after_minutes.get().saturating_mul(60_000)
    }
}
