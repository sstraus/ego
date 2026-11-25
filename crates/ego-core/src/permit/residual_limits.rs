//! Finite owner-configured residual judge bounds. These never grant path authority.
use serde::Deserialize;
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResidualLimits {
    pub decisions_per_call: NonZeroUsize,
    pub decisions_per_session: NonZeroUsize,
    pub deadline_ms: NonZeroU32,
}
impl Default for ResidualLimits {
    fn default() -> Self {
        Self {
            decisions_per_call: NonZeroUsize::new(super::MAX_DECISIONS_PER_CALL).unwrap(),
            decisions_per_session: NonZeroUsize::new(super::MAX_DECISIONS_PER_SESSION).unwrap(),
            deadline_ms: NonZeroU32::new(
                (super::DECISION_DEADLINE.as_secs() * 1_000)
                    .try_into()
                    .expect("default deadline fits u32"),
            )
            .unwrap(),
        }
    }
}
impl ResidualLimits {
    pub fn deadline(self) -> Duration {
        Duration::from_millis(u64::from(self.deadline_ms.get()))
    }
}
