use crate::event::domain::DomainEvent;
use crate::store::session_store::SessionStore;

/// Publication outcome at the atomic target-log boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionPublication {
    NotPublished { diagnostic: String },
    PublishedDurably { diagnostic: Option<String> },
    PublishedDurabilityUncertain { diagnostic: String },
}

impl CompactionPublication {
    pub fn switch_target(&self) -> bool {
        !matches!(self, Self::NotPublished { .. })
    }

    pub fn retry_safe(&self) -> bool {
        matches!(self, Self::NotPublished { .. })
    }

    pub fn diagnostic(&self) -> Option<&str> {
        match self {
            Self::NotPublished { diagnostic }
            | Self::PublishedDurabilityUncertain { diagnostic } => Some(diagnostic),
            Self::PublishedDurably { diagnostic } => diagnostic.as_deref(),
        }
    }
}

/// Consumer-owned target publication boundary, implemented by a lease-held store.
pub trait CompactionTargetStore: SessionStore {
    fn publish_compaction_seed(&self, seed: &DomainEvent) -> CompactionPublication;
}
