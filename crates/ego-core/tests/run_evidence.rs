//! contracts for regression case: authoritative run and generation evidence.

use ego_core::config::ModelRef;
use ego_core::event::domain::GenerationRole;
use ego_core::event::id::{GenerationId, SessionId};
use ego_core::evidence::{
    ConfigProvenance, EVIDENCE_SCHEMA_VERSION, EvidenceAdmissionService, EvidenceError,
    EvidenceKey, EvidenceRecord, EvidenceStore, EvidenceWrite, ExecutionAdmissionDescriptor,
    GenerationAdmissionEvidence, GenerationTarget, ManifestEvidence, McpContextDescriptor,
    PricingEvidence, RunContextEvidence, RunInvocationId,
};
use ego_core::generator::attempt::AttemptRef;
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ImageInputSupport, ModelCapabilities, ReasoningCarryover,
    ReasoningEffortSupport, Transport,
};
use ego_core::public::{InstructionDescriptor, InstructionSource};
use ego_core::run::{
    EventSinkFactory, IdSource, RecoveryEvidence, RequestId, RunAppend, RunAttachment,
    RunCommandOutcome, RunController, RunExecutionBindings, RunExecutor, RunExecutorError,
    RunHandle, RunId, RunPreparation, RunPrepared, RunPrompt, RunRetention, RunRoot, RunSelections,
    RunSettlement, RunTransaction, StartRequest,
};
use ego_core::sink::{EventSink, NullSink};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use futures::future::{self, BoxFuture};
use futures::stream::{self, BoxStream};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

fn invocation() -> RunInvocationId {
    RunInvocationId::new()
}

fn attempt() -> AttemptRef {
    AttemptRef::first(GenerationId::new())
}

fn model() -> ModelRef {
    ModelRef("provider/model".to_owned())
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        schema_version: ModelCapabilities::SCHEMA_VERSION,
        max_context_tokens: Some(8192),
        max_output_tokens: Some(1024),
        max_tools: Some(32),
        supports_parallel_tool_calls: true,
        reasoning_carryover: ReasoningCarryover::Portable,
        reasoning_efforts: ReasoningEffortSupport::Any,
        reasoning_state_dialect: None,
        cancel: CancelSemantics::Cooperative,
        cache: CacheRetention::None,
        transport: Transport::Sse,
        image_input: ImageInputSupport::Unknown,
        default_reasoning_effort: None,
    }
}

fn target() -> GenerationTarget {
    GenerationTarget::new("provider", model(), "wire-model")
}

fn run_context(session: SessionId, id: RunInvocationId) -> RunContextEvidence {
    RunContextEvidence::new(
        session,
        id,
        RunId::new(session, id.uuid()),
        ConfigProvenance::default(),
    )
}

fn instruction(path: &str) -> InstructionDescriptor {
    InstructionDescriptor {
        path: path.to_owned(),
        sha256: "a".repeat(64),
        bytes: 128,
        scope: ".".to_owned(),
        source: InstructionSource::Ambient,
    }
}

fn generation(
    session: SessionId,
    id: RunInvocationId,
    attempt: AttemptRef,
    role: GenerationRole,
) -> GenerationAdmissionEvidence {
    generation_with(
        session,
        id,
        attempt,
        role,
        target(),
        capabilities(),
        PricingEvidence::Unpriced {
            source: "fixture".to_owned(),
            reason: "subscription".to_owned(),
        },
    )
}

fn generation_with(
    session: SessionId,
    id: RunInvocationId,
    attempt: AttemptRef,
    role: GenerationRole,
    target: GenerationTarget,
    capabilities: ModelCapabilities,
    pricing: PricingEvidence,
) -> GenerationAdmissionEvidence {
    GenerationAdmissionEvidence::new(session, id, attempt, role, target, capabilities, pricing)
}

fn contains_key(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.contains_key(needle) || map.values().any(|child| contains_key(child, needle))
        }
        serde_json::Value::Array(values) => values.iter().any(|child| contains_key(child, needle)),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => false,
    }
}

fn assert_no_unsafe_keys(value: &serde_json::Value) {
    for key in [
        "args",
        "env",
        "cwd",
        "url",
        "query",
        "userinfo",
        "credential_headers",
        "raw_config",
        "secrets",
        "raw_prompt",
        "results",
        "errors",
        "reasoning",
    ] {
        assert!(!contains_key(value, key), "unsafe key leaked: {key}");
    }
}

