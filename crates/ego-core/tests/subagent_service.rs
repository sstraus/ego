//! RED contract for M4 S2: isolated child runs over the rooted controller.
//!
//! This suite builds the ordinary `RunController` with test-owned adapters
//! and injects only the narrow durable parent/child journal port. It must not
//! pass through a fixture service, crash counters, an in-memory event
//! projection, or an externally supplied child result.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use ego_core::budget::{BudgetGate, Metering, PricingQuote, Reservation, SettlementSource};
use ego_core::ceiling::Budget;
use ego_core::config::{
    ConfigLayer, Effective, ModelRef, Source, Trust, resolve, resolve_selected,
};
use ego_core::cost::{Amount, CurrencyCode, PricingRevision};
use ego_core::event::domain::{DomainEvent, Envelope, EventKind, SubagentTerminal};
use ego_core::event::hook::{
    HookActivation, HookDecision, HookFailure, HookInvocationId, HookLifecycle, HookOutcome,
    HookSubject,
};
use ego_core::event::id::{
    EventId, ItemId, Producer, Seq, SessionId, SubagentInvocationId, TurnId,
};
use ego_core::event::skill::SkillSourceAuthority;
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{EvidenceError, EvidenceWrite};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::types::{Conversation, GenRequest, Message, Role, ThinkingLevel};
use ego_core::hook::{
    HookCatalogue, HookConfigEntry, HookEvent, HookFuture, HookHandlerConfig, HookPort,
    HookRecordFuture, HookRunner, HookSource, HookStartError, HookStartFuture, HookStartedEvidence,
};
use ego_core::permit::{CanonPath, Confinement, RunConfinement};
use ego_core::run::{
    EventSinkFactory, IdSource, RecoveryEvidence, RunAppend, RunController, RunExecutionBindings,
    RunExecutor, RunExecutorError, RunHandle, RunId, RunOrigin, RunPreparation, RunPrepared,
    RunRetention, RunRoot, RunSelections, RunSettlement, RunStart, RunTransaction,
};
use ego_core::sink::{EventSink, NullSink};
use ego_core::store::blob::{BlobId, BlobRef};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::subagent::{
    ChildJournal, ChildJournalError, ChildProfileResolver, ChildRequest, ChildResultEvidence,
    ChildResultReader, ChildResultReaderError, ChildResultStore, ChildRunService,
    ChildSelectionAdmission, ChildServicePorts, ChildTaskReader, ChildWait, ChildWaitResult,
};
use futures::future::{self, BoxFuture};
use futures::stream::{self, BoxStream};

const DEFAULT_TASK_BYTES: &[u8] = b"rendered default task";
const RESULT: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn child_event_kind(event: &DomainEvent) -> &'static str {
    match &event.kind {
        EventKind::SubagentRequested { .. } => "subagent_requested",
        EventKind::SubagentSpawned { .. } => "subagent_spawned",
        EventKind::SubagentSettled { .. } => "subagent_settled",
        _ => "not a child lifecycle event",
    }
}

fn blob(hex: &str, bytes: u64) -> BlobRef {
    BlobRef::new(BlobId::parse(hex).expect("test blob id"), bytes)
}

