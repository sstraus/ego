//! RED contract for M4 S2: isolated child runs over the rooted controller.
//!
//! This suite builds the ordinary `RunController` with test-owned adapters
//! and injects only the narrow durable parent/child journal port. It must not
//! pass through a fixture service, crash counters, an in-memory event
//! projection, or an externally supplied child result.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ego_core::budget::BudgetGate;
use ego_core::ceiling::Budget;
use ego_core::config::{resolve, ConfigLayer, ModelRef, Source, Trust};
use ego_core::event::domain::{DomainEvent, SubagentTerminal};
use ego_core::event::hook::HookInvocationId;
use ego_core::event::id::{ItemId, SessionId, SubagentInvocationId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{EvidenceError, EvidenceWrite};
use ego_core::run::{
    EventSinkFactory, IdSource, RecoveryEvidence, RunAppend, RunController,
    RunExecutionBindings, RunExecutor, RunExecutorError, RunHandle, RunId, RunOrigin,
    RunPreparation, RunPrepared, RunRetention, RunRoot, RunSelections, RunSettlement, RunStart,
    RunTransaction,
};
use ego_core::sink::{EventSink, NullSink};
use ego_core::store::blob::{BlobId, BlobRef};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::subagent::{
    ChildAuthority, ChildJournal, ChildJournalError, ChildJournalRecord, ChildRequest,
    ChildRunService, ChildWait, ChildWaitResult, SandboxLevel,
};
use futures::future::{self, BoxFuture};
use futures::stream::{self, BoxStream};

const TASK: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const RESULT: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn blob(hex: &str, bytes: u64) -> BlobRef {
    BlobRef::new(BlobId::parse(hex).expect("test blob id"), bytes)
}

fn authority() -> ChildAuthority {
    ChildAuthority::new(
        ["read", "exec", "mcp:trusted"],
        ["allow(read(src/**))"],
        SandboxLevel::Workspace,
        true,
        ["trusted"],
    )
}

fn selections() -> RunSelections {
    RunSelections::new(
        Some(ModelRef("provider/model".to_owned())),
        Some(ego_core::generator::types::ThinkingLevel::Low),
        Some(ego_core::config::Mode::Default),
        Some("40k".parse::<Budget>().expect("test budget")),
        None,
    )
}

fn profile_name_from_owner() -> String {
    let layer = ConfigLayer::parse(Source::User, "[profile.worker]\n").expect("profile layer");
    resolve(std::slice::from_ref(&layer), Trust::Trusted, Some("worker"))
        .expect("profile owner resolves the selected profile")
        .active_profile
        .expect("resolved profile provenance")
        .name
}

fn request(invocation: SubagentInvocationId, wait: ChildWait) -> ChildRequest {
    ChildRequest::new(
        invocation,
        RunId::new(SessionId::new(), uuid::Uuid::now_v7()),
        TurnId::new(),
        ItemId::new(),
        blob(TASK, 64),
        selections(),
        wait,
    )
    .with_requested_tools(["read"])
    .with_requested_permission("allow(read(src/**))")
    .with_requested_sandbox("workspace")
    .with_requested_profile(profile_name_from_owner())
}

#[derive(Default, Clone)]
struct DurableJournal {
    records: Arc<Mutex<Vec<ChildJournalRecord>>>,
}

impl DurableJournal {
    fn records(&self) -> Vec<ChildJournalRecord> {
        self.records.lock().expect("journal records").clone()
    }

    fn truncate_at(&self, cut: ego_core::subagent::RecoveryCut) {
        self.records
            .lock()
            .expect("journal records")
            .retain(|record| record.is_present_at(cut));
    }
}

impl ChildJournal for DurableJournal {
    fn append(&self, record: ChildJournalRecord) -> Result<(), ChildJournalError> {
        self.records.lock().expect("journal records").push(record);
        Ok(())
    }

    fn read(
        &self,
        invocation: SubagentInvocationId,
    ) -> Result<Vec<ChildJournalRecord>, ChildJournalError> {
        Ok(self
            .records()
            .into_iter()
            .filter(|record| record.invocation() == invocation)
            .collect())
    }
}

#[derive(Default)]
struct Store {
    entries: Mutex<BTreeMap<SessionId, Vec<SessionEntry>>>,
}

impl Store {
    fn all_transactions(&self) -> Vec<RunTransaction> {
        self.entries
            .lock()
            .expect("store entries")
            .values()
            .flat_map(|entries| entries.iter())
            .filter_map(|entry| match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction.clone()),
                SessionEntry::Event(_) | SessionEntry::Evidence(_) => None,
            })
            .collect()
    }
}