#[derive(Default)]
struct FakeEvidenceStore {
    writes: Mutex<Vec<(EvidenceKey, EvidenceRecord)>>,
    projection_warning: Mutex<Option<String>>,
    fault: Mutex<Option<EvidenceFault>>,
    outcomes: Mutex<Vec<EvidenceWrite>>,
}

#[derive(Clone, Copy)]
enum EvidenceFault {
    Store,
    DurabilityUncertain,
}

impl FakeEvidenceStore {
    fn with_projection_warning(diagnostic: impl Into<String>) -> Self {
        Self {
            writes: Mutex::new(Vec::new()),
            projection_warning: Mutex::new(Some(diagnostic.into())),
            fault: Mutex::new(None),
            outcomes: Mutex::new(Vec::new()),
        }
    }

    fn with_fault(fault: EvidenceFault) -> Self {
        Self {
            writes: Mutex::new(Vec::new()),
            projection_warning: Mutex::new(None),
            fault: Mutex::new(Some(fault)),
            outcomes: Mutex::new(Vec::new()),
        }
    }
}

impl EvidenceStore for FakeEvidenceStore {
    fn write(
        &self,
        key: EvidenceKey,
        record: EvidenceRecord,
    ) -> Result<EvidenceWrite, EvidenceError> {
        if let Some(fault) = self.fault.lock().unwrap().take() {
            return Err(match fault {
                EvidenceFault::Store => EvidenceError::Store {
                    key,
                    diagnostic: "evidence store failed".to_owned(),
                },
                EvidenceFault::DurabilityUncertain => EvidenceError::DurabilityUncertain {
                    key,
                    diagnostic: "evidence durability is uncertain".to_owned(),
                },
            });
        }
        let mut writes = self.writes.lock().unwrap();
        if let Some((existing_key, existing_record)) =
            writes.iter().find(|(existing, _)| existing == &key)
        {
            if existing_record == &record {
                return Ok(EvidenceWrite::AlreadyRecorded);
            }
            return Err(EvidenceError::idempotency_conflict(
                existing_key.clone(),
                existing_record.digest(),
                record.digest(),
            ));
        }
        writes.push((key, record));
        let outcome = match self.projection_warning.lock().unwrap().take() {
            Some(diagnostic) => EvidenceWrite::PublishedWithProjectionWarning { diagnostic },
            None => EvidenceWrite::Published,
        };
        self.outcomes.lock().unwrap().push(outcome.clone());
        Ok(outcome)
    }
}

#[derive(Default)]
struct FakeSessionStore {
    entries: Mutex<Vec<SessionEntry>>,
    sequence: Arc<Mutex<Vec<&'static str>>>,
    settlement_failure: Mutex<Option<String>>,
}

impl FakeSessionStore {
    fn with_sequence(sequence: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            sequence,
            settlement_failure: Mutex::new(None),
        }
    }

    fn fail_next_settlement(&self, diagnostic: impl Into<String>) {
        *self.settlement_failure.lock().unwrap() = Some(diagnostic.into());
    }

    fn entries(&self) -> Vec<SessionEntry> {
        self.entries.lock().unwrap().clone()
    }
}

impl SessionStore for FakeSessionStore {
    fn append(
        &self,
        _id: &SessionId,
        event: &ego_core::event::domain::DomainEvent,
    ) -> Result<(), StoreError> {
        self.entries
            .lock()
            .unwrap()
            .push(SessionEntry::Event(event.clone()));
        Ok(())
    }

    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let entries = self.entries();
        if entries.is_empty() {
            return Err(StoreError::NotFound);
        }
        Ok(Box::pin(stream::iter(entries.into_iter().map(Ok))))
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }

    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        expected_revision: Option<ego_core::run::RunRevision>,
        transaction: RunTransaction,
    ) -> Result<RunAppend, StoreError> {
        let mut entries = self.entries.lock().unwrap();
        let current = entries.iter().rev().find_map(|entry| match entry {
            SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
            SessionEntry::Event(_)
            | SessionEntry::Evidence(_)
            | SessionEntry::NoticeDelivered(_)
            | SessionEntry::ModelSelected(_) => None,
        });
        if expected_revision != current {
            return Ok(current.map_or(
                RunAppend::Conflict {
                    actual: ego_core::run::RunRevision::new(0),
                },
                |actual| RunAppend::Conflict { actual },
            ));
        }
        let marker = match transaction.command() {
            ego_core::run::RunCommand::CommandAdmitted { .. } => "command_admitted",
            ego_core::run::RunCommand::CommandSettled { .. } => "command_settled",
            ego_core::run::RunCommand::CancelRequested { .. } => "cancel_requested",
        };
        self.sequence.lock().unwrap().push(marker);
        if marker == "command_settled"
            && let Some(diagnostic) = self.settlement_failure.lock().unwrap().take()
        {
            return Err(StoreError::Io(std::io::Error::other(diagnostic)));
        }
        let revision = transaction.revision();
        entries.push(SessionEntry::RunTransaction(transaction));
        Ok(RunAppend::Committed { revision })
    }
}