fn blob_for_bytes(bytes: &[u8]) -> BlobRef {
    blob(&ego_core::digest::sha256_hex(bytes), bytes.len() as u64)
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

fn parent_run() -> RunId {
    static PARENT: OnceLock<RunId> = OnceLock::new();
    *PARENT.get_or_init(|| RunId::new(SessionId::new(), uuid::Uuid::now_v7()))
}

fn profile_name_from_owner() -> String {
    let layer = ConfigLayer::parse(Source::User, "[profile.worker]\n").expect("profile layer");
    resolve_selected(
        std::slice::from_ref(&layer),
        Trust::Trusted,
        Some(("worker", Source::Flags)),
    )
    .expect("profile owner resolves the selected profile")
    .active_profile
    .expect("resolved profile provenance")
    .name
}

#[derive(Clone)]
struct ExistingAuthorityAdmission {
    // Effective owns profile/permissions/sandbox/MCP configuration and
    // RunConfinement owns execution confinement. Admitted tool names remain
    // behind RunExecutionBindings::admitted_tools (CapturedToolCatalogue's
    // entries are crate-private), so the narrow admission port is the only
    // boundary needed to apply the real tool owner without duplicating sets.
    parent: Effective,
    system: Effective,
    parent_confinement: RunConfinement,
    system_confinement: RunConfinement,
    calls: Arc<Mutex<usize>>,
    requests: Arc<Mutex<Vec<SubagentInvocationId>>>,
}

impl ExistingAuthorityAdmission {
    fn new() -> Self {
        let parent = resolve(
            &[ConfigLayer::parse(Source::User, "").expect("parent config")],
            Trust::Trusted,
            None,
        )
        .expect("parent config resolves");
        let system = resolve(
            &[ConfigLayer::parse(Source::System, "").expect("system config")],
            Trust::Trusted,
            None,
        )
        .expect("system config resolves");
        let workspace = CanonPath::new("/workspace");
        Self {
            parent,
            system,
            parent_confinement: RunConfinement::new(
                Confinement::Workspace,
                true,
                workspace.clone(),
            )
            .expect("parent confinement"),
            system_confinement: RunConfinement::new(Confinement::Workspace, true, workspace)
                .expect("system confinement"),
            calls: Arc::new(Mutex::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().expect("authority admissions")
    }
}

impl ChildSelectionAdmission for ExistingAuthorityAdmission {
    fn admit(
        &self,
        request: &ChildRequest,
        bindings: &RunExecutionBindings,
    ) -> Result<(RunSelections, RunExecutionBindings), String> {
        *self.calls.lock().expect("authority admissions") += 1;
        self.requests
            .lock()
            .expect("authority requests")
            .push(request.invocation());
        let _parent_effective = &self.parent;
        let _system_effective = &self.system;
        if self.parent_confinement.level() != self.system_confinement.level() {
            return Err("parent and system confinement disagree".to_owned());
        }
        if request
            .requested_tools()
            .is_some_and(|tools| tools.iter().any(|tool| tool != "read"))
        {
            return Err("requested tools widen the admitted tool owner".to_owned());
        }
        if request.requested_permission() != Some("allow(read(src/**))") {
            return Err("requested permission is not admitted by Ruleset".to_owned());
        }
        if request.requested_sandbox() != Some("workspace")
            || request.requested_offline() != Some(true)
            || request
                .requested_mcp()
                .is_some_and(|servers| servers.iter().any(|server| server != "trusted"))
        {
            return Err("requested confinement or MCP widens the owner".to_owned());
        }
        Ok((request.selections().clone(), bindings.clone()))
    }
}

#[derive(Clone)]
struct ExistingProfileOwner {
    layers: Vec<ego_core::config::Sourced<ConfigLayer>>,
    trust: Trust,
    calls: Arc<Mutex<usize>>,
}

fn default_profile_owner() -> Arc<dyn ChildProfileResolver> {
    Arc::new(ExistingProfileOwner {
        layers: vec![
            ConfigLayer::parse(Source::User, "[profile.worker]\n").expect("default profile layer"),
        ],
        trust: Trust::Trusted,
        calls: Arc::new(Mutex::new(0)),
    })
}

impl ChildProfileResolver for ExistingProfileOwner {
    fn resolve_profile(&self, name: &str) -> Result<(), String> {
        *self.calls.lock().expect("profile resolutions") += 1;
        resolve_selected(&self.layers, self.trust, Some((name, Source::Flags)))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn request(invocation: SubagentInvocationId, wait: ChildWait) -> ChildRequest {
    request_with_task(invocation, wait, blob_for_bytes(DEFAULT_TASK_BYTES))
}

fn request_with_task(
    invocation: SubagentInvocationId,
    wait: ChildWait,
    task: BlobRef,
) -> ChildRequest {
    ChildRequest::new(
        invocation,
        parent_run(),
        TurnId::new(),
        ItemId::new(),
        task,
        selections(),
        wait,
    )
    .with_requested_tools(["read"])
    .with_requested_permission("allow(read(src/**))")
    .with_requested_sandbox("workspace")
    .with_requested_offline(true)
    .with_requested_mcp(["trusted"])
    .with_requested_profile(profile_name_from_owner())
}

#[derive(Clone)]
struct DurableJournal {
    events: Arc<Mutex<Vec<DomainEvent>>>,
    fail_next_settled: Arc<Mutex<bool>>,
}

impl Default for DurableJournal {
    fn default() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            fail_next_settled: Arc::new(Mutex::new(false)),
        }
    }
}

#[derive(Default, Clone)]
struct DurableResults {
    results: Arc<Mutex<BTreeMap<RunId, ChildResultEvidence>>>,
    reads: Arc<Mutex<usize>>,
}

impl DurableResults {
    fn record_evidence(&self, run: RunId, item: BlobRef, bytes: impl Into<Vec<u8>>) {
        self.results
            .lock()
            .expect("result evidence")
            .insert(run, ChildResultEvidence::new(item, bytes.into()));
    }

    fn read_count(&self) -> usize {
        *self.reads.lock().expect("result reads")
    }
}

impl ChildResultReader for DurableResults {
    fn read_final_assistant(
        &self,
        run: RunId,
    ) -> Result<ChildResultEvidence, ChildResultReaderError> {
        *self.reads.lock().expect("result reads") += 1;
        self.results
            .lock()
            .expect("result evidence")
            .get(&run)
            .cloned()
            .ok_or(ChildResultReaderError::Missing { run })
    }
}

// Final S2 content ports: the task reader returns rendered bytes, while the
// result reader returns child item evidence. The parent store owns the only
// cross-session copy and returns the parent-owned BlobRef.
#[derive(Clone, Default)]
struct RenderedTaskReader {
    bytes: Arc<Mutex<Vec<u8>>>,
    reads: Arc<Mutex<Vec<BlobRef>>>,
}

impl RenderedTaskReader {
    fn with_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(bytes.into())),
            reads: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl ChildTaskReader for RenderedTaskReader {
    fn read_rendered_task(&self, reference: &BlobRef) -> Result<Vec<u8>, ChildResultReaderError> {
        self.reads
            .lock()
            .expect("task reads")
            .push(reference.clone());
        Ok(self.bytes.lock().expect("task bytes").clone())
    }
}

#[derive(Clone, Default)]
struct ParentBlobStore {
    copied: Arc<Mutex<Vec<(RunId, ChildResultEvidence, BlobRef)>>>,
    fail_copy: Arc<Mutex<bool>>,
}

impl ParentBlobStore {
    fn fail_copy(&self) {
        *self.fail_copy.lock().expect("copy failure flag") = true;
    }

    fn copied(&self) -> Vec<(RunId, ChildResultEvidence, BlobRef)> {
        self.copied.lock().expect("copied results").clone()
    }
}

impl ChildResultStore for ParentBlobStore {
    fn copy_to_parent(
        &self,
        child: RunId,
        evidence: &ChildResultEvidence,
    ) -> Result<BlobRef, ChildResultReaderError> {
        if *self.fail_copy.lock().expect("copy failure flag") {
            return Err(ChildResultReaderError::Read {
                diagnostic: "injected parent blob copy failure".to_owned(),
            });
        }
        let parent = blob_for_bytes(evidence.bytes());
        self.copied
            .lock()
            .expect("copied results")
            .push((child, evidence.clone(), parent.clone()));
        Ok(parent)
    }
}

impl DurableJournal {
    fn records(&self) -> Vec<DomainEvent> {
        self.events.lock().expect("journal events").clone()
    }

    fn truncate_at(&self, cut: ego_core::subagent::RecoveryCut) {
        self.events
            .lock()
            .expect("journal events")
            .retain(|event| match cut {
                ego_core::subagent::RecoveryCut::BeforeRequest => false,
                ego_core::subagent::RecoveryCut::AfterRequest => {
                    matches!(&event.kind, EventKind::SubagentRequested { .. })
                }
                ego_core::subagent::RecoveryCut::AfterChildAdmission => {
                    matches!(&event.kind, EventKind::SubagentRequested { .. })
                }
                ego_core::subagent::RecoveryCut::AfterSpawn => matches!(
                    &event.kind,
                    EventKind::SubagentRequested { .. } | EventKind::SubagentSpawned { .. }
                ),
                ego_core::subagent::RecoveryCut::AfterChildSettlement
                | ego_core::subagent::RecoveryCut::BeforeParentSettlement => true,
            });
    }

    fn fail_next_settled(&self) {
        *self.fail_next_settled.lock().expect("settled failure flag") = true;
    }
}

impl ChildJournal for DurableJournal {
    fn append(&self, event: DomainEvent) -> Result<(), ChildJournalError> {
        let fail = matches!(&event.kind, EventKind::SubagentSettled { .. }) && {
            let mut flag = self.fail_next_settled.lock().expect("settled failure flag");
            let fail = *flag;
            *flag = false;
            fail
        };
        if fail {
            return Err(ChildJournalError::new(
                "injected parent settlement durability failure",
            ));
        }
        self.events.lock().expect("journal events").push(event);
        Ok(())
    }

    fn read(
        &self,
        invocation: SubagentInvocationId,
    ) -> Result<Vec<DomainEvent>, ChildJournalError> {
        Ok(self
            .records()
            .into_iter()
            .filter_map(|event| match &event.kind {
                EventKind::SubagentRequested { invocation: id, .. }
                | EventKind::SubagentSpawned { invocation: id, .. }
                | EventKind::SubagentSettled { invocation: id, .. }
                    if *id == invocation =>
                {
                    Some(event)
                }
                _ => None,
            })
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

#[derive(Clone)]
struct ReturnHookPort {
    decision: HookDecision,
    started: Arc<Mutex<Vec<HookStartedEvidence>>>,
    settled: Arc<Mutex<Vec<HookInvocationId>>>,
}

impl ReturnHookPort {
    fn new(decision: HookDecision) -> Self {
        Self {
            decision,
            started: Arc::new(Mutex::new(Vec::new())),
            settled: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl HookPort for ReturnHookPort {
    fn started<'a>(
        &'a self,
        spec: &'a ego_core::hook::HookSpec,
        _event: &'a HookEvent,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> HookStartFuture<'a, Result<HookStartedEvidence, HookStartError>> {
        let evidence = HookStartedEvidence::new(spec.id(), invocation, activation);
        self.started
            .lock()
            .expect("return hook starts")
            .push(evidence);
        Box::pin(future::ready(Ok(evidence)))
    }

    fn invoke<'a>(
        &'a self,
        _spec: &'a ego_core::hook::HookSpec,
        _event: HookEvent,
    ) -> HookFuture<'a> {
        let outcome = HookOutcome::new(self.decision, None);
        Box::pin(future::ready(Ok(outcome)))
    }

    fn settled<'a>(
        &'a self,
        _spec: &'a ego_core::hook::HookSpec,
        invocation: HookInvocationId,
        _outcome: Result<&'a HookOutcome, &'a HookFailure>,
    ) -> HookRecordFuture<'a> {
        self.settled
            .lock()
            .expect("return hook settlements")
            .push(invocation);
        Box::pin(future::ready(Ok(())))
    }
}

fn return_hook_catalogue() -> HookCatalogue {
    HookCatalogue::admit_typed(
        &[],
        &[HookSource::skill_entries_with_authority(
            "skills/return/SKILL.md",
            ego_core::event::skill::SkillInvocationId::new(),
            SkillSourceAuthority::User,
            [HookConfigEntry::new(
                HookLifecycle::SubagentStop,
                None,
                HookHandlerConfig::Command {
                    command: "/usr/bin/return-hook".to_owned(),
                    args: None,
                    shell: None,
                    timeout_seconds: None,
                },
            )],
        )],
        Trust::Trusted,
    )
    .expect("return hook catalogue")
}

struct Executor {
    calls: Mutex<Vec<&'static str>>,
    prompts: Mutex<Vec<String>>,
    settlement: Mutex<RunSettlement>,
    reservations: Mutex<Vec<Reservation>>,
}

impl Executor {
    fn new(settlement: RunSettlement) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
            settlement: Mutex::new(settlement),
            reservations: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("executor calls").len()
    }

    fn reservation_count(&self) -> usize {
        self.reservations.lock().expect("reservations").len()
    }
}

fn generation_metering() -> Metering {
    Metering::Currency {
        settlement: SettlementSource::Quote,
    }
}

fn generation_quote() -> PricingQuote {
    PricingQuote {
        revision: PricingRevision("test-m4-child".to_owned()),
        currency: CurrencyCode::new("USD").expect("USD is valid"),
        uncached_input: Some("0.000003".parse::<Amount>().expect("rate")),
        cache_read: Some("0.0000003".parse::<Amount>().expect("rate")),
        cache_write: Some("0.00000375".parse::<Amount>().expect("rate")),
        cache_write_extended: None,
        output: Some("0.000015".parse::<Amount>().expect("rate")),
    }
}

fn generation_request(prompt: &str) -> GenRequest {
    GenRequest {
        model: "provider/model".to_owned(),
        messages: Conversation::new(vec![Message::text(Role::User, prompt.to_owned())])
            .expect("one user message is valid"),
        tools: Vec::new(),
        thinking: ThinkingLevel::Off,
        max_output_tokens: Some(1024),
        parallel_tool_calls: false,
    }
}

fn generation_capabilities() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        max_context_tokens: Some(4096),
        max_output_tokens: Some(1024),
        max_tools: Some(128),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::Short,
        transport: Transport::Sse,
        image_input: ego_core::generator::capabilities::ImageInputSupport::Unknown,
    }
}

impl RunExecutor for Executor {
    type Prepared = ();

    fn prepare<'a>(
        &'a self,
        _request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        self.calls.lock().expect("executor calls").push("prepare");
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
        self.calls.lock().expect("executor calls").push("start");
        Box::pin(future::ready(Ok(RunHandle::new(request.run_id, ()))))
    }

    fn recover<'a>(
        &'a self,
        _run: &'a RunId,
    ) -> BoxFuture<'a, Result<RecoveryEvidence, RunExecutorError>> {
        self.calls.lock().expect("executor calls").push("recover");
        Box::pin(future::ready(Ok(RecoveryEvidence::Ambiguous)))
    }

    fn observe<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(future::ready(Ok(self
            .settlement
            .lock()
            .expect("settlement")
            .clone())))
    }

    fn submit<'a>(
        &'a self,
        _handle: &'a RunHandle,
        prompt: &'a str,
        _prepared: Self::Prepared,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        self.calls.lock().expect("executor calls").push("submit");
        self.prompts
            .lock()
            .expect("prompts")
            .push(prompt.to_owned());
        let reservation = bindings
            .budget_gate()
            .expect("the execution binding owns the shared live budget gate")
            .lock()
            .expect("shared live budget gate")
            .reserve(
                &generation_metering(),
                Some(&generation_quote()),
                &generation_request(prompt),
                &generation_capabilities(),
            );
        let reservation = match reservation {
            Ok(reservation) => reservation,
            Err(error) => {
                return Box::pin(future::ready(Err(RunExecutorError::new(format!(
                    "budget reservation denied: {error}"
                )))));
            }
        };
        self.reservations
            .lock()
            .expect("reservations")
            .push(reservation);
        Box::pin(future::ready(Ok(self
            .settlement
            .lock()
            .expect("settlement")
            .clone())))
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
    parent_run: RunId,
    store: Arc<Store>,
    executor: Arc<Executor>,
    journal: DurableJournal,
    results: DurableResults,
    task_reader: RenderedTaskReader,
    parent_blobs: ParentBlobStore,
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
            RunRetention::new(std::num::NonZeroUsize::new(8).expect("retention"))
                .expect("retention"),
        ));
        Self {
            parent_run: parent_run(),
            store,
            executor,
            journal: DurableJournal::default(),
            results: DurableResults::default(),
            task_reader: RenderedTaskReader::with_bytes(DEFAULT_TASK_BYTES.to_vec()),
            parent_blobs: ParentBlobStore::default(),
            controller,
        }
    }

    fn service(&self, gate: BudgetGate) -> ChildRunService {
        let gate = Arc::new(Mutex::new(gate));
        self.service_with_ports(gate, self.default_ports())
    }

    fn service_with_gate_and_authority(
        &self,
        gate: Arc<Mutex<BudgetGate>>,
        authority: Arc<dyn ChildSelectionAdmission>,
    ) -> ChildRunService {
        self.service_with_ports(
            gate,
            ChildServicePorts::new(
                self.parent_run,
                Arc::new(self.journal.clone()),
                Arc::new(self.task_reader.clone()),
                Arc::new(self.results.clone()),
                Arc::new(self.parent_blobs.clone()),
                default_profile_owner(),
                authority,
            ),
        )
    }

    fn default_ports(&self) -> ChildServicePorts {
        ChildServicePorts::new(
            self.parent_run,
            Arc::new(self.journal.clone()),
            Arc::new(self.task_reader.clone()),
            Arc::new(self.results.clone()),
            Arc::new(self.parent_blobs.clone()),
            default_profile_owner(),
            Arc::new(ExistingAuthorityAdmission::new()),
        )
    }

    fn service_with_ports(
        &self,
        gate: Arc<Mutex<BudgetGate>>,
        ports: ChildServicePorts,
    ) -> ChildRunService {
        ChildRunService::new(
            Arc::clone(&self.controller),
            Arc::clone(&gate),
            RunRoot::new("/workspace").expect("absolute root"),
            RunExecutionBindings::new(Arc::new(Sinks), None).with_budget_gate(gate),
            ports,
        )
    }

    fn content_service(
        &self,
        task_reader: Arc<dyn ChildTaskReader>,
        results: Arc<dyn ChildResultReader>,
        parent_blobs: Arc<dyn ChildResultStore>,
    ) -> ChildRunService {
        let gate = Arc::new(Mutex::new(BudgetGate::unlimited(
            ego_core::cost::CurrencyCode::new("USD").expect("currency"),
        )));
        self.service_with_ports(
            gate,
            ChildServicePorts::new(
                self.parent_run,
                Arc::new(self.journal.clone()),
                task_reader,
                results,
                parent_blobs,
                default_profile_owner(),
                Arc::new(ExistingAuthorityAdmission::new()),
            ),
        )
    }

    fn profile_service(&self, owner: Arc<dyn ChildProfileResolver>) -> ChildRunService {
        let gate = Arc::new(Mutex::new(BudgetGate::unlimited(
            ego_core::cost::CurrencyCode::new("USD").expect("currency"),
        )));
        self.service_with_ports(
            gate,
            ChildServicePorts::new(
                self.parent_run,
                Arc::new(self.journal.clone()),
                Arc::new(self.task_reader.clone()),
                Arc::new(self.results.clone()),
                Arc::new(self.parent_blobs.clone()),
                owner,
                Arc::new(ExistingAuthorityAdmission::new()),
            ),
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
    let admission =
        futures::executor::block_on(service.start(request(invocation, ChildWait::Foreground)))
            .expect("child admission");
    harness.results.record_evidence(
        admission.child_run(),
        blob_for_bytes(RESULT.as_bytes()),
        RESULT.as_bytes().to_vec(),
    );
    assert!(
        matches!(admission.origin(), RunOrigin::Subagent { invocation: id, .. } if *id == invocation)
    );
    assert_ne!(admission.child_run(), admission.parent_run());
    assert_ne!(
        admission.child_session(),
        admission.parent_run().session_id()
    );
    let result = futures::executor::block_on(admission.wait()).expect("child settlement");
    assert_eq!(
        result,
        ChildWaitResult::Returned {
            final_message: blob_for_bytes(RESULT.as_bytes())
        }
    );
    let prompts = harness.executor.prompts.lock().expect("prompts").clone();
    assert_eq!(
        prompts.as_slice(),
        &[String::from_utf8(DEFAULT_TASK_BYTES.to_vec()).expect("UTF-8 default task")]
    );
    assert_ne!(prompts[0], blob_for_bytes(DEFAULT_TASK_BYTES).id.as_str());
    assert_eq!(harness.parent_blobs.copied().len(), 1);
    assert_eq!(
        harness
            .journal
            .records()
            .iter()
            .map(child_event_kind)
            .collect::<Vec<_>>(),
        ["subagent_requested", "subagent_spawned", "subagent_settled"],
    );
    assert!(harness.store.all_transactions().iter().any(|transaction| {
        matches!(transaction.origin(), RunOrigin::Subagent { invocation: id, .. } if *id == invocation)
    }));
}

#[test]
fn child_reads_rendered_task_bytes_through_the_injected_reader() {
    let harness = Harness::new(RunSettlement::Completed);
    let task_bytes = b"rendered child task, not its content digest".to_vec();
    let task_reader = Arc::new(RenderedTaskReader::with_bytes(task_bytes.clone()));
    let results = Arc::new(DurableResults::default());
    let parent_blobs = Arc::new(ParentBlobStore::default());
    let service = harness.content_service(task_reader.clone(), results.clone(), parent_blobs);
    let invocation = SubagentInvocationId::new();
    let task = blob_for_bytes(&task_bytes);
    let admission = futures::executor::block_on(service.start(request_with_task(
        invocation,
        ChildWait::Foreground,
        task.clone(),
    )))
    .expect("child admission");
    let result_bytes = b"final child assistant";
    results.record_evidence(
        admission.child_run(),
        blob_for_bytes(result_bytes),
        result_bytes.to_vec(),
    );
    let _ = futures::executor::block_on(admission.wait()).expect("child settlement");
    assert_eq!(
        harness.executor.prompts.lock().expect("prompts").as_slice(),
        &[String::from_utf8(task_bytes.clone()).expect("UTF-8 task")]
    );
    assert_ne!(
        harness.executor.prompts.lock().expect("prompts")[0],
        task.id.as_str()
    );
    assert_eq!(
        task_reader.reads.lock().expect("task reads").as_slice(),
        &[task]
    );
}

#[test]
fn child_result_is_verified_copied_to_parent_and_only_parent_ref_is_returned() {
    let result_bytes = b"verified final assistant bytes";
    let child_item = blob_for_bytes(result_bytes);
    let harness = Harness::new(RunSettlement::Completed);
    let results = Arc::new(DurableResults::default());
    let parent_blobs = Arc::new(ParentBlobStore::default());
    let task = blob_for_bytes(b"task");
    let service = harness.content_service(
        Arc::new(RenderedTaskReader::with_bytes(b"task".to_vec())),
        results.clone(),
        parent_blobs.clone(),
    );
    let invocation = SubagentInvocationId::new();
    let admission = futures::executor::block_on(service.start(request_with_task(
        invocation,
        ChildWait::Foreground,
        task,
    )))
    .expect("child admission");
    results.record_evidence(
        admission.child_run(),
        child_item.clone(),
        result_bytes.to_vec(),
    );
    let returned = futures::executor::block_on(admission.wait()).expect("verified result");
    let parent_ref = match returned {
        ChildWaitResult::Returned { final_message } => final_message,
        other => panic!("expected returned child result, got {other:?}"),
    };
    assert_eq!(parent_ref, child_item);
    assert_eq!(parent_blobs.copied().len(), 1);
    assert_eq!(parent_blobs.copied()[0].2, parent_ref);
    assert!(matches!(
        harness.journal.records().as_slice(),
        [
            DomainEvent {
                kind: EventKind::SubagentRequested { .. },
                ..
            },
            DomainEvent {
                kind: EventKind::SubagentSpawned { .. },
                ..
            },
            DomainEvent {
                kind: EventKind::SubagentSettled {
                    terminal: SubagentTerminal::Returned { final_message },
                    ..
                },
                ..
            }
        ] if *final_message == parent_ref
    ));
}

#[test]
fn child_result_digest_or_parent_copy_failure_fails_closed_without_return_or_settlement() {
    for copy_failure in [false, true] {
        let declared = blob_for_bytes(b"declared item bytes");
        let actual = if copy_failure {
            b"declared item bytes".to_vec()
        } else {
            b"dishonest bytes".to_vec()
        };
        let harness = Harness::new(RunSettlement::Completed);
        let results = Arc::new(DurableResults::default());
        let parent_blobs = Arc::new(ParentBlobStore::default());
        if copy_failure {
            parent_blobs.fail_copy();
        }
        let task = blob_for_bytes(b"task");
        let service = harness.content_service(
            Arc::new(RenderedTaskReader::with_bytes(b"task".to_vec())),
            results.clone(),
            parent_blobs.clone(),
        );
        let invocation = SubagentInvocationId::new();
        let admission = futures::executor::block_on(service.start(request_with_task(
            invocation,
            ChildWait::Foreground,
            task,
        )))
        .expect("child admission");
        results.record_evidence(admission.child_run(), declared, actual);
        let error = futures::executor::block_on(admission.wait()).unwrap_err();
        assert!(matches!(
            error,
            ego_core::subagent::ChildServiceError::Result { .. }
        ));
        assert!(parent_blobs.copied().is_empty());
        assert!(
            harness
                .journal
                .records()
                .iter()
                .all(|event| !matches!(&event.kind, EventKind::SubagentSettled { .. }))
        );
    }
}

#[test]
fn settled_journal_failure_preserves_result_for_exactly_once_retry() {
    let harness = Harness::new(RunSettlement::Completed);
    let service = harness.service(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let invocation = SubagentInvocationId::new();
    let admission =
        futures::executor::block_on(service.start(request(invocation, ChildWait::Foreground)))
            .expect("child admission");
    harness.results.record_evidence(
        admission.child_run(),
        blob_for_bytes(RESULT.as_bytes()),
        RESULT.as_bytes().to_vec(),
    );
    harness.journal.fail_next_settled();
    let first = futures::executor::block_on(admission.wait());
    assert!(matches!(
        first,
        Err(ego_core::subagent::ChildServiceError::Journal { diagnostic })
            if diagnostic.contains("settlement durability")
    ));
    assert!(
        harness
            .journal
            .records()
            .iter()
            .all(|event| !matches!(&event.kind, EventKind::SubagentSettled { .. }))
    );
    let calls_before_retry = harness.executor.call_count();

    let retried = futures::executor::block_on(admission.wait()).expect("recoverable result");
    assert_eq!(
        retried,
        ChildWaitResult::Returned {
            final_message: blob_for_bytes(RESULT.as_bytes())
        }
    );
    assert_eq!(harness.executor.call_count(), calls_before_retry);
    assert_eq!(
        harness
            .journal
            .records()
            .iter()
            .filter(|event| matches!(&event.kind, EventKind::SubagentSettled { .. }))
            .count(),
        1
    );
    assert_eq!(
        futures::executor::block_on(admission.wait()).expect("cached result"),
        retried
    );
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
        let calls_before_recovery = harness.executor.call_count();
        let reopened = harness.service(BudgetGate::unlimited(
            ego_core::cost::CurrencyCode::new("USD").expect("currency"),
        ));
        let recovered = futures::executor::block_on(reopened.recover_from_durable(invocation, cut))
            .expect("durable recovery");
        if cut == ego_core::subagent::RecoveryCut::BeforeRequest {
            assert!(recovered.is_none(), "a missing request is explicit absence");
            assert!(harness.executor.prompts.lock().expect("prompts").is_empty());
            assert_eq!(harness.executor.call_count(), calls_before_recovery);
        } else {
            let recovered = recovered.expect("request record permits lookup");
            assert_eq!(recovered.invocation(), invocation);
            assert_eq!(recovered.admission_count(), 1, "cut {cut:?}");
            assert!(!recovered.delivery_was_retried(), "cut {cut:?}");
        }
    }
}

#[test]
fn request_only_recovery_admits_once_without_executing_or_delivering() {
    let harness = Harness::new(RunSettlement::Completed);
    let authority = Arc::new(ExistingAuthorityAdmission::new());
    let profile = Arc::new(ExistingProfileOwner {
        layers: vec![
            ConfigLayer::parse(Source::User, "[profile.worker]\n").expect("profile layer"),
        ],
        trust: Trust::Trusted,
        calls: Arc::new(Mutex::new(0)),
    });
    let task_bytes = b"request-only rendered task".to_vec();
    let task_reader = Arc::new(RenderedTaskReader::with_bytes(task_bytes.clone()));
    let results = Arc::new(DurableResults::default());
    let parent_blobs = Arc::new(ParentBlobStore::default());
    let service = harness.service_with_ports(
        Arc::new(Mutex::new(BudgetGate::unlimited(
            ego_core::cost::CurrencyCode::new("USD").expect("currency"),
        ))),
        ChildServicePorts::new(
            harness.parent_run,
            Arc::new(harness.journal.clone()),
            task_reader.clone(),
            results.clone(),
            parent_blobs.clone(),
            profile.clone(),
            authority.clone(),
        ),
    );
    let invocation = SubagentInvocationId::new();
    let parent_turn = TurnId::new();
    let parent_item = ItemId::new();
    let task = blob_for_bytes(&task_bytes);
    let child_selections = selections().with_profile_name("worker");
    let mut seq = Seq::default();
    harness
        .journal
        .append(DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn: parent_turn,
                item: Some(parent_item),
                parent: None,
                ts: Timestamp::EPOCH,
                producer: Producer::Main,
            },
            kind: EventKind::SubagentRequested {
                invocation,
                task,
                selections: child_selections,
            },
        })
        .expect("request is durable before admission");

    let recovered = futures::executor::block_on(
        service.recover_from_durable(invocation, ego_core::subagent::RecoveryCut::AfterRequest),
    )
    .expect("request-only recovery")
    .expect("durable request fact");
    assert_eq!(
        recovered.origin(),
        &RunOrigin::Subagent {
            invocation,
            parent_run: harness.parent_run,
            parent_turn,
            parent_item,
        }
    );
    let child_run = recovered
        .child_run()
        .expect("request-only recovery admits one child");
    assert_eq!(recovered.admission_count(), 1);
    assert!(!recovered.delivery_was_retried());
    assert_eq!(
        authority.calls(),
        0,
        "selection admission already happened before request durability"
    );
    assert_eq!(
        *profile.calls.lock().expect("profile resolutions"),
        0,
        "profile resolution already happened before request durability"
    );
    assert_eq!(task_reader.reads.lock().expect("task reads").len(), 1);
    let executor_calls = harness
        .executor
        .calls
        .lock()
        .expect("executor calls")
        .clone();
    assert_eq!(
        executor_calls
            .iter()
            .filter(|call| **call == "prepare")
            .count(),
        1,
        "request-only recovery prepares exactly one child"
    );
    assert_eq!(
        executor_calls
            .iter()
            .filter(|call| **call == "start")
            .count(),
        0,
        "request-only recovery defers the external execution effect"
    );
    assert!(
        executor_calls.iter().all(|call| *call != "submit"),
        "request-only recovery defers execution"
    );
    assert!(harness.executor.prompts.lock().expect("prompts").is_empty());
    assert_eq!(
        results.read_count(),
        0,
        "recovery does not deliver a result"
    );
    assert!(parent_blobs.copied().is_empty());
    let transactions = harness.store.all_transactions();
    assert_eq!(transactions.len(), 1);
    assert_eq!(transactions[0].run_id(), child_run);
    assert_eq!(transactions[0].origin(), recovered.origin());
    let events = harness
        .journal
        .read(invocation)
        .expect("request evidence remains readable");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].env.turn, parent_turn);
    assert_eq!(events[0].env.item, Some(parent_item));
    assert!(matches!(
        &events[1].kind,
        EventKind::SubagentSpawned {
            invocation: actual_invocation,
            child: actual_child,
        } if *actual_invocation == invocation && *actual_child == child_run
    ));

    let result_bytes = b"request-only final assistant";
    let result_ref = blob_for_bytes(result_bytes);
    results.record_evidence(child_run, result_ref.clone(), result_bytes.to_vec());
    let collected = futures::executor::block_on(service.collect_background(invocation))
        .expect("request-only recovery executes retained one-shot");
    assert_eq!(
        collected,
        ChildWaitResult::Returned {
            final_message: result_ref.clone()
        }
    );
    let calls_after_collect = harness
        .executor
        .calls
        .lock()
        .expect("executor calls")
        .clone();
    assert_eq!(
        calls_after_collect
            .iter()
            .filter(|call| **call == "prepare")
            .count(),
        1
    );
    assert_eq!(
        calls_after_collect
            .iter()
            .filter(|call| **call == "start")
            .count(),
        1
    );
    assert_eq!(
        calls_after_collect
            .iter()
            .filter(|call| **call == "submit")
            .count(),
        1
    );
    assert_eq!(results.read_count(), 1);
    assert_eq!(parent_blobs.copied().len(), 1);

    let calls_before_second_collect = harness.executor.call_count();
    assert_eq!(
        futures::executor::block_on(service.collect_background(invocation))
            .expect("cached request-only result"),
        collected
    );
    assert_eq!(
        harness.executor.call_count(),
        calls_before_second_collect,
        "subsequent collection adds no executor effects"
    );
}

