//! `ego acp`: production composition for the inbound ACP adapter.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::{ConnectTo, Stdio};
use ego_acp::serve::{AcpRunBindingsBuilder, ExecutionSpawner, RootResolveError, RootResolver};
use ego_core::run::{IdSource, RunController, RunRetention, RunRoot, RunTermination};
use ego_tools::clock::SystemClock;

use crate::cli::AcpArgs;
use crate::{run, session};

struct CanonicalRoots {
    base: PathBuf,
}

impl RootResolver for CanonicalRoots {
    fn resolve(&self, cwd: &Path) -> Result<RunRoot, RootResolveError> {
        let candidate = if cwd.is_absolute() {
            cwd.to_path_buf()
        } else {
            self.base.join(cwd)
        };
        let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
            RootResolveError::new(format!(
                "the ACP working directory {} could not be resolved: {error}",
                candidate.display()
            ))
        })?;
        RunRoot::new(canonical).map_err(|error| RootResolveError::new(error.to_string()))
    }
}

#[derive(Debug, Default)]
struct TokioExecutionSpawner;

impl ExecutionSpawner for TokioExecutionSpawner {
    fn spawn(
        &self,
        execution: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) {
        tokio::spawn(execution);
    }
}

#[derive(Debug, Default)]
struct UuidIds;

impl IdSource for UuidIds {
    fn next_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::new_v4()
    }
}

/// Serve the protocol on stdio until the host closes it.
pub async fn serve(args: &AcpArgs) -> RunTermination {
    let root = match run::workspace(args.directory.as_ref()) {
        Ok(root) => root,
        Err(error) => {
            return RunTermination::UsageError {
                message: error.to_string(),
            };
        }
    };
    let store_root = match session::store_root(args.session_dir.as_ref()) {
        Ok(store_root) => run::rooted_path(&root, store_root),
        Err(error) => {
            return RunTermination::UsageError {
                message: error.to_string(),
            };
        }
    };
    let store = Arc::new(session::jsonl(store_root, root.clone()));
    let executor = Arc::new(run::CliRunExecutor::for_acp(
        args.session_dir.clone(),
        Arc::clone(&store),
    ));
    let ids = Arc::new(UuidIds);
    let controller = Arc::new(RunController::new(
        store,
        executor,
        Arc::new(SystemClock),
        Arc::clone(&ids),
        RunRetention::new(NonZeroUsize::new(64).expect("non-zero retention"))
            .expect("valid retention"),
    ));

    let agent = ego_acp::serve::agent(
        controller,
        Arc::new(CanonicalRoots { base: root }),
        Arc::new(TokioExecutionSpawner),
        ids,
        Arc::new(AcpRunBindingsBuilder),
    );
    match agent.connect_to(Stdio::new()).await {
        Ok(()) => RunTermination::Done,
        Err(error) => RunTermination::Failed {
            message: format!("the ACP connection ended badly: {error}"),
        },
    }
}