#[derive(Default)]
struct NullSinkFactory;

impl EventSinkFactory for NullSinkFactory {
    fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

struct PreparedRun {
    context: RunContextEvidence,
}

struct FakeRunExecutor {
    admission: EvidenceAdmissionService,
    evidence: FakeEvidenceStore,
    sequence: Arc<Mutex<Vec<&'static str>>>,
    settlements: Mutex<VecDeque<Result<RunSettlement, RunExecutorError>>>,
}

impl FakeRunExecutor {
    fn new(sequence: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self::with_evidence(FakeEvidenceStore::default(), sequence)
    }

    fn with_fault(sequence: Arc<Mutex<Vec<&'static str>>>, fault: EvidenceFault) -> Self {
        Self::with_evidence(FakeEvidenceStore::with_fault(fault), sequence)
    }

    fn with_projection_warning(sequence: Arc<Mutex<Vec<&'static str>>>, diagnostic: &str) -> Self {
        Self::with_evidence(
            FakeEvidenceStore::with_projection_warning(diagnostic),
            sequence,
        )
    }

    fn with_evidence(evidence: FakeEvidenceStore, sequence: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            admission: EvidenceAdmissionService::new(),
            evidence,
            sequence,
            settlements: Mutex::new(VecDeque::new()),
        }
    }

    fn evidence_records(&self) -> usize {
        self.evidence.writes.lock().unwrap().len()
    }

    fn evidence_outcomes(&self) -> Vec<EvidenceWrite> {
        self.evidence.outcomes.lock().unwrap().clone()
    }
}

impl RunExecutor for FakeRunExecutor {
    type Prepared = PreparedRun;

    fn prepare<'a>(
        &'a self,
        request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        self.sequence.lock().unwrap().push("prepare");
        let context = RunContextEvidence::new(
            request.run_id.session_id(),
            request.invocation,
            request.run_id,
            ConfigProvenance::default(),
        );
        Box::pin(future::ready(Ok(RunPrepared::new(
            ModelRef("provider/model".to_owned()),
            PreparedRun { context },
        ))))
    }

    fn admit_run_context<'a>(
        &'a self,
        prepared: &'a Self::Prepared,
    ) -> ego_core::run::RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
        self.sequence.lock().unwrap().push("evidence_write");
        Box::pin(future::ready(
            self.admission
                .admit_run_context(&self.evidence, prepared.context.clone()),
        ))
    }

    fn attach<'a>(
        &'a self,
        request: &'a RunAttachment,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        self.sequence.lock().unwrap().push("start");
        Box::pin(future::ready(Ok(RunHandle::new(request.run_id, ()))))
    }

    fn recover<'a>(
        &'a self,
        _run: &'a ego_core::run::RunId,
    ) -> BoxFuture<'a, Result<RecoveryEvidence, RunExecutorError>> {
        Box::pin(future::ready(Ok(RecoveryEvidence::Ambiguous)))
    }

    fn observe<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(future::ready(Ok(RunSettlement::Completed)))
    }

    fn submit<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _prompt: &'a RunPrompt,
        _prepared: Self::Prepared,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        self.sequence.lock().unwrap().push("submit");
        let settlement = self
            .settlements
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(RunSettlement::Completed));
        Box::pin(future::ready(settlement))
    }

    fn interrupt<'a>(
        &'a self,
        _handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(future::ready(Ok(())))
    }

    /// A no-op for the same reason `interrupt` is one: this double never
    /// runs a turn, so there is nothing here that a hold could hold back.
    fn pause<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _request: RequestId,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(future::ready(Ok(())))
    }
}

#[derive(Default)]
struct FixedClock;

impl ego_core::event::time::Clock for FixedClock {
    fn now(&self) -> ego_core::event::time::Timestamp {
        ego_core::event::time::Timestamp::EPOCH
    }
}

