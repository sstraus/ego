use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ego_acp::serve::{AcpRunBindingsBuilder, ExecutionSpawner, RootResolver};
use ego_core::config::ModelRef;
use ego_core::event::domain::DomainEvent;
use ego_core::event::id::SessionId;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::run::{
    IdSource, RunAppend, RunController, RunExecutionBindings, RunExecutor, RunExecutorError,
    RunHandle, RunPreparation, RunPrepared, RunRetention, RunRevision, RunRoot, RunSettlement,
    RunStart, RunTransaction,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use futures::future::{self, BoxFuture};
use futures::stream::BoxStream;

#[derive(Default)]
struct Store {
    entries: Mutex<BTreeMap<SessionId, Vec<SessionEntry>>>,
}

impl SessionStore for Store {
    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.entries
            .lock()
            .expect("support entries")
            .entry(*id)
            .or_default()
            .push(SessionEntry::Event(event.clone()));
        Ok(())
    }

    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let entries = self
            .entries
            .lock()
            .expect("support entries")
            .get(id)
            .cloned()
            .ok_or(StoreError::NotFound)?;
        Ok(Box::pin(futures::stream::iter(
            entries.into_iter().map(Ok::<_, StoreError>),
        )))
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(self
            .entries
            .lock()
            .expect("support entries")
            .keys()
            .map(|id| SessionMeta {
                id: *id,
                workspace_root: PathBuf::from("/tmp"),
                model: None,
                created: Timestamp::EPOCH,
                updated: Timestamp::EPOCH,
                title: None,
                lineage: None,
            })
            .collect())
    }

    fn compare_and_append_run(
        &self,
        session: &SessionId,
        expected_revision: Option<RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        let mut entries = self.entries.lock().expect("support entries");
        let actual = entries.get(session).and_then(|entries| {
            entries.iter().rev().find_map(|entry| match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
                SessionEntry::Event(_) => None,
            })
        });
        if expected_revision != actual {
            return match actual {
                Some(actual) => Ok(RunAppend::Conflict { actual }),
                None => Err(StoreError::NotFound),
            };
        }
        let revision = transaction.revision();
        entries
            .entry(*session)
            .or_default()
            .push(SessionEntry::RunTransaction(transaction));
        Ok(RunAppend::Committed { revision })
    }
}

// Shared by integration crates; some crates exercise only the alternate injected adapters.
#[allow(dead_code)]
struct Executor;

#[allow(dead_code)]
struct Prepared;

impl RunExecutor for Executor {
    type Prepared = Prepared;

    fn prepare<'a>(
        &'a self,
        _request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        Box::pin(future::ready(Ok(RunPrepared::new(
            ModelRef("model/test".to_owned()),
            Prepared,
        ))))
    }

    fn start<'a>(
        &'a self,
        request: &'a RunStart,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        Box::pin(future::ready(Ok(RunHandle::new(
            request.run_id,
            std::sync::Arc::new(()),
        ))))
    }

    fn recover<'a>(
        &'a self,
        _run: &'a ego_core::run::RunId,
    ) -> BoxFuture<'a, Result<ego_core::run::RecoveryEvidence, RunExecutorError>> {
        Box::pin(future::ready(Ok(
            ego_core::run::RecoveryEvidence::Ambiguous,
        )))
    }

    fn observe<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(future::ready(Ok(RunSettlement::completed())))
    }

    fn submit<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _prompt: &'a str,
        _prepared: Self::Prepared,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(future::ready(Ok(RunSettlement::completed())))
    }

    fn interrupt<'a>(
        &'a self,
        _handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(future::ready(Ok(())))
    }
}

#[allow(dead_code)]
#[derive(Default)]
struct TestClock;

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::EPOCH
    }
}

#[allow(dead_code)]
#[derive(Default)]
struct TestIds {
    next: Mutex<u128>,
}

impl IdSource for TestIds {
    fn next_uuid(&self) -> uuid::Uuid {
        let mut next = self.next.lock().expect("support ids");
        *next += 1;
        uuid::Uuid::from_u128(*next)
    }
}

#[derive(Default)]
struct TestRoots;

impl RootResolver for TestRoots {
    fn resolve(&self, cwd: &Path) -> Result<RunRoot, ego_acp::serve::RootResolveError> {
        RunRoot::new(cwd.to_path_buf())
            .map_err(|error| ego_acp::serve::RootResolveError::new(error.to_string()))
    }
}

#[derive(Default)]
struct TestSpawner;

impl ExecutionSpawner for TestSpawner {
    fn spawn(&self, execution: BoxFuture<'static, ()>) {
        tokio::spawn(execution);
    }
}

#[allow(dead_code)]
pub fn agent() -> agent_client_protocol::AgentProtocolRouter {
    let ids = Arc::new(TestIds::default());
    agent_with(
        Arc::new(Store::default()),
        Arc::new(Executor),
        Arc::new(TestClock),
        ids,
    )
}

pub fn agent_with<S, E, C, I>(
    store: Arc<S>,
    executor: Arc<E>,
    clock: Arc<C>,
    ids: Arc<I>,
) -> agent_client_protocol::AgentProtocolRouter
where
    S: SessionStore + 'static,
    E: RunExecutor + 'static,
    C: Clock + 'static,
    I: IdSource + 'static,
{
    let controller = Arc::new(RunController::new(
        store,
        executor,
        clock,
        Arc::clone(&ids),
        RunRetention::new(std::num::NonZeroUsize::new(2).expect("retention")).expect("retention"),
    ));
    ego_acp::serve::agent(
        controller,
        Arc::new(TestRoots),
        Arc::new(TestSpawner),
        ids,
        Arc::new(AcpRunBindingsBuilder),
    )
}