impl SessionStore for Store {
    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.entries
            .lock()
            .expect("store entries")
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
            .expect("store entries")
            .get(id)
            .cloned()
            .ok_or(StoreError::NotFound)?;
        Ok(Box::pin(stream::iter(entries.into_iter().map(Ok))))
    }

    fn compare_and_append_run(
        &self,
        session: &SessionId,
        expected_revision: Option<ego_core::run::RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        let mut entries = self.entries.lock().expect("store entries");
        let current = entries.get(session).and_then(|values| {
            values.iter().rev().find_map(|entry| match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
                SessionEntry::Event(_) | SessionEntry::Evidence(_) => None,
            })
        });
        if current != expected_revision {
            return Ok(RunAppend::Conflict {
                actual: current.unwrap_or_else(|| ego_core::run::RunRevision::new(0)),
            });
        }
        let revision = transaction.revision();
        entries
            .entry(*session)
            .or_default()
            .push(SessionEntry::RunTransaction(transaction));
        Ok(RunAppend::Committed { revision })
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::EPOCH
    }
}

#[derive(Default)]
struct FixedIds;

impl IdSource for FixedIds {
    fn next_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::now_v7()
    }
}

#[derive(Default)]
struct Sinks;

impl EventSinkFactory for Sinks {
    fn create(
        &self,
        _capabilities: &ego_core::generator::capabilities::ModelCapabilities,
    ) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

struct Executor {
    prompts: Mutex<Vec<String>>,
    settlement: Mutex<RunSettlement>,
}

impl Executor {
    fn new(settlement: RunSettlement) -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
            settlement: Mutex::new(settlement),
        }
    }
}

impl RunExecutor for Executor {
    type Prepared = ();

    fn prepare<'a>(
        &'a self,
        _request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        Box::pin(future::ready(Ok(RunPrepared::new(
            ModelRef("provider/model".to_owned()),
            (),
        ))))
    }

    fn admit_run_context<'a>(
        &'a self,
        _prepared: &'a Self::Prepared,
    ) -> ego_core::run::RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
        Box::pin(future::ready(Ok(EvidenceWrite::Published)))
    }

    fn start<'a>(
        &'a self,
        request: &'a RunStart,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        Box::pin(future::ready(Ok(RunHandle::new(request.run_id, ()))))
    }

    fn recover<'a>(
        &'a self,
        _run: &'a RunId,
    ) -> BoxFuture<'a, Result<RecoveryEvidence, RunExecutorError>> {
        Box::pin(future::ready(Ok(RecoveryEvidence::Ambiguous)))
    }

    fn observe<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(future::ready(Ok(self.settlement.lock().expect("settlement").clone())))
    }

    fn submit<'a>(
        &'a self,
        _handle: &'a RunHandle,
        prompt: &'a str,
        _prepared: Self::Prepared,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        self.prompts
            .lock()
            .expect("prompts")
            .push(prompt.to_owned());
        Box::pin(future::ready(Ok(self.settlement.lock().expect("settlement").clone())))
    }

    fn interrupt<'a>(
        &'a self,
        _handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(future::ready(Ok(())))
    }
}

type Controller = RunController<Store, Executor, FixedClock, FixedIds>;

struct Harness {
    store: Arc<Store>,
    executor: Arc<Executor>,
    journal: DurableJournal,
    controller: Arc<Controller>,
}