#[derive(Default)]
struct FixedIds;

impl IdSource for FixedIds {
    fn next_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::now_v7()
    }
}

#[test]
fn evidence_record_is_tagged_inside_session_entry_without_consuming_event_seq() {
    let session = SessionId::new();
    let id = invocation();
    let record = EvidenceRecord::RunContext(run_context(session, id));
    let entry = SessionEntry::Evidence(record.clone());
    let wire = serde_json::to_value(&entry).unwrap();
    assert_eq!(wire["kind"], "evidence");
    assert_eq!(wire["evidence"]["kind"], "run_context");
    assert_eq!(
        record.key(),
        EvidenceKey::RunContext {
            session,
            invocation: id
        }
    );
    assert_eq!(record.semantic_event_sequence(), None);
}

#[test]
fn run_context_is_one_record_per_invocation_and_same_digest_is_idempotent() {
    let session = SessionId::new();
    let id = invocation();
    let store = FakeEvidenceStore::default();
    let service = EvidenceAdmissionService::new();
    let context = run_context(session, id);

    assert_eq!(
        service.admit_run_context(&store, context.clone()).unwrap(),
        EvidenceWrite::Published
    );
    assert_eq!(
        service.admit_run_context(&store, context).unwrap(),
        EvidenceWrite::AlreadyRecorded
    );
    assert_eq!(store.writes.lock().unwrap().len(), 1);

    let changed = RunContextEvidence::new(
        session,
        id,
        RunId::new(session, uuid::Uuid::now_v7()),
        ConfigProvenance::default(),
    );
    assert!(matches!(
        service.admit_run_context(&store, changed),
        Err(EvidenceError::IdempotencyConflict { .. })
    ));
    assert_eq!(store.writes.lock().unwrap().len(), 1);
}

#[test]
fn generation_evidence_joins_exact_attempt_and_role_and_changes_need_new_attempt() {
    let session = SessionId::new();
    let id = invocation();
    let first = attempt();
    let store = FakeEvidenceStore::default();
    let service = EvidenceAdmissionService::new();

    let run = run_context(session, id);
    service.admit_run_context(&store, run).unwrap();
    let evidence = generation(session, id, first, GenerationRole::Main);
    assert_eq!(
        service.admit_generation(&store, evidence.clone()).unwrap(),
        EvidenceWrite::Published
    );
    assert_eq!(
        service.admit_generation(&store, evidence).unwrap(),
        EvidenceWrite::AlreadyRecorded
    );

    let changed = generation_with(
        session,
        id,
        first,
        GenerationRole::Main,
        GenerationTarget::new(
            "other-provider",
            ModelRef("other/model".to_owned()),
            "other-wire",
        ),
        ModelCapabilities {
            max_output_tokens: Some(2048),
            ..capabilities()
        },
        PricingEvidence::Unpriced {
            source: "other-fixture".to_owned(),
            reason: "metered".to_owned(),
        },
    );
    assert!(matches!(
        service.admit_generation(&store, changed),
        Err(EvidenceError::IdempotencyConflict { .. })
    ));
    let retry = generation(session, id, first.retry(), GenerationRole::Main);
    assert_eq!(
        service.admit_generation(&store, retry).unwrap(),
        EvidenceWrite::Published
    );
    assert_eq!(store.writes.lock().unwrap().len(), 3);
}

