//! Run-owned guarded duplex process adapter.

use std::sync::Arc;

use ego_core::permit::{DuplexHandle, DuplexSpawner, SpawnSpec, TransportError};
use futures::future::BoxFuture;

use crate::exec::process::RunProcessContext;

/// The only duplex spawner wired into a run's effect ports.
pub(crate) struct GuardedDuplex {
    context: Arc<RunProcessContext>,
}

impl GuardedDuplex {
    pub(crate) fn new(context: Arc<RunProcessContext>) -> Self {
        Self { context }
    }
}

impl DuplexSpawner for GuardedDuplex {
    fn spawn(&self, spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        let context = Arc::clone(&self.context);
        Box::pin(async move { context.spawn_duplex(spec).await })
    }
}
