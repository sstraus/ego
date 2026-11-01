//! Local request preparation: only the session owner can authorize durable state.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use futures::channel::oneshot;
use serde::{Deserialize, Serialize};

use crate::event::id::SessionId;

/// Private write-once variables for one configured filter in one session.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterVariables {
    pub revision: u64,
    pub values: BTreeMap<String, String>,
}

impl fmt::Debug for FilterVariables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilterVariables")
            .field("revision", &self.revision)
            .field("keys", &self.values.len())
            .finish_non_exhaustive()
    }
}

impl FilterVariables {
    /// Validate a complete proposal without changing the accepted state.
    pub fn proposed(&self, writes: &BTreeMap<String, String>) -> Result<Self, &'static str> {
        let mut next = self.clone();
        for (key, value) in writes {
            if key.is_empty()
                || key.len() > 128
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
                || value.len() > 4096
            {
                return Err("protocol_filter session variable exceeds limits");
            }
            if let Some(previous) = next.values.get(key) {
                if previous != value {
                    return Err("protocol_filter cannot replace a captured session variable");
                }
            } else {
                next.values.insert(key.clone(), value.clone());
            }
        }
        if next.values.len() > 64
            || serde_json::to_vec(&next.values)
                .map_err(|_| "protocol_filter state encoding failed")?
                .len()
                > 65_536
        {
            return Err("protocol_filter session state exceeds limits");
        }
        if next.values != self.values {
            next.revision = self
                .revision
                .checked_add(1)
                .ok_or("protocol_filter state revision exhausted")?;
        }
        Ok(next)
    }
}

#[derive(Debug)]
pub struct FilterStateSnapshot {
    pub session_id: SessionId,
    pub state: FilterVariables,
}

pub(crate) enum FilterStateOperation {
    Read,
    Save {
        expected_revision: u64,
        writes: BTreeMap<String, String>,
    },
}

type Reply = Result<FilterStateSnapshot, &'static str>;
type Pending = (String, FilterStateOperation, oneshot::Sender<Reply>);

/// An in-process acknowledgement, never a provider frame or a durable event.
/// Clones share one reply authority; values are omitted from diagnostics.
#[derive(Clone)]
pub struct FilterStateExchange(Arc<Mutex<Option<Pending>>>);

impl fmt::Debug for FilterStateExchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FilterStateExchange(..)")
    }
}

impl PartialEq for FilterStateExchange {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl FilterStateExchange {
    fn new(namespace: String, operation: FilterStateOperation) -> (Self, oneshot::Receiver<Reply>) {
        let (sender, receiver) = oneshot::channel();
        (
            Self(Arc::new(Mutex::new(Some((namespace, operation, sender))))),
            receiver,
        )
    }

    pub fn read(namespace: String) -> (Self, oneshot::Receiver<Reply>) {
        Self::new(namespace, FilterStateOperation::Read)
    }

    pub fn save(
        namespace: String,
        expected_revision: u64,
        writes: BTreeMap<String, String>,
    ) -> (Self, oneshot::Receiver<Reply>) {
        Self::new(
            namespace,
            FilterStateOperation::Save {
                expected_revision,
                writes,
            },
        )
    }

    pub(crate) fn take(&self) -> Option<Pending> {
        self.0.lock().ok()?.take()
    }
}