impl Harness {
    fn new(settlement: RunSettlement) -> Self {
        let store = Arc::new(Store::default());
        let executor = Arc::new(Executor::new(settlement));
        let controller = Arc::new(RunController::new(
            Arc::clone(&store),
            Arc::clone(&executor),
            Arc::new(FixedClock),
            Arc::new(FixedIds),
            RunRetention::new(std::num::NonZeroUsize::new(8).expect("retention")).expect("retention"),
        ));
        Self { store, executor, journal: DurableJournal::default(), controller }
    }

    fn service(&self, gate: BudgetGate) -> ChildRunService {
        ChildRunService::new(
            Arc::clone(&self.controller),
            Arc::new(Mutex::new(gate)),
            authority(),
            authority(),
            RunRoot::new("/workspace").expect("absolute root"),
            RunExecutionBindings::new(Arc::new(Sinks), None),
            Arc::new(self.journal.clone()),
        )
    }
}

#[test]
fn child_uses_real_controller_and_durable_typed_parent_order() {
    let harness = Harness::new(RunSettlement::Completed);
    let service = harness.service(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let invocation = SubagentInvocationId::new();
    let admission = futures::executor::block_on(service.start(request(invocation, ChildWait::Foreground)))
        .expect("child admission");
    assert!(matches!(admission.origin(), RunOrigin::Subagent { invocation: id, .. } if *id == invocation));
    assert_ne!(admission.child_run(), admission.parent_run());
    assert_ne!(admission.child_session(), admission.parent_run().session_id());
    let result = futures::executor::block_on(admission.wait()).expect("child settlement");
    assert_eq!(result, ChildWaitResult::Returned { final_message: blob(RESULT, 64) });
    assert_eq!(harness.executor.prompts.lock().expect("prompts").as_slice(), &[TASK.to_owned()]);
    assert_eq!(
        harness.journal.records().iter().map(ChildJournalRecord::kind).collect::<Vec<_>>(),
        ["subagent_requested", "subagent_spawned", "subagent_settled"],
    );
    assert!(harness.store.all_transactions().iter().any(|transaction| {
        matches!(transaction.origin(), RunOrigin::Subagent { invocation: id, .. } if *id == invocation)
    }));
}

#[test]
fn recovery_reopens_durable_facts_at_all_six_cuts_without_retrying_uncertain_delivery() {
    for cut in [
        ego_core::subagent::RecoveryCut::BeforeRequest,
        ego_core::subagent::RecoveryCut::AfterRequest,
        ego_core::subagent::RecoveryCut::AfterChildAdmission,
        ego_core::subagent::RecoveryCut::AfterSpawn,
        ego_core::subagent::RecoveryCut::AfterChildSettlement,
        ego_core::subagent::RecoveryCut::BeforeParentSettlement,
    ] {
        let harness = Harness::new(RunSettlement::Completed);
        let invocation = SubagentInvocationId::new();
        let service = harness.service(BudgetGate::unlimited(
            ego_core::cost::CurrencyCode::new("USD").expect("currency"),
        ));
        futures::executor::block_on(service.start(request(invocation, ChildWait::Background)))
            .expect("initial child");
        harness.journal.truncate_at(cut);
        let reopened = harness.service(BudgetGate::unlimited(
            ego_core::cost::CurrencyCode::new("USD").expect("currency"),
        ));
        let recovered = futures::executor::block_on(reopened.recover_from_durable(invocation, cut))
            .expect("durable recovery");
        assert_eq!(recovered.invocation(), invocation);
        assert_eq!(recovered.admission_count(), 1, "cut {cut:?}");
        assert!(!recovered.delivery_was_retried(), "cut {cut:?}");
    }
}

#[test]
fn child_execution_carries_prompt_and_preserves_each_typed_terminal() {
    for settlement in [
        RunSettlement::Cancelled { reason: ego_core::run::CancelReason::User },
        RunSettlement::Interrupted,
        RunSettlement::Uncertain,
    ] {
        let harness = Harness::new(settlement.clone());
        let service = harness.service(BudgetGate::unlimited(
            ego_core::cost::CurrencyCode::new("USD").expect("currency"),
        ));
        let admission = futures::executor::block_on(service.start(request(
            SubagentInvocationId::new(), ChildWait::Foreground,
        )))
        .expect("child admission");
        let result = futures::executor::block_on(admission.wait()).expect("typed settlement");
        assert!(matches!(result, ChildWaitResult::Failed { settlement: actual } if actual == settlement));
    }
    let harness = Harness::new(RunSettlement::Completed);
    let service = harness.service(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let blocked = futures::executor::block_on(service.run_return_hook(
        request(SubagentInvocationId::new(), ChildWait::Foreground),
        blob(RESULT, 64),
        HookInvocationId::new(),
    ))
    .expect("blocked return boundary");
    assert!(matches!(blocked, SubagentTerminal::ReturnBlocked { .. }));
}

#[test]
fn foreground_cancel_and_background_drop_preserve_real_execution_ownership() {
    let harness = Harness::new(RunSettlement::Cancelled { reason: ego_core::run::CancelReason::User });
    let service = harness.service(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let foreground = futures::executor::block_on(service.start(request(
        SubagentInvocationId::new(), ChildWait::Foreground,
    )))
    .expect("foreground");
    futures::executor::block_on(foreground.cancel()).expect("foreground cancellation");
    assert!(matches!(
        futures::executor::block_on(foreground.wait()).expect("cancelled result"),
        ChildWaitResult::Failed { settlement: RunSettlement::Cancelled { .. } }
    ));
    let background = futures::executor::block_on(service.start(request(
        SubagentInvocationId::new(), ChildWait::Background,
    )))
    .expect("background");
    let invocation = background.invocation();
    drop(background.subscription());
    let collected = futures::executor::block_on(service.collect_background(invocation))
        .expect("background collection");
    assert!(matches!(collected, ChildWaitResult::Failed { .. }));
}

#[test]
fn authority_profile_and_shared_budget_are_admitted_by_real_owners() {
    let harness = Harness::new(RunSettlement::Completed);
    let gate = BudgetGate::new(ego_core::cost::Money::new(
        "1".parse().expect("amount"),
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let service = harness.service(gate);
    let first = futures::executor::block_on(service.start(request(
        SubagentInvocationId::new(), ChildWait::Background,
    )))
    .expect("first child uses shared gate");
    let second = futures::executor::block_on(service.start(request(
        SubagentInvocationId::new(), ChildWait::Background,
    )));
    assert!(matches!(second, Err(ego_core::subagent::ChildServiceError::BudgetDenied { .. })));
    assert_eq!(first.selections().sandbox(), SandboxLevel::Workspace);
    assert!(first.selections().tools().is_subset_of(&authority().tools()));
    assert!(first.selections().mcp().is_subset_of(&authority().mcp()));
    assert!(first.selections().permissions().is_subset_of(&authority().permissions()));
    assert!(first.selections().offline());
}

#[test]
fn concurrent_children_have_distinct_leases_and_no_parent_transcript() {
    let harness = Harness::new(RunSettlement::Completed);
    let service = harness.service(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let first = futures::executor::block_on(service.start(request(
        SubagentInvocationId::new(), ChildWait::Background,
    )))
    .expect("first child");
    let second = futures::executor::block_on(service.start(request(
        SubagentInvocationId::new(), ChildWait::Background,
    )))
    .expect("second child");
    assert_ne!(first.child_run(), second.child_run());
    assert_ne!(first.turn_lease(), second.turn_lease());
    assert_ne!(first.child_session(), second.child_session());
    assert_eq!(first.budget_gate(), second.budget_gate());
    assert!(futures::executor::block_on(service.start(
        request(SubagentInvocationId::new(), ChildWait::Background)
            .with_parent_transcript("parent transcript"),
    ))
    .is_err());
}