#[test]
fn after_spawn_recovery_delegates_uncertainty_without_reexecution_or_result_io() {
    let harness = Harness::new(RunSettlement::Completed);
    let invocation = SubagentInvocationId::new();
    let child_run = {
        let service = harness.service(BudgetGate::unlimited(
            ego_core::cost::CurrencyCode::new("USD").expect("currency"),
        ));
        let admission =
            futures::executor::block_on(service.start(request(invocation, ChildWait::Background)))
                .expect("initial background child admission");
        let child_run = admission.child_run();
        drop(admission);
        child_run
    };
    let events_before_recovery = harness
        .journal
        .read(invocation)
        .expect("durable child lifecycle");
    assert!(matches!(
        events_before_recovery.as_slice(),
        [
            DomainEvent {
                kind: EventKind::SubagentRequested { .. },
                ..
            },
            DomainEvent {
                kind: EventKind::SubagentSpawned { child, .. },
                ..
            }
        ] if *child == child_run
    ));
    let task_reads_before_recovery = harness.task_reader.reads.lock().expect("task reads").len();
    let reopened = harness.service(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let recovered = futures::executor::block_on(
        reopened.recover_from_durable(invocation, ego_core::subagent::RecoveryCut::AfterSpawn),
    )
    .expect("AfterSpawn recovery delegates to the controller")
    .expect("spawned child remains recoverable");
    assert_eq!(recovered.child_run(), Some(child_run));
    let collected = futures::executor::block_on(reopened.collect_background(invocation))
        .expect("uncertain recovery settles through the child service");
    assert_eq!(
        collected,
        ChildWaitResult::Failed {
            settlement: RunSettlement::Uncertain
        }
    );
    let calls_after_recovery = harness
        .executor
        .calls
        .lock()
        .expect("executor calls")
        .clone();
    assert_eq!(
        calls_after_recovery
            .iter()
            .filter(|call| **call == "recover")
            .count(),
        1,
        "reopened recovery delegates to RunController::recover"
    );
    assert_eq!(
        calls_after_recovery
            .iter()
            .filter(|call| **call == "start")
            .count(),
        0,
        "uncertain recovery never starts a new external effect"
    );
    assert_eq!(
        calls_after_recovery
            .iter()
            .filter(|call| **call == "submit")
            .count(),
        0,
        "uncertain recovery never submits a prompt"
    );
    assert_eq!(
        harness.task_reader.reads.lock().expect("task reads").len(),
        task_reads_before_recovery,
        "uncertain recovery does not reread the task"
    );
    assert_eq!(harness.results.read_count(), 0);
    assert!(harness.parent_blobs.copied().is_empty());
    assert_eq!(
        harness
            .journal
            .records()
            .iter()
            .filter(|event| matches!(&event.kind, EventKind::SubagentSettled { .. }))
            .count(),
        1,
        "uncertainty is durably settled exactly once"
    );
}

#[test]
fn child_execution_carries_prompt_and_preserves_each_typed_terminal() {
    for settlement in [
        RunSettlement::Cancelled {
            reason: ego_core::run::CancelReason::User,
        },
        RunSettlement::Interrupted,
        RunSettlement::Uncertain,
    ] {
        let harness = Harness::new(settlement.clone());
        let service = harness.service(BudgetGate::unlimited(
            ego_core::cost::CurrencyCode::new("USD").expect("currency"),
        ));
        let admission = futures::executor::block_on(
            service.start(request(SubagentInvocationId::new(), ChildWait::Foreground)),
        )
        .expect("child admission");
        let result = futures::executor::block_on(admission.wait()).expect("typed settlement");
        assert!(
            matches!(result, ChildWaitResult::Failed { settlement: actual } if actual == settlement)
        );
    }
    let harness = Harness::new(RunSettlement::Completed);
    let service = harness.service(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let invocation = SubagentInvocationId::new();
    let admission =
        futures::executor::block_on(service.start(request(invocation, ChildWait::Foreground)))
            .expect("completed child admission");
    harness.results.record_evidence(
        admission.child_run(),
        blob_for_bytes(RESULT.as_bytes()),
        RESULT.as_bytes().to_vec(),
    );
    let returned = futures::executor::block_on(admission.wait()).expect("completed child");
    assert!(matches!(returned, ChildWaitResult::Returned { .. }));
}

#[test]
fn subagent_stop_uses_real_hook_runner_decision_and_never_accepts_a_forged_id() {
    for decision in [HookDecision::Continue, HookDecision::Block] {
        let harness = Harness::new(RunSettlement::Completed);
        let service = harness.service(BudgetGate::unlimited(
            ego_core::cost::CurrencyCode::new("USD").expect("currency"),
        ));
        let invocation = SubagentInvocationId::new();
        let admission =
            futures::executor::block_on(service.start(request(invocation, ChildWait::Foreground)))
                .expect("completed child admission");
        harness.results.record_evidence(
            admission.child_run(),
            blob_for_bytes(RESULT.as_bytes()),
            RESULT.as_bytes().to_vec(),
        );
        let port = ReturnHookPort::new(decision);
        let runner = HookRunner::new(port.clone());
        let catalogue = return_hook_catalogue();
        let event = HookEvent::turn(HookLifecycle::SubagentStop, HookSubject::Session);
        let terminal = futures::executor::block_on(
            service.run_return_hook(invocation, &runner, &catalogue, event),
        )
        .expect("real SubagentStop return boundary");
        let started = port.started.lock().expect("return hook starts").clone();
        assert_eq!(started.len(), 1);
        assert_eq!(
            port.settled.lock().expect("return hook settlements").len(),
            1
        );
        match decision {
            HookDecision::Continue => {
                assert!(matches!(terminal, SubagentTerminal::Returned { .. }));
            }
            HookDecision::Block => {
                let forged = HookInvocationId::new();
                assert!(matches!(
                    terminal,
                    SubagentTerminal::ReturnBlocked {
                        hook_invocation,
                        ..
                    } if hook_invocation == started[0].invocation() && hook_invocation != forged
                ));
            }
            HookDecision::RequestEscalation => unreachable!("fixture only uses terminal decisions"),
        }
    }
}

#[test]
fn foreground_cancel_and_background_drop_preserve_real_execution_ownership() {
    let harness = Harness::new(RunSettlement::Cancelled {
        reason: ego_core::run::CancelReason::User,
    });
    let service = harness.service(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let foreground = futures::executor::block_on(
        service.start(request(SubagentInvocationId::new(), ChildWait::Foreground)),
    )
    .expect("foreground");
    futures::executor::block_on(foreground.cancel()).expect("foreground cancellation");
    assert!(matches!(
        futures::executor::block_on(foreground.wait()).expect("cancelled result"),
        ChildWaitResult::Failed {
            settlement: RunSettlement::Cancelled { .. }
        }
    ));
    let background = futures::executor::block_on(
        service.start(request(SubagentInvocationId::new(), ChildWait::Background)),
    )
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
    let gate = Arc::new(Mutex::new(BudgetGate::new(ego_core::cost::Money::new(
        "0.04".parse().expect("amount"),
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ))));
    let authority = Arc::new(ExistingAuthorityAdmission::new());
    let service = harness.service_with_gate_and_authority(Arc::clone(&gate), authority.clone());
    let gate_id = gate.lock().expect("shared gate").id();
    let first = futures::executor::block_on(
        service.start(request(SubagentInvocationId::new(), ChildWait::Background)),
    )
    .expect("first child uses shared gate");
    let second = futures::executor::block_on(
        service.start(request(SubagentInvocationId::new(), ChildWait::Background)),
    )
    .expect("second child is admitted before its generation boundary");
    assert_eq!(first.budget_gate(), gate_id);
    assert_eq!(second.budget_gate(), gate_id);
    harness.results.record_evidence(
        first.child_run(),
        blob_for_bytes(RESULT.as_bytes()),
        RESULT.as_bytes().to_vec(),
    );
    assert!(matches!(
        futures::executor::block_on(first.wait()).expect("first child settles"),
        ChildWaitResult::Returned { .. }
    ));
    assert!(
        gate.lock()
            .expect("shared gate")
            .outstanding()
            .is_some_and(|amount| !amount.amount.is_zero())
    );
    let second_result = futures::executor::block_on(second.wait());
    assert!(matches!(
        second_result,
        Err(ego_core::subagent::ChildServiceError::Controller { diagnostic })
            if diagnostic.contains("budget reservation denied")
    ));
    assert_eq!(harness.executor.reservation_count(), 1);
    assert!(
        authority.calls() >= 2,
        "both children pass the authority owner"
    );
}

#[test]
fn profile_owner_refuses_unknown_untrusted_and_widening_profiles_before_controller_effect() {
    let layer = ConfigLayer::parse(Source::User, "[profile.worker]\n").expect("profile layer");
    let owner = Arc::new(ExistingProfileOwner {
        layers: vec![layer],
        trust: Trust::Trusted,
        calls: Arc::new(Mutex::new(0)),
    });
    let harness = Harness::new(RunSettlement::Completed);
    let service = harness.profile_service(owner);
    let known = futures::executor::block_on(
        service.start(request(SubagentInvocationId::new(), ChildWait::Background)),
    )
    .expect("existing profile owner admits known profile");
    assert_eq!(known.selections().profile(), Some("worker"));
    let calls_after_known = harness.executor.call_count();

    let unknown = futures::executor::block_on(
        service.start(
            request(SubagentInvocationId::new(), ChildWait::Background)
                .with_requested_profile("does-not-exist"),
        ),
    );
    assert!(matches!(
        unknown,
        Err(ego_core::subagent::ChildServiceError::ProfileUnavailable { profile })
            if profile == "does-not-exist"
    ));
    assert_eq!(harness.executor.call_count(), calls_after_known);

    let project_layer = ConfigLayer::parse(Source::Project, "[profile.project-only]\n")
        .expect("project profile layer");
    let untrusted = Arc::new(ExistingProfileOwner {
        layers: vec![project_layer],
        trust: Trust::Untrusted,
        calls: Arc::new(Mutex::new(0)),
    });
    let untrusted_service = harness.profile_service(untrusted);
    let refused = futures::executor::block_on(
        untrusted_service.start(
            request(SubagentInvocationId::new(), ChildWait::Background)
                .with_requested_profile("project-only"),
        ),
    );
    assert!(matches!(
        refused,
        Err(ego_core::subagent::ChildServiceError::ProfileUnavailable { profile })
            if profile == "project-only"
    ));
    assert_eq!(harness.executor.call_count(), calls_after_known);

    let widening = futures::executor::block_on(
        service.start(
            request(SubagentInvocationId::new(), ChildWait::Background)
                .with_requested_profile("worker")
                .with_requested_tools(["write"]),
        ),
    );
    assert!(matches!(
        widening,
        Err(ego_core::subagent::ChildServiceError::AuthorityWouldWiden { dimension })
            if dimension == "tools"
    ));
    assert_eq!(harness.executor.call_count(), calls_after_known);
}

#[test]
fn concurrent_children_have_distinct_leases_and_no_parent_transcript() {
    let harness = Harness::new(RunSettlement::Completed);
    let service = harness.service(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let first = futures::executor::block_on(
        service.start(request(SubagentInvocationId::new(), ChildWait::Background)),
    )
    .expect("first child");
    let second = futures::executor::block_on(
        service.start(request(SubagentInvocationId::new(), ChildWait::Background)),
    )
    .expect("second child");
    assert_ne!(first.child_run(), second.child_run());
    assert_ne!(first.turn_lease(), second.turn_lease());
    assert_ne!(first.child_session(), second.child_session());
    assert_eq!(first.budget_gate(), second.budget_gate());
    assert!(
        futures::executor::block_on(
            service.start(
                request(SubagentInvocationId::new(), ChildWait::Background)
                    .with_parent_transcript("parent transcript"),
            )
        )
        .is_err()
    );
}