#[test]
fn canonical_digest_is_versioned_length_framed_and_order_deterministic() {
    let session = SessionId::new();
    let service = EvidenceAdmissionService::new();
    let id = invocation();
    let first =
        run_context(session, id).with_instructions(vec![instruction("a"), instruction("bc")]);
    let second =
        run_context(session, id).with_instructions(vec![instruction("ab"), instruction("c")]);
    let first_digest = service.canonical_digest(&first).unwrap();
    let reordered =
        run_context(session, id).with_instructions(vec![instruction("bc"), instruction("a")]);
    assert_ne!(first_digest, service.canonical_digest(&second).unwrap());
    assert_eq!(first_digest, service.canonical_digest(&reordered).unwrap());
    assert!(
        first_digest
            .as_str()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
}

#[test]
fn projection_warning_is_visible_without_withdrawing_effect_authority() {
    let session = SessionId::new();
    let id = invocation();
    let store = FakeEvidenceStore::with_projection_warning("manifest publication failed");
    let service = EvidenceAdmissionService::new();

    assert_eq!(
        service
            .admit_run_context(&store, run_context(session, id))
            .unwrap(),
        EvidenceWrite::PublishedWithProjectionWarning {
            diagnostic: "manifest publication failed".to_owned()
        }
    );
    assert_eq!(store.writes.lock().unwrap().len(), 1);
}

#[test]
fn safe_evidence_is_structurally_descriptor_only_and_pricing_is_explicit() {
    let session = SessionId::new();
    let id = invocation();
    let context = run_context(session, id).with_instructions(vec![
        instruction("AGENTS.md"),
        instruction("nested/AGENTS.md"),
    ]);
    let generation = generation(session, id, attempt(), GenerationRole::Main);
    let context_wire = serde_json::to_value(&context).unwrap();
    let generation_wire = serde_json::to_value(&generation).unwrap();
    assert!(context_wire.to_string().contains("AGENTS.md"));
    assert!(generation_wire.to_string().contains("provider/model"));
    assert!(generation_wire.to_string().contains("wire-model"));
    assert_no_unsafe_keys(&context_wire);
    assert_no_unsafe_keys(&generation_wire);

    let instruction = InstructionDescriptor {
        path: "AGENTS.md".to_owned(),
        sha256: "a".repeat(64),
        bytes: 128,
        scope: ".".to_owned(),
        source: InstructionSource::Ambient,
    };
    assert_no_unsafe_keys(&serde_json::to_value(instruction).unwrap());
    assert!(matches!(
        generation.pricing(),
        PricingEvidence::Unpriced { source, reason }
            if source == "fixture" && reason == "subscription"
    ));
}

#[test]
fn run_context_schema_requires_complete_pre_effect_facts_and_closed_mcp_subjects() {
    let session = SessionId::new();
    let invocation = invocation();
    let wire = serde_json::json!({
        "version": EVIDENCE_SCHEMA_VERSION,
        "session": session,
        "invocation": invocation,
        "run_id": RunId::new(session, invocation.uuid()),
        "origin": {"kind": "top_level"},
        "ego_version": env!("CARGO_PKG_VERSION"),
        "config": ConfigProvenance::default(),
        "config_digest": "a".repeat(64),
        "policy_digest": "b".repeat(64),
        "ceilings": {"money": null, "tokens": null},
        "instructions": [{
            "path": "AGENTS.md",
            "sha256": "c".repeat(64),
            "bytes": 128,
            "scope": ".",
            "source": "ambient"
        }],
        "mcp": [{
            "server": "fixture-server",
            "transport": "stdio",
            "admissions": [{
                "operation": "read_resource",
                "generation_digest": "d".repeat(64),
                "schema_digest": "e".repeat(64)
            }],
            "provenance_digest": "f".repeat(64)
        }],
        "execution": {
            "confinement": "workspace",
            "process_network": false,
            "workspace_digest": "0".repeat(64),
            "capabilities_digest": "1".repeat(64)
        }
    });
    let context: RunContextEvidence = serde_json::from_value(wire.clone())
        .expect("exact current RunContextEvidence deserializes");
    let wire = serde_json::to_value(&context).expect("exact current RunContextEvidence serializes");
    let exact_fields = [
        "version",
        "session",
        "invocation",
        "run_id",
        "origin",
        "ego_version",
        "config",
        "config_digest",
        "policy_digest",
        "ceilings",
        "instructions",
        "mcp",
        "execution",
    ];
    let mut keys: Vec<_> = wire
        .as_object()
        .expect("current RunContextEvidence is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let mut expected = exact_fields.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected);
    for field in [
        "version",
        "session",
        "invocation",
        "run_id",
        "origin",
        "ego_version",
        "config",
        "config_digest",
        "policy_digest",
        "ceilings",
        "instructions",
        "mcp",
        "execution",
    ] {
        assert!(
            wire.get(field).is_some_and(|value| !value.is_null()),
            "complete RunContextEvidence omitted {field}: {wire}"
        );
        let mut missing = wire.clone();
        missing
            .as_object_mut()
            .expect("RunContextEvidence is an object")
            .remove(field);
        assert!(
            serde_json::from_value::<RunContextEvidence>(missing).is_err(),
            "RunContextEvidence accepted missing required field {field}"
        );
    }
    let mcp = wire["mcp"].as_array().expect("MCP descriptors");
    serde_json::from_value::<McpContextDescriptor>(mcp[0].clone())
        .expect("MCP descriptor is the closed typed form");
    let mcp_object = mcp[0].as_object().expect("MCP descriptor object");
    let mut mcp_keys: Vec<_> = mcp_object.keys().map(String::as_str).collect();
    mcp_keys.sort_unstable();
    assert_eq!(
        mcp_keys,
        ["admissions", "provenance_digest", "server", "transport"]
    );
    let admissions = mcp_object["admissions"]
        .as_array()
        .expect("nested MCP admissions");
    let mut admission_keys: Vec<_> = admissions[0]
        .as_object()
        .expect("MCP admission descriptor object")
        .keys()
        .map(String::as_str)
        .collect();
    admission_keys.sort_unstable();
    assert_eq!(
        admission_keys,
        ["generation_digest", "operation", "schema_digest"]
    );
    serde_json::from_value::<ExecutionAdmissionDescriptor>(wire["execution"].clone())
        .expect("execution descriptor has the exact pre-effect shape");
    for forbidden in [
        "endpoint",
        "uri",
        "path",
        "query",
        "userinfo",
        "header",
        "credential",
        "environment",
    ] {
        assert!(
            !contains_key(&wire["mcp"], forbidden),
            "MCP field leaked: {forbidden}"
        );
    }
    assert!(
        !wire.to_string().contains("https://")
            && !wire.to_string().contains("?")
            && !wire.to_string().contains("SECRET"),
        "MCP descriptor serialized an unsafe value: {wire}"
    );
}

#[test]
fn manifest_evidence_rebuilds_from_ordered_records_and_refuses_unknown_prefix() {
    let session = SessionId::new();
    let id = invocation();
    let records = vec![
        EvidenceRecord::RunContext(run_context(session, id)),
        EvidenceRecord::GenerationAdmission(generation(
            session,
            id,
            attempt(),
            GenerationRole::Main,
        )),
    ];
    let manifest = ManifestEvidence::from_records(&records).unwrap();
    assert_eq!(manifest.run_contexts().len(), 1);
    assert_eq!(manifest.generations().len(), 1);
    assert!(ManifestEvidence::from_records_with_unknown(&records).is_err());
}

#[test]
fn persisted_evidence_read_rejects_old_outer_and_inner_schema_versions() {
    assert_eq!(EVIDENCE_SCHEMA_VERSION, 3);
    assert_eq!(ModelCapabilities::SCHEMA_VERSION, 5);
    let session = SessionId::new();
    let id = invocation();
    let record = EvidenceRecord::GenerationAdmission(generation(
        session,
        id,
        attempt(),
        GenerationRole::Main,
    ));
    let entry = SessionEntry::Evidence(record);
    let current = serde_json::to_value(&entry).unwrap();
    let _: SessionEntry = serde_json::from_value(current.clone()).unwrap();

    let mut old_outer = current.clone();
    old_outer["evidence"]["version"] = serde_json::json!(2);
    assert!(serde_json::from_value::<SessionEntry>(old_outer).is_err());

    let mut old_inner = current;
    old_inner["evidence"]["capabilities"]["schema_version"] = serde_json::json!(4);
    assert!(serde_json::from_value::<SessionEntry>(old_inner).is_err());
}

#[test]
fn generation_evidence_version_and_manifest_rebuild_use_current_authorities() {
    let session = SessionId::new();
    let id = invocation();
    let generation = generation(session, id, attempt(), GenerationRole::Main);
    let record = EvidenceRecord::GenerationAdmission(generation.clone());
    assert_eq!(record.version(), EVIDENCE_SCHEMA_VERSION);
    assert_eq!(
        generation.capabilities().schema_version,
        ModelCapabilities::SCHEMA_VERSION
    );

    let source = FixtureEvidenceLog {
        records: vec![EvidenceRecord::RunContext(run_context(session, id)), record],
    };
    let manifest = ManifestEvidence::rebuild(&source, &session).unwrap();
    assert_eq!(manifest.run_contexts().len(), 1);
    assert_eq!(manifest.generations().len(), 1);
}

#[test]
fn manifest_projection_refuses_typed_old_outer_and_inner_versions() {
    assert_eq!(EVIDENCE_SCHEMA_VERSION, 3);
    assert_eq!(ModelCapabilities::SCHEMA_VERSION, 5);
    let session = SessionId::new();
    let id = invocation();
    let attempt = attempt();
    let current = generation(session, id, attempt, GenerationRole::Main);
    let current_records = vec![
        EvidenceRecord::RunContext(run_context(session, id)),
        EvidenceRecord::GenerationAdmission(current.clone()),
    ];
    let manifest = ManifestEvidence::from_records(&current_records).unwrap();
    assert_eq!(manifest.generations().len(), 1);

    for (outer, inner, expected_found, expected_required) in [(3, 4, 4, 5), (2, 5, 2, 3)] {
        let mut wire = serde_json::to_value(&current).unwrap();
        wire["version"] = serde_json::json!(outer);
        wire["capabilities"]["schema_version"] = serde_json::json!(inner);
        let stale: GenerationAdmissionEvidence = serde_json::from_value(wire).unwrap();
        let error = ManifestEvidence::from_records(&[
            EvidenceRecord::RunContext(run_context(session, id)),
            EvidenceRecord::GenerationAdmission(stale),
        ])
        .unwrap_err();
        assert!(matches!(
            error,
            ego_core::evidence::ManifestEvidenceError::UnsupportedVersion { found, required }
                if found == expected_found && required == expected_required
        ));
    }
}

struct FixtureEvidenceLog {
    records: Vec<EvidenceRecord>,
}

impl ego_core::evidence::EvidenceLogSource for FixtureEvidenceLog {
    fn evidence_records(
        &self,
        _session: &SessionId,
    ) -> Result<Vec<EvidenceRecord>, ego_core::evidence::ManifestEvidenceError> {
        Ok(self.records.clone())
    }
}

type TestController = RunController<FakeSessionStore, FakeRunExecutor, FixedClock, FixedIds>;

fn test_controller(store: Arc<FakeSessionStore>, executor: Arc<FakeRunExecutor>) -> TestController {
    RunController::new(
        store,
        executor,
        Arc::new(FixedClock),
        Arc::new(FixedIds),
        RunRetention::new(std::num::NonZeroUsize::new(2).unwrap()).unwrap(),
    )
}

fn start_request() -> StartRequest {
    StartRequest::new(
        ego_core::run::RequestId::session_id(ego_core::run::RequestId::from_uuid(
            uuid::Uuid::from_u128(0x6600_0000_0000_0000_0000_0000_0000_0001),
        )),
        RunRoot::new(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/run-evidence-workspace"),
        )
        .unwrap(),
        ego_core::run::RequestId::from_uuid(uuid::Uuid::from_u128(
            0x6600_0000_0000_0000_0000_0000_0000_0001,
        )),
        ego_core::run::RequestDigest::parse("ordering").unwrap(),
        RunSelections::new(None, None, None, None, None),
        "prompt",
    )
}

fn bindings() -> RunExecutionBindings {
    RunExecutionBindings::new(Arc::new(NullSinkFactory), None)
}

/// The settlement this run was durably closed with, if it was closed at all.
///
/// Returns the settlement rather than comparing against an expected one so a
/// caller can assert both halves — which failure it was, and the diagnostic
/// naming why — and so a mismatch prints what was actually written.
fn settled_settlement(entries: &[SessionEntry]) -> Option<RunSettlement> {
    entries.iter().find_map(|entry| match entry {
        SessionEntry::RunTransaction(transaction) => match transaction.command() {
            ego_core::run::RunCommand::CommandSettled { settlement, .. } => {
                Some(settlement.clone())
            }
            _ => None,
        },
        _ => None,
    })
}

#[test]
fn run_controller_admits_context_between_command_and_the_first_effect() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(FakeSessionStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(FakeRunExecutor::new(Arc::clone(&sequence)));
    let controller = test_controller(Arc::clone(&store), Arc::clone(&executor));
    let outcome =
        futures::executor::block_on(controller.start(start_request(), bindings())).unwrap();
    assert!(matches!(outcome, RunCommandOutcome::Admitted(_)));
    if let RunCommandOutcome::Admitted(admission) = outcome {
        assert_eq!(executor.evidence_records(), 1);
        assert_eq!(
            sequence.lock().unwrap().as_slice(),
            ["prepare", "command_admitted", "evidence_write"]
        );

        futures::executor::block_on(controller.execute(admission.into_execution())).unwrap();
        assert_eq!(
            sequence.lock().unwrap().as_slice(),
            [
                "prepare",
                "command_admitted",
                "evidence_write",
                "start",
                "submit",
                "command_settled"
            ]
        );
        assert_eq!(store.entries().len(), 2);
    }
}

#[test]
fn definite_evidence_failure_settles_interrupted_without_effect_authority() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(FakeSessionStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(FakeRunExecutor::with_fault(
        Arc::clone(&sequence),
        EvidenceFault::Store,
    ));
    let controller = test_controller(Arc::clone(&store), Arc::clone(&executor));

    assert!(futures::executor::block_on(controller.start(start_request(), bindings())).is_err());
    assert_eq!(executor.evidence_records(), 0);
    assert_eq!(
        sequence.lock().unwrap().as_slice(),
        [
            "prepare",
            "command_admitted",
            "evidence_write",
            "command_settled"
        ]
    );
    let settlement = settled_settlement(&store.entries()).expect("the run was settled");
    assert!(
        matches!(settlement, RunSettlement::Interrupted { .. }),
        "a store failure writing evidence settled as something other than an \
         internal failure: {settlement:?}"
    );
    assert!(
        settlement
            .diagnostic()
            .is_some_and(|diagnostic| diagnostic.contains("evidence")),
        "the settlement does not name the evidence failure that caused it: \
         {settlement:?}"
    );
    assert!(!sequence.lock().unwrap().contains(&"start"));
    assert!(!sequence.lock().unwrap().contains(&"submit"));
}

#[test]
fn visible_evidence_uncertainty_settles_uncertain_and_does_not_retry_same_request() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(FakeSessionStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(FakeRunExecutor::with_fault(
        Arc::clone(&sequence),
        EvidenceFault::DurabilityUncertain,
    ));
    let controller = test_controller(Arc::clone(&store), Arc::clone(&executor));

    assert!(futures::executor::block_on(controller.start(start_request(), bindings())).is_err());
    let settlement = settled_settlement(&store.entries()).expect("the run was settled");
    assert!(
        matches!(settlement, RunSettlement::Uncertain { .. }),
        "visible evidence uncertainty settled as something other than \
         uncertain: {settlement:?}"
    );
    assert!(
        settlement
            .diagnostic()
            .is_some_and(|diagnostic| diagnostic.contains("evidence")),
        "the settlement does not name the evidence failure that caused it: \
         {settlement:?}"
    );
    let before_retry = sequence.lock().unwrap().clone();
    assert!(futures::executor::block_on(controller.start(start_request(), bindings())).is_err());
    assert_eq!(sequence.lock().unwrap().as_slice(), before_retry.as_slice());
    assert_eq!(executor.evidence_records(), 0);
    assert!(!sequence.lock().unwrap().contains(&"start"));
    assert!(!sequence.lock().unwrap().contains(&"submit"));
}

#[test]
fn evidence_failure_preserves_root_cause_when_settlement_also_fails() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(FakeSessionStore::with_sequence(Arc::clone(&sequence)));
    store.fail_next_settlement("settlement publication failed");
    let executor = Arc::new(FakeRunExecutor::with_fault(
        Arc::clone(&sequence),
        EvidenceFault::Store,
    ));
    let controller = test_controller(Arc::clone(&store), Arc::clone(&executor));

    let result = futures::executor::block_on(controller.start(start_request(), bindings()));
    assert!(result.is_err());
    let diagnostic = result
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default();
    assert!(diagnostic.contains("evidence store failed"));
    assert!(diagnostic.contains("settlement publication failed"));
    assert!(!sequence.lock().unwrap().contains(&"start"));
    assert!(!sequence.lock().unwrap().contains(&"submit"));
}

#[test]
fn projection_warning_keeps_effect_authority_and_exposes_its_diagnostic() {
    let sequence = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(FakeSessionStore::with_sequence(Arc::clone(&sequence)));
    let executor = Arc::new(FakeRunExecutor::with_projection_warning(
        Arc::clone(&sequence),
        "manifest publication warning",
    ));
    let controller = test_controller(Arc::clone(&store), Arc::clone(&executor));

    let result = futures::executor::block_on(controller.start(start_request(), bindings()));
    assert!(result.is_ok());
    let Some(outcome) = result.ok() else {
        return;
    };
    assert!(matches!(outcome, RunCommandOutcome::Admitted(_)));
    assert_eq!(
        executor.evidence_outcomes(),
        vec![EvidenceWrite::PublishedWithProjectionWarning {
            diagnostic: "manifest publication warning".to_owned()
        }]
    );
    if let RunCommandOutcome::Admitted(admission) = outcome {
        futures::executor::block_on(controller.execute(admission.into_execution())).unwrap();
    }
    assert!(sequence.lock().unwrap().contains(&"start"));
    assert!(sequence.lock().unwrap().contains(&"submit"));
}
