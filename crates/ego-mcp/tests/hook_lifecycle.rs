//! RED contract for the concrete MCP continuation owner.
//!
//! This crosses the real stdio operation and guarded-authority seams.  It
//! intentionally remains RED until `McpContinuationOwner` owns both the
//! asynchronous input-required projection and accepted-answer resume.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ego_core::config::{
    ConfigLayer, Mode, PermCfg, ResolvedMcpEntry, Ruleset, Source, Trust, resolve,
};
use ego_core::event::domain::{DomainEvent, EventKind};
use ego_core::event::hook::{
    HookActivation, HookCatalogueEntryEvidence, HookCatalogueEvidence, HookContentDigest,
    HookDecision, HookDescriptorEvidence, HookId, HookInvocationId, HookLifecycle, HookOutcome,
    HookSourceAuthority, HookSourceLocation,
};
use ego_core::event::id::{EventId, ItemId, Producer, SessionId, TurnId};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::hook::{
    EffectScope, HookCatalogue, HookCausality, HookEvent, HookFuture, HookHandlerConfig,
    HookInvocationPort, HookInvocationRequest, HookLifecycleDispatcher, HookRunner,
    HookStartedEvidence, SessionHookRuntime, SessionHookState,
};
use ego_core::mcp::{ForeignInputRequired, ForeignQuestionPort, ToolExecution};
use ego_core::permit::{
    Authorized, CanonPath, Classification, CleanupCertainty, DnsResolution, DuplexEnd,
    DuplexHandle, DuplexSession, DuplexSpawner, GuardedEffects, OneHop, OneHopResponse,
    PolicyEngine, PrepareCtx, PreparedCall, ProcessError, ProcessExecutor, Resolver, SpawnSpec,
    Tool, ToolResult, TransportError, TransportFailure,
};
use ego_core::question::{QuestionAnswer, QuestionOutcome, QuestionTarget};
use ego_core::sink::NullSink;
use ego_core::state::SessionState;
use ego_core::store::blob::{BlobError, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter};
use ego_core::store::evidence::{
    EvidenceError, EvidenceId, EvidencePage, EvidenceRead, EvidenceRef, EvidenceStore,
    EvidenceWriter,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::suspension::{
    ClaimedContinuation, ContinuationError, ContinuationJournal, ContinuationRecord,
    ContinuationState, GuardedMcpResume, McpResumeContext, ResumeKeySource, ResumeSealer,
};
use ego_core::turn::{
    CancelToken, GenerationPolicyCapture, GenerationPolicyCaptureStore,
    GenerationPolicyCaptureStoreError, GenerationPolicyCaptureStoreErrorKind,
    GenerationPolicyCaptureStoreOperation, GenerationPolicyDigest, GenerationPolicySnapshot,
    Journal as SessionJournal, McpRegistryEvent, McpRegistryJournal, McpRegistryJournalError,
    SharedJournal, ToolEffectPorts,
};
use ego_mcp::{
    McpAdmittedCandidate, McpCacheMetadata, McpCandidateAdmission, McpCandidateError,
    McpCandidateIdentity, McpCandidateInventory, McpCapabilities, McpCaptureResolver,
    McpContinuationOwner, McpError, McpFailure, McpFuture, McpInputMapping, McpInputRequired,
    McpRegistryCandidate, McpRegistryCapture, McpRegistryController, McpRegistryRefresh,
    McpServerAdmission, McpStdioOperationFactory, McpTransportMaterial, RegistryScope,
    RegistryTrigger,
};
use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncWrite};
use futures::stream::{self, BoxStream};
use serde_json::json;

const SCHEMA: &[u8] = br#"{"type":"object","properties":{"choice":{"type":"string"}},"required":["choice"],"additionalProperties":false}"#;

#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<String>>>);
impl Trace {
    fn push(&self, item: impl Into<String>) {
        self.0.lock().expect("trace").push(item.into());
    }
    fn events(&self) -> Vec<String> {
        self.0.lock().expect("trace").clone()
    }
}

#[derive(Clone, Copy, Default)]
enum HookMode {
    #[default]
    Continue,
    BlockElicitation,
    BlockResult,
    UncertainResult,
}

#[derive(Clone, Default)]
struct RecordingPort {
    calls: Arc<Mutex<Vec<HookLifecycle>>>,
    trace: Trace,
    mode: HookMode,
}
impl RecordingPort {
    fn new(trace: Trace, mode: HookMode) -> Self {
        Self {
            calls: Arc::default(),
            trace,
            mode,
        }
    }
}
impl HookInvocationPort for RecordingPort {
    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        let lifecycle = request.lifecycle();
        self.calls.lock().expect("hook calls").push(lifecycle);
        self.trace.push(lifecycle.as_str());
        let decision = match (self.mode, lifecycle) {
            (HookMode::BlockElicitation, HookLifecycle::Elicitation)
            | (HookMode::BlockResult, HookLifecycle::ElicitationResult) => HookDecision::Block,
            _ => HookDecision::Continue,
        };
        Box::pin(std::future::ready(Ok(HookOutcome::new(decision, None))))
    }
}

#[derive(Default)]
struct HookSessionStore {
    events: Mutex<Vec<DomainEvent>>,
    mode: HookMode,
    lifecycle: Mutex<Option<HookLifecycle>>,
}

impl SessionStore for HookSessionStore {
    fn append(&self, _session: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        if matches!(self.mode, HookMode::UncertainResult)
            && matches!(event.kind, EventKind::HookInvocationSettled { .. })
            && *self.lifecycle.lock().expect("hook lifecycle")
                == Some(HookLifecycle::ElicitationResult)
        {
            return Err(StoreError::Io(std::io::Error::other(
                "simulated delivery uncertainty",
            )));
        }
        if let EventKind::HookLifecycleReached { occurrence, .. } = &event.kind {
            *self.lifecycle.lock().expect("hook lifecycle") = Some(occurrence.lifecycle());
        }
        self.events
            .lock()
            .expect("session events")
            .push(event.clone());
        Ok(())
    }

    fn read(
        &self,
        _session: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let events = self
            .events
            .lock()
            .expect("session events")
            .iter()
            .cloned()
            .map(SessionEntry::Event)
            .map(Ok)
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(events)))
    }

    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _expected_revision: Option<ego_core::run::RunRevision>,
        _transaction: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        panic!("run transactions are outside this fixture")
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}

fn hooks_with_mode(mode: HookMode, trace: Trace) -> SessionHookRuntime<'static> {
    let source = "[[hooks]]\nevent = \"Elicitation\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n[[hooks]]\nevent = \"ElicitationResult\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n";
    let catalogue = HookCatalogue::admit_typed(
        &[ConfigLayer::parse(Source::User, source).expect("hooks")],
        &[],
        Trust::Trusted,
    )
    .expect("catalogue");
    let descriptors = HookCatalogueEvidence::new(
        catalogue
            .specs()
            .iter()
            .map(|spec| {
                HookCatalogueEntryEvidence::new(
                    HookDescriptorEvidence::new(
                        HookSourceAuthority::User,
                        spec.location().clone(),
                        spec.content_digest(),
                        spec.ordinal().expect("hook ordinal"),
                    ),
                    HookActivation::Persistent,
                )
            })
            .collect(),
    )
    .expect("catalogue evidence");
    let session = SessionId::new();
    let turn = TurnId::new();
    let item = ItemId::new();
    let store: &'static HookSessionStore = Box::leak(Box::new(HookSessionStore {
        events: Mutex::default(),
        mode,
        lifecycle: Mutex::default(),
    }));
    let clock: &'static FixedClock = Box::leak(Box::new(FixedClock));
    let sink: &'static NullSink = Box::leak(Box::new(NullSink));
    let journal = SessionJournal::new(
        session,
        store,
        clock,
        sink,
        Producer::Main,
        SessionState::default(),
    );
    let shared = SharedJournal::new(journal);
    shared
        .append(
            turn,
            None,
            None,
            EventKind::HookCatalogueAdmitted { descriptors },
        )
        .expect("catalogue admission");
    shared
        .append(
            turn,
            Some(item),
            None,
            EventKind::ToolCallStarted {
                name: "server/lookup".to_owned(),
                args: "{}".to_owned(),
                call_id: Some("mcp-hook-fixture".to_owned()),
            },
        )
        .expect("tool call start");
    let state = SessionHookState::new(
        HookLifecycleDispatcher::new(HookRunner::new(), catalogue),
        shared,
        CancelToken::new(),
        std::time::Instant::now() + Duration::from_secs(60),
    );
    state.bind(Arc::new(RecordingPort::new(trace, mode)))
}

fn tool_causality(runtime: &SessionHookRuntime<'_>) -> HookCausality {
    runtime
        .hook_state()
        .inspect(|state| {
            state.items.iter().find_map(|(item, state)| {
                state
                    .tool_call_started
                    .map(|parent| HookCausality::item(*item, state.turn, parent))
            })
        })
        .expect("inspect continuation causality")
        .expect("MCP fixture has a tool item causality")
}

fn settled_continuation_causality(
    runtime: &SessionHookRuntime<'_>,
    required: &ego_core::mcp::ForeignInputRequired,
    canonical_json: &str,
) -> HookCausality {
    let answer = QuestionAnswer::from_json(required.question_request(), canonical_json)
        .expect("fixture answer matches the durable MCP question schema");
    let (tool_call, turn, tool_started) = runtime
        .hook_state()
        .inspect(|state| {
            state.items.iter().find_map(|(item, state)| {
                state
                    .tool_call_started
                    .map(|parent| (*item, state.turn, parent))
            })
        })
        .expect("inspect continuation question causality")
        .expect("MCP fixture has a tool item causality");
    let question_item = ItemId::new();
    let requested = runtime
        .hook_state()
        .journal()
        .append(
            turn,
            Some(question_item),
            Some(tool_started),
            EventKind::QuestionRequested {
                request: required.question_request().clone(),
                target: QuestionTarget::ForeignContinuation {
                    tool_call,
                    continuation_id: required.continuation_id().to_owned(),
                },
            },
        )
        .expect("durable foreign question request");
    let settled = runtime
        .hook_state()
        .journal()
        .append(
            turn,
            Some(question_item),
            Some(requested),
            EventKind::QuestionSettled {
                outcome: QuestionOutcome::Answered(answer),
            },
        )
        .expect("durable foreign question settlement");
    HookCausality::continuation(required.continuation_id(), turn, tool_call, settled)
        .expect("continuation causality")
}

#[derive(Default)]
struct Journal {
    events: Mutex<Vec<&'static str>>,
    records: Mutex<BTreeMap<String, ContinuationRecord>>,
    latest: Mutex<Option<String>>,
    fail_record: Mutex<bool>,
    fail_pending: Mutex<bool>,
    fail_settle: Mutex<bool>,
    trace: Trace,
}
impl Journal {
    fn with_trace(trace: Trace) -> Self {
        Self {
            events: Mutex::default(),
            records: Mutex::default(),
            latest: Mutex::default(),
            fail_record: Mutex::default(),
            fail_pending: Mutex::default(),
            fail_settle: Mutex::default(),
            trace,
        }
    }
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().expect("journal events").clone()
    }
    fn state(&self) -> Option<ContinuationState> {
        let latest = self.latest.lock().expect("latest record").clone()?;
        self.state_for(&latest)
    }
    fn state_for(&self, id: &str) -> Option<ContinuationState> {
        self.records
            .lock()
            .expect("records")
            .get(id)
            .map(ContinuationRecord::state)
    }
    fn id(&self) -> String {
        self.latest
            .lock()
            .expect("latest record")
            .as_ref()
            .expect("recorded continuation")
            .to_owned()
    }
    fn recorded_capture(&self) -> String {
        let id = self.id();
        self.records
            .lock()
            .expect("records")
            .get(&id)
            .expect("recorded continuation")
            .fingerprints()
            .capture()
            .to_owned()
    }
    fn recorded_policy_digest(&self) -> GenerationPolicyDigest {
        let id = self.id();
        self.records
            .lock()
            .expect("records")
            .get(&id)
            .expect("recorded continuation")
            .generation_policy_digest()
            .clone()
    }
    fn replace_policy_digest(&self, id: &str, digest: &GenerationPolicyDigest) {
        let mut records = self.records.lock().expect("records");
        let mut wire = serde_json::to_value(records.get(id).expect("recorded continuation"))
            .expect("continuation wire");
        wire.as_object_mut().expect("continuation object").insert(
            "generation_policy_digest".to_owned(),
            serde_json::to_value(digest).expect("policy digest wire"),
        );
        records.insert(
            id.to_owned(),
            serde_json::from_value(wire).expect("continuation with replacement policy digest"),
        );
    }
    fn replace_capture_identity(&self, id: &str, capture: &str) {
        let mut records = self.records.lock().expect("records");
        let mut wire = serde_json::to_value(records.get(id).expect("recorded continuation"))
            .expect("continuation wire");
        wire.get_mut("subject")
            .and_then(serde_json::Value::as_object_mut)
            .expect("continuation subject")
            .insert("capture".to_owned(), json!(capture));
        wire.get_mut("fingerprints")
            .and_then(serde_json::Value::as_object_mut)
            .expect("continuation fingerprints")
            .insert("capture".to_owned(), json!(capture));
        records.insert(
            id.to_owned(),
            serde_json::from_value(wire).expect("continuation with replacement capture identity"),
        );
    }
    fn fail_next_record(&self) {
        *self.fail_record.lock().expect("record failure") = true;
    }
    fn fail_next_pending_failure(&self) {
        *self.fail_pending.lock().expect("pending failure") = true;
    }
    fn fail_next_settlement(&self) {
        *self.fail_settle.lock().expect("settlement failure") = true;
    }
    fn push(&self, event: &'static str) {
        self.events.lock().expect("journal events").push(event);
        self.trace.push(event);
    }
}
impl ContinuationJournal for Journal {
    fn record(&self, record: ContinuationRecord) -> Result<(), ContinuationError> {
        self.push("record");
        if std::mem::take(&mut *self.fail_record.lock().expect("record failure")) {
            return Err(ContinuationError::Unavailable(
                "scripted continuation record failure".into(),
            ));
        }
        let id = record.id().to_owned();
        let mut records = self.records.lock().expect("records");
        if records.contains_key(&id) {
            return Err(ContinuationError::InvalidTransition);
        }
        records.insert(id.clone(), record);
        *self.latest.lock().expect("latest record") = Some(id);
        Ok(())
    }
    fn fail_pending(&self, id: &str) -> Result<(), ContinuationError> {
        self.push("fail_pending");
        if std::mem::take(&mut *self.fail_pending.lock().expect("pending failure")) {
            return Err(ContinuationError::Unavailable(
                "scripted pending terminalization failure".into(),
            ));
        }
        self.records
            .lock()
            .expect("records")
            .get_mut(id)
            .ok_or(ContinuationError::NotFound)?
            .fail_pending()
    }
    fn accept_answer(
        &self,
        id: &str,
        answer: &[u8],
        now: Timestamp,
    ) -> Result<(), ContinuationError> {
        let mut records = self.records.lock().expect("records");
        let record = records.get_mut(id).ok_or(ContinuationError::NotFound)?;
        record.accept_answer(answer.to_vec(), now)?;
        self.push("accepted_answer");
        Ok(())
    }
    fn load(&self, id: &str) -> Result<ContinuationRecord, ContinuationError> {
        let bytes = {
            let records = self.records.lock().expect("records");
            let record = records.get(id).ok_or(ContinuationError::NotFound)?;
            serde_json::to_vec(record).map_err(|_| ContinuationError::Corrupt("encode".into()))?
        };
        serde_json::from_slice(&bytes).map_err(|_| ContinuationError::Corrupt("decode".into()))
    }
    fn claim(&self, id: &str, now: Timestamp) -> Result<ClaimedContinuation, ContinuationError> {
        let mut records = self.records.lock().expect("records");
        let record = records.remove(id).ok_or(ContinuationError::NotFound)?;
        let bytes =
            serde_json::to_vec(&record).map_err(|_| ContinuationError::Corrupt("encode".into()))?;
        let claimed = match record.claim(now) {
            Ok(claimed) => claimed,
            Err(error) => {
                records.insert(
                    id.to_owned(),
                    serde_json::from_slice(&bytes)
                        .map_err(|_| ContinuationError::Corrupt("decode".into()))?,
                );
                return Err(error);
            }
        };
        records.insert(
            id.to_owned(),
            serde_json::from_slice(
                &serde_json::to_vec(claimed.record())
                    .map_err(|_| ContinuationError::Corrupt("encode".into()))?,
            )
            .map_err(|_| ContinuationError::Corrupt("decode".into()))?,
        );
        self.push("claim");
        Ok(claimed)
    }
    fn settle(
        &self,
        claimed: ClaimedContinuation,
        state: ContinuationState,
    ) -> Result<(), ContinuationError> {
        self.push("settle");
        if std::mem::take(&mut *self.fail_settle.lock().expect("settlement failure")) {
            return Err(ContinuationError::Unavailable(
                "scripted continuation settlement failure".into(),
            ));
        }
        let id = claimed.id().to_owned();
        let settled = claimed.settle(state)?;
        self.records.lock().expect("records").insert(id, settled);
        Ok(())
    }
}

#[derive(Default)]
struct CaptureStore {
    entries: Mutex<BTreeMap<(String, String), Arc<GenerationPolicySnapshot>>>,
    calls: Mutex<
        Vec<(
            GenerationPolicyCaptureStoreOperation,
            String,
            GenerationPolicyDigest,
        )>,
    >,
    fail_store: Mutex<bool>,
    fail_load: Mutex<Option<GenerationPolicyCaptureStoreErrorKind>>,
    after_load: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    trace: Trace,
}

impl CaptureStore {
    fn with_trace(trace: Trace) -> Self {
        Self {
            trace,
            ..Self::default()
        }
    }

    fn key(id: &str, digest: &GenerationPolicyDigest) -> (String, String) {
        (id.to_owned(), String::from(digest.clone()))
    }

    fn fail_next_store(&self) {
        *self.fail_store.lock().expect("capture store failure") = true;
    }

    fn fail_next_load(&self, kind: GenerationPolicyCaptureStoreErrorKind) {
        *self.fail_load.lock().expect("capture load failure") = Some(kind);
    }

    fn after_next_load(&self, callback: impl FnOnce() + Send + 'static) {
        *self.after_load.lock().expect("capture load callback") = Some(Box::new(callback));
    }

    fn stored_snapshot(
        &self,
        id: &str,
        digest: &GenerationPolicyDigest,
    ) -> Option<Arc<GenerationPolicySnapshot>> {
        self.entries
            .lock()
            .expect("capture entries")
            .get(&Self::key(id, digest))
            .cloned()
    }

    fn contains(&self, id: &str, digest: &GenerationPolicyDigest) -> bool {
        self.entries
            .lock()
            .expect("capture entries")
            .contains_key(&Self::key(id, digest))
    }

    fn evict(&self, id: &str, digest: &GenerationPolicyDigest) {
        self.entries
            .lock()
            .expect("capture entries")
            .remove(&Self::key(id, digest));
    }

    fn calls(
        &self,
    ) -> Vec<(
        GenerationPolicyCaptureStoreOperation,
        String,
        GenerationPolicyDigest,
    )> {
        self.calls.lock().expect("capture calls").clone()
    }

    fn record_call(
        &self,
        operation: GenerationPolicyCaptureStoreOperation,
        id: &str,
        digest: GenerationPolicyDigest,
    ) {
        self.calls
            .lock()
            .expect("capture calls")
            .push((operation, id.to_owned(), digest));
    }

    fn error(
        operation: GenerationPolicyCaptureStoreOperation,
        kind: GenerationPolicyCaptureStoreErrorKind,
    ) -> GenerationPolicyCaptureStoreError {
        GenerationPolicyCaptureStoreError::new(operation, kind)
    }
}

impl GenerationPolicyCaptureStore for CaptureStore {
    fn store(
        &self,
        id: &str,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<(), GenerationPolicyCaptureStoreError> {
        let digest = snapshot.digest();
        self.trace.push("store");
        self.record_call(
            GenerationPolicyCaptureStoreOperation::Store,
            id,
            digest.clone(),
        );
        if std::mem::take(&mut *self.fail_store.lock().expect("capture store failure")) {
            return Err(Self::error(
                GenerationPolicyCaptureStoreOperation::Store,
                GenerationPolicyCaptureStoreErrorKind::Unavailable,
            ));
        }
        self.entries
            .lock()
            .expect("capture entries")
            .insert(Self::key(id, &digest), snapshot);
        Ok(())
    }

    fn load(
        &self,
        id: &str,
        expected: &GenerationPolicyDigest,
    ) -> Result<Arc<GenerationPolicySnapshot>, GenerationPolicyCaptureStoreError> {
        self.trace.push("load");
        self.record_call(
            GenerationPolicyCaptureStoreOperation::Load,
            id,
            expected.clone(),
        );
        if let Some(kind) = self.fail_load.lock().expect("capture load failure").take() {
            return Err(Self::error(
                GenerationPolicyCaptureStoreOperation::Load,
                kind,
            ));
        }
        let snapshot = self.stored_snapshot(id, expected).ok_or_else(|| {
            Self::error(
                GenerationPolicyCaptureStoreOperation::Load,
                GenerationPolicyCaptureStoreErrorKind::Missing,
            )
        })?;
        let callback = self
            .after_load
            .lock()
            .expect("capture load callback")
            .take();
        if let Some(callback) = callback {
            callback();
        }
        Ok(snapshot)
    }

    fn delete(
        &self,
        id: &str,
        expected: &GenerationPolicyDigest,
    ) -> Result<(), GenerationPolicyCaptureStoreError> {
        self.trace.push("delete");
        self.record_call(
            GenerationPolicyCaptureStoreOperation::Delete,
            id,
            expected.clone(),
        );
        self.entries
            .lock()
            .expect("capture entries")
            .remove(&Self::key(id, expected));
        Ok(())
    }
}

#[derive(Default)]
struct MultiJournal {
    events: Mutex<Vec<&'static str>>,
    records: Mutex<BTreeMap<String, ContinuationRecord>>,
}

impl MultiJournal {
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().expect("multi journal events").clone()
    }

    fn push(&self, event: &'static str) {
        self.events
            .lock()
            .expect("multi journal events")
            .push(event);
    }
}

impl ContinuationJournal for MultiJournal {
    fn record(&self, record: ContinuationRecord) -> Result<(), ContinuationError> {
        let id = record.id().to_owned();
        let mut records = self.records.lock().expect("multi journal records");
        if records.insert(id, record).is_some() {
            return Err(ContinuationError::InvalidTransition);
        }
        drop(records);
        self.push("record");
        Ok(())
    }

    fn fail_pending(&self, id: &str) -> Result<(), ContinuationError> {
        self.records
            .lock()
            .expect("multi journal records")
            .get_mut(id)
            .ok_or(ContinuationError::NotFound)?
            .fail_pending()?;
        self.push("fail_pending");
        Ok(())
    }

    fn accept_answer(
        &self,
        id: &str,
        answer: &[u8],
        now: Timestamp,
    ) -> Result<(), ContinuationError> {
        let mut records = self.records.lock().expect("multi journal records");
        records
            .get_mut(id)
            .ok_or(ContinuationError::NotFound)?
            .accept_answer(answer.to_vec(), now)?;
        drop(records);
        self.push("accepted_answer");
        Ok(())
    }

    fn load(&self, id: &str) -> Result<ContinuationRecord, ContinuationError> {
        let records = self.records.lock().expect("multi journal records");
        let record = records.get(id).ok_or(ContinuationError::NotFound)?;
        serde_json::from_slice(
            &serde_json::to_vec(record).map_err(|_| ContinuationError::Corrupt("encode".into()))?,
        )
        .map_err(|_| ContinuationError::Corrupt("decode".into()))
    }

    fn claim(&self, id: &str, now: Timestamp) -> Result<ClaimedContinuation, ContinuationError> {
        let mut records = self.records.lock().expect("multi journal records");
        let record = records.remove(id).ok_or(ContinuationError::NotFound)?;
        let bytes =
            serde_json::to_vec(&record).map_err(|_| ContinuationError::Corrupt("encode".into()))?;
        let claimed = match record.claim(now) {
            Ok(claimed) => claimed,
            Err(error) => {
                records.insert(
                    id.to_owned(),
                    serde_json::from_slice(&bytes)
                        .map_err(|_| ContinuationError::Corrupt("decode".into()))?,
                );
                return Err(error);
            }
        };
        records.insert(
            id.to_owned(),
            serde_json::from_slice(
                &serde_json::to_vec(claimed.record())
                    .map_err(|_| ContinuationError::Corrupt("encode".into()))?,
            )
            .map_err(|_| ContinuationError::Corrupt("decode".into()))?,
        );
        drop(records);
        self.push("claim");
        Ok(claimed)
    }

    fn settle(
        &self,
        claimed: ClaimedContinuation,
        state: ContinuationState,
    ) -> Result<(), ContinuationError> {
        let id = claimed.id().to_owned();
        let settled = claimed.settle(state)?;
        self.records
            .lock()
            .expect("multi journal records")
            .insert(id, settled);
        self.push("settle");
        Ok(())
    }
}

struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::EPOCH
    }
}
struct FixedResumeKey;
impl ResumeKeySource for FixedResumeKey {
    fn resume_key(&self) -> Result<Arc<[u8]>, ContinuationError> {
        Ok(Arc::from(*b"mcp-hook-lifecycle-test-key-32bytes!"))
    }
}

struct NoProcess;
impl ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(std::future::ready(Err(ProcessError::Invariant(
            "fixture does not execute processes".into(),
        ))))
    }
}
struct EmptyBlobs;
impl BlobStore for EmptyBlobs {
    fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError> {
        Err(BlobError::Limit { limit: 0 })
    }
    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        Err(BlobError::NotFound {
            reference: reference.id.clone(),
        })
    }
    fn read_page<'a>(
        &'a self,
        request: &'a BlobRead,
    ) -> BoxFuture<'a, Result<BlobPage, BlobError>> {
        Box::pin(std::future::ready(Err(BlobError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }
    fn collect_garbage(&self, _reachable: &[BlobRef]) -> Result<usize, BlobError> {
        Ok(0)
    }
}
static EMPTY_BLOBS: EmptyBlobs = EmptyBlobs;
struct EmptyEvidence;
struct FixtureEvidenceWriter {
    bytes: Vec<u8>,
}
impl EvidenceWriter for FixtureEvidenceWriter {
    fn append(&mut self, chunk: &[u8]) -> Result<(), EvidenceError> {
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn commit(self: Box<Self>) -> Result<EvidenceRef, EvidenceError> {
        Ok(EvidenceRef::new(
            EvidenceId::parse("0000000000000000000000000000000000000000000000000000000000000000")
                .expect("fixture evidence id"),
            self.bytes.len() as u64,
        ))
    }

    fn abort(self: Box<Self>) -> Result<(), EvidenceError> {
        Ok(())
    }
}
impl EvidenceStore for EmptyEvidence {
    fn blob_store(&self) -> &dyn BlobStore {
        &EMPTY_BLOBS
    }
    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
        Ok(Box::new(FixtureEvidenceWriter { bytes: Vec::new() }))
    }
    fn read_page<'a>(
        &'a self,
        _request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
        Box::pin(std::future::ready(Err(EvidenceError::Limit { limit: 0 })))
    }
}
struct OfflineResolver;
impl Resolver for OfflineResolver {
    fn resolve(
        &self,
        _target: ego_core::permit::NetworkTarget,
        _deadline: std::time::Instant,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        Box::pin(std::future::ready(Err(TransportError::definitely_unsent(
            TransportFailure::Offline,
        ))))
    }
}
struct OfflineOneHop;
impl OneHop for OfflineOneHop {
    fn begin_call(&self) -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(60)
    }
    fn execute(
        &self,
        _connect: ego_core::permit::ResolvedConnect,
        _request: ego_core::permit::HttpRequest,
        _deadline: std::time::Instant,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        Box::pin(std::future::ready(Err(TransportError::definitely_unsent(
            TransportFailure::Offline,
        ))))
    }
}

struct ScriptedDuplex {
    input: VecDeque<u8>,
    output: Vec<u8>,
    reader_waker: Option<std::task::Waker>,
    complete: bool,
    delivery: DeliveryCertainty,
}
impl ScriptedDuplex {
    fn new(complete: bool, delivery: DeliveryCertainty) -> Self {
        Self {
            input: VecDeque::new(),
            output: Vec::new(),
            reader_waker: None,
            complete,
            delivery,
        }
    }

    fn enqueue_response(&mut self, request: &serde_json::Value, result: serde_json::Value) {
        let response = json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(serde_json::Value::Null),
            "result": result,
        });
        self.input
            .extend(serde_json::to_vec(&response).expect("response"));
        self.input.push_back(b'\n');
        if let Some(waker) = self.reader_waker.take() {
            waker.wake();
        }
    }

    fn respond_to(&mut self, request: &serde_json::Value) {
        match request.get("method").and_then(serde_json::Value::as_str) {
            Some("server/discover") => self.enqueue_response(
                request,
                json!({
                    "resultType":"complete",
                    "supportedVersions":["2026-07-28"],
                    "capabilities":{"tools":{"listChanged":false}},
                    "ttlMs":0,
                    "cacheScope":"private",
                    "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"local","version":"1.0"}}
                }),
            ),
            Some("tools/list") => self.enqueue_response(
                request,
                json!({
                    "resultType":"complete",
                    "tools":[{
                        "name":"lookup",
                        "description":"continuation fixture",
                        "inputSchema":{"type":"object","additionalProperties":false}
                    }],
                    "ttlMs":0,
                    "cacheScope":"private"
                }),
            ),
            Some("tools/call") => {
                let result = if self.complete {
                    json!({"resultType":"complete","content":[{"type":"text","text":"retry-complete"}],"isError":false})
                } else {
                    json!({"resultType":"input_required","inputRequests":{"answer":{"method":"elicitation/create","params":{"mode":"form","message":"confirm","requestedSchema":{"type":"object","properties":{"choice":{"type":"string"}},"required":["choice"],"additionalProperties":false}}}},"requestState":"opaque-state"})
                };
                self.enqueue_response(request, result);
            }
            Some("notifications/initialized") | None => {}
            Some(_) => {}
        }
    }
}
impl AsyncRead for ScriptedDuplex {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.input.is_empty() {
            self.reader_waker = Some(cx.waker().clone());
            return std::task::Poll::Pending;
        }
        let amount = bytes.len().min(self.input.len());
        for slot in &mut bytes[..amount] {
            *slot = self.input.pop_front().expect("input byte");
        }
        std::task::Poll::Ready(Ok(amount))
    }
}
impl AsyncWrite for ScriptedDuplex {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.output.extend_from_slice(bytes);
        while let Some(index) = self.output.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.output.drain(..=index).collect();
            if let Ok(request) = serde_json::from_slice::<serde_json::Value>(&line[..index]) {
                self.respond_to(&request);
            }
        }
        let _ = cx;
        std::task::Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let _ = cx;
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let _ = cx;
        std::task::Poll::Ready(Ok(()))
    }
}
impl DuplexSession for ScriptedDuplex {
    fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
    fn settle(self: Box<Self>) -> BoxFuture<'static, ego_core::permit::DuplexSettlement> {
        let delivery = self.delivery;
        Box::pin(std::future::ready(ego_core::permit::DuplexSettlement::new(
            DuplexEnd::Exited { code: 0 },
            CleanupCertainty::Complete,
            delivery,
            Ok(ToolResult::builtin("settled")),
        )))
    }
    fn interrupt(self: Box<Self>) -> BoxFuture<'static, ego_core::permit::DuplexSettlement> {
        let delivery = self.delivery;
        Box::pin(std::future::ready(ego_core::permit::DuplexSettlement::new(
            DuplexEnd::Interrupted,
            CleanupCertainty::Complete,
            delivery,
            Ok(ToolResult::builtin("interrupted")),
        )))
    }
}

#[derive(Clone, Copy)]
enum ScriptedTerminal {
    Complete,
    TransportFailure(DeliveryCertainty),
}

#[derive(Clone)]
struct ScriptedSpawner {
    calls: Arc<AtomicUsize>,
    input_required_rounds: usize,
    terminal: ScriptedTerminal,
}
impl ScriptedSpawner {
    fn new() -> Self {
        Self::with_input_required_rounds(1)
    }
    fn with_input_required_rounds(input_required_rounds: usize) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            input_required_rounds,
            terminal: ScriptedTerminal::Complete,
        }
    }
    fn with_resume_transport_failure(delivery: DeliveryCertainty) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            input_required_rounds: 1,
            terminal: ScriptedTerminal::TransportFailure(delivery),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
impl DuplexSpawner for ScriptedSpawner {
    fn spawn(&self, _spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let outcome = if call < self.input_required_rounds {
            Ok(Box::new(ScriptedDuplex::new(
                false,
                DeliveryCertainty::DefinitelySent,
            )) as DuplexHandle)
        } else {
            match self.terminal {
                ScriptedTerminal::Complete => Ok(Box::new(ScriptedDuplex::new(
                    true,
                    DeliveryCertainty::DefinitelySent,
                )) as DuplexHandle),
                ScriptedTerminal::TransportFailure(DeliveryCertainty::DefinitelyUnsent) => {
                    Err(TransportError::definitely_unsent(
                        TransportFailure::Exchange("scripted resumed effect failure".into()),
                    ))
                }
                ScriptedTerminal::TransportFailure(
                    DeliveryCertainty::DefinitelySent | DeliveryCertainty::PossiblySent,
                ) => Err(TransportError::possibly_sent(TransportFailure::Exchange(
                    "scripted resumed effect failure".into(),
                ))),
            }
        };
        Box::pin(std::future::ready(outcome))
    }
}

fn server_admission() -> Arc<McpServerAdmission> {
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.server]\ntransport='stdio'\ncommand='mcp-server'\n",
    )
    .expect("config");
    let resolved = resolve(&[layer], Trust::Trusted, None)
        .expect("resolved")
        .mcp;
    let (name, config) = resolved
        .entries()
        .iter()
        .find_map(|(name, entry)| match entry {
            ResolvedMcpEntry::Configured { server, .. } => Some((name.clone(), server.clone())),
            ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .expect("server");
    Arc::new(
        McpServerAdmission::new(name, config, McpTransportMaterial::stdio(Vec::new()))
            .expect("admission"),
    )
}
fn captured_lookup(factory: Arc<McpStdioOperationFactory>) -> (McpRegistryCapture, Arc<dyn Tool>) {
    let capture = registry_capture(factory);
    let tool = captured_tool(&capture, "lookup");
    (capture, tool)
}

fn captured_tool(capture: &McpRegistryCapture, name: &str) -> Arc<dyn Tool> {
    capture
        .tools()
        .iter()
        .find(|tool| tool.descriptor().name == name)
        .cloned()
        .unwrap_or_else(|| panic!("captured {name} tool"))
}
fn prepare_lookup(tool: &Arc<dyn Tool>) -> PreparedCall {
    let resolver = ego_core::permit::fake::FakeFs::default();
    let ctx = PrepareCtx {
        cwd: CanonPath::new("/fixture"),
        resolver: &resolver,
    };
    tool.prepare(json!({}), &ctx).expect("prepare")
}
fn authorized(call: PreparedCall) -> Authorized {
    let tool_name = call
        .mcp_envelope()
        .expect("MCP envelope")
        .subject()
        .operation_name()
        .to_owned();
    let policy = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Yolo,
        Ruleset::from_layer(
            PermCfg {
                judge: None,
                allow: vec![
                    ego_core::permit::rule::Rule::load("tool(server/lookup)").expect("tool rule"),
                    ego_core::permit::rule::Rule::load("tool(server/lookup-secondary)")
                        .expect("secondary tool rule"),
                    ego_core::permit::rule::Rule::load("exec(mcp-server *)").expect("process rule"),
                ],
                deny: Vec::new(),
            },
            Source::Project,
        ),
    );
    let Classification::Settled(Ok(authorized)) = policy.classify(call) else {
        panic!("{tool_name} call must be authorized")
    };
    authorized
}
fn mapped_input() -> ego_mcp::McpMappedInput {
    McpInputMapping::map(
        McpInputRequired::elicitation(SCHEMA, "answer", b"opaque-state").expect("elicitation"),
    )
    .expect("mapped input")
}
fn prepared_resume() -> (ResumeSealer, ego_core::suspension::ResumeFloor) {
    let policy = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Yolo, Ruleset::default());
    let floor = policy.resume_floor();
    let sealer =
        ResumeSealer::new(Arc::new(FixedResumeKey), Arc::new(FixedClock), 30_000).expect("sealer");
    (sealer, floor)
}
fn ports(spawner: Arc<ScriptedSpawner>) -> ToolEffectPorts<'static> {
    let fs: &'static ego_core::permit::fake::FakeFs =
        Box::leak(Box::new(ego_core::permit::fake::FakeFs::default()));
    let process: &'static NoProcess = Box::leak(Box::new(NoProcess));
    let evidence: &'static EmptyEvidence = Box::leak(Box::new(EmptyEvidence));
    ToolEffectPorts::new(
        fs,
        process,
        evidence,
        Arc::new(OfflineResolver),
        Arc::new(OfflineOneHop),
        spawner,
    )
}

async fn attempt_initial_record(
    journal: Arc<Journal>,
    policy_captures: Arc<CaptureStore>,
    resolver: Arc<ExactCaptureResolver>,
    mode: HookMode,
    trace: Trace,
) -> (
    Result<ForeignInputRequired, McpError>,
    std::sync::Weak<GenerationPolicySnapshot>,
    GenerationPolicyDigest,
) {
    let owner =
        context_free_owner_with_store(journal, Arc::clone(&policy_captures), resolver.clone());
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(factory);
    resolver.insert(capture.clone());
    let call = prepare_lookup(&tool);
    let envelope = call.mcp_envelope().expect("MCP envelope").clone();
    let auth = authorized(call);
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor).expect("resume");
    let snapshot = GenerationPolicyCapture::new(
        CanonPath::new("/transaction-policy"),
        Mode::Auto,
        Ruleset::default(),
        None,
    )
    .expect("generation policy capture")
    .snapshot();
    let digest = snapshot.digest();
    let weak = Arc::downgrade(&snapshot);
    let guarded = guarded_resume_with_session_context(
        resume,
        hooks_with_mode(mode, trace),
        &capture,
        Arc::clone(&snapshot),
    );
    drop(snapshot);
    let result = owner
        .record_input_required(envelope, mapped_input(), guarded)
        .await;
    (result, weak, digest)
}

#[tokio::test(flavor = "current_thread")]
async fn real_operation_records_before_elicitation_and_returns_foreign_id() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner_with_store(
        journal.clone(),
        Arc::clone(&policy_captures),
        resolver.clone(),
    );
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(Arc::clone(&factory));
    resolver.insert(capture.clone());
    let call = prepare_lookup(&tool);
    let auth = authorized(call);
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());
    let policy_snapshot = GenerationPolicyCapture::new(
        CanonPath::new("/recorded-policy"),
        Mode::Auto,
        Ruleset::default(),
        None,
    )
    .expect("generation policy capture")
    .snapshot();
    let policy_digest = policy_snapshot.digest();
    let (call, guarded) = guarded_with_session_context_and_policy_snapshot(
        auth,
        resume,
        hooks_with_mode(HookMode::Continue, trace.clone()),
        &capture,
        Arc::clone(&policy_snapshot),
        spawner.clone(),
    );
    let outcome = tool.execute(call, &guarded).await.expect("outcome");
    let ToolExecution::InputRequired(foreign) = outcome else {
        panic!("expected input-required")
    };
    assert_eq!(foreign.continuation_id(), journal.id());
    let stored = policy_captures
        .stored_snapshot(foreign.continuation_id(), &policy_digest)
        .expect("generation policy stored before question exposure");
    assert!(Arc::ptr_eq(&stored, &policy_snapshot));
    assert_eq!(stored.digest(), journal.recorded_policy_digest());
    assert_eq!(journal.events(), vec!["record"]);
    assert_eq!(trace.events(), vec!["store", "record", "Elicitation"]);
    assert_eq!(
        policy_captures.calls(),
        vec![(
            GenerationPolicyCaptureStoreOperation::Store,
            foreign.continuation_id().to_owned(),
            policy_digest,
        )]
    );
    assert_eq!(spawner.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn restarted_owner_loads_policy_before_binding_exact_capture_and_accepting() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let (recorded, _policy_weak, digest) = attempt_initial_record(
        Arc::clone(&journal),
        Arc::clone(&policy_captures),
        Arc::clone(&resolver),
        HookMode::Continue,
        trace.clone(),
    )
    .await;
    let required = recorded.expect("initial input-required");
    let id = required.continuation_id().to_owned();
    let stable_capture = journal.recorded_capture();
    drop(
        resolver
            .remove(&stable_capture)
            .expect("remove the prior process capture"),
    );

    let restarted = context_free_owner_with_store(
        journal.clone(),
        Arc::clone(&policy_captures),
        Arc::clone(&resolver),
    );
    let resolve_calls_before_load = resolver.calls();
    let stored = policy_captures
        .stored_snapshot(&id, &digest)
        .expect("stored generation policy");
    let loaded = restarted
        .load_pending_foreign_policy(&id)
        .expect("load pending generation policy before reconciliation");
    assert!(Arc::ptr_eq(&loaded, &stored));
    assert_eq!(loaded.digest(), digest);
    assert_eq!(
        resolver.calls(),
        resolve_calls_before_load,
        "phase one must not consult the registry capture resolver"
    );
    assert_eq!(
        policy_captures.calls(),
        vec![
            (
                GenerationPolicyCaptureStoreOperation::Store,
                id.clone(),
                digest.clone(),
            ),
            (
                GenerationPolicyCaptureStoreOperation::Load,
                id.clone(),
                digest.clone(),
            ),
        ]
    );
    assert_eq!(
        trace.events(),
        vec!["store", "record", "Elicitation", "load"]
    );

    let restarted_factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&restarted), 64 * 1024, Duration::from_secs(30))
            .expect("restarted factory"),
    );
    let (reconciled_capture, _tool) = captured_lookup(restarted_factory);
    assert_eq!(reconciled_capture.stable_fingerprint(), stable_capture);
    resolver.insert(reconciled_capture);
    restarted
        .bind_pending_foreign_continuation(&id, Arc::clone(&loaded))
        .expect("bind the reconciled exact capture");

    let runtime = hooks_with_mode(HookMode::Continue, trace.clone());
    let causality = settled_continuation_causality(&runtime, &required, r#"{"choice":"yes"}"#);
    let spawner = Arc::new(ScriptedSpawner::with_input_required_rounds(0));
    let execution = restarted
        .accept_answer(
            &id,
            br#"{"choice":"yes"}"#,
            runtime,
            causality,
            prepared_resume().1,
            ports(spawner.clone()),
        )
        .await
        .expect("accept with the recovered authorities");
    assert!(matches!(execution, ToolExecution::Complete(_)));
    assert_eq!(spawner.calls(), 1);
    assert_eq!(journal.state_for(&id), Some(ContinuationState::Completed));
    assert_eq!(
        trace.events(),
        vec![
            "store",
            "record",
            "Elicitation",
            "load",
            "accepted_answer",
            "ElicitationResult",
            "claim",
            "settle",
            "delete",
        ]
    );
    assert_eq!(
        policy_captures.calls(),
        vec![
            (
                GenerationPolicyCaptureStoreOperation::Store,
                id.clone(),
                digest.clone(),
            ),
            (
                GenerationPolicyCaptureStoreOperation::Load,
                id.clone(),
                digest.clone(),
            ),
            (GenerationPolicyCaptureStoreOperation::Delete, id, digest,),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pending_policy_load_failures_are_closed_and_do_not_advance_authority() {
    for kind in [
        GenerationPolicyCaptureStoreErrorKind::Missing,
        GenerationPolicyCaptureStoreErrorKind::Corrupt,
        GenerationPolicyCaptureStoreErrorKind::DigestMismatch,
        GenerationPolicyCaptureStoreErrorKind::CapacityExceeded,
        GenerationPolicyCaptureStoreErrorKind::Unavailable,
    ] {
        let trace = Trace::default();
        let journal = Arc::new(Journal::with_trace(trace.clone()));
        let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
        let resolver = Arc::new(ExactCaptureResolver::default());
        let (recorded, _policy_weak, digest) = attempt_initial_record(
            Arc::clone(&journal),
            Arc::clone(&policy_captures),
            Arc::clone(&resolver),
            HookMode::Continue,
            trace.clone(),
        )
        .await;
        let id = recorded
            .expect("initial input-required")
            .continuation_id()
            .to_owned();
        policy_captures.fail_next_load(kind);
        let restarted =
            context_free_owner_with_store(journal.clone(), Arc::clone(&policy_captures), resolver);

        let Err(error) = restarted.load_pending_foreign_policy(&id) else {
            panic!("{kind:?} must fail closed");
        };
        assert_eq!(error.failure(), McpFailure::Authority, "{kind:?}");
        assert_eq!(error.delivery(), None, "{kind:?}");
        let diagnostic = error.to_string();
        let digest_text = String::from(digest.clone());
        assert!(!diagnostic.contains(&id), "{kind:?}");
        assert!(!diagnostic.contains(&digest_text), "{kind:?}");
        assert_eq!(journal.state_for(&id), Some(ContinuationState::Pending));
        assert_eq!(journal.events(), vec!["record"]);
        assert_eq!(
            trace.events(),
            vec!["store", "record", "Elicitation", "load"]
        );
        assert_eq!(
            policy_captures
                .calls()
                .into_iter()
                .map(|(operation, _, _)| operation)
                .collect::<Vec<_>>(),
            vec![
                GenerationPolicyCaptureStoreOperation::Store,
                GenerationPolicyCaptureStoreOperation::Load,
            ],
            "{kind:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pending_policy_load_rejects_unknown_and_non_pending_records_before_store_access() {
    let empty_journal = Arc::new(Journal::default());
    let empty_store = Arc::new(CaptureStore::default());
    let empty_owner = context_free_owner_with_store(
        empty_journal.clone(),
        Arc::clone(&empty_store),
        Arc::new(ExactCaptureResolver::default()),
    );
    let Err(unknown) = empty_owner.load_pending_foreign_policy("unknown-continuation") else {
        panic!("an unknown continuation must fail closed");
    };
    assert_eq!(unknown.failure(), McpFailure::Authority);
    assert_eq!(unknown.delivery(), None);
    assert!(empty_journal.events().is_empty());
    assert!(empty_store.calls().is_empty());

    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let (recorded, _policy_weak, _digest) = attempt_initial_record(
        Arc::clone(&journal),
        Arc::clone(&policy_captures),
        Arc::clone(&resolver),
        HookMode::Continue,
        trace.clone(),
    )
    .await;
    let id = recorded
        .expect("initial input-required")
        .continuation_id()
        .to_owned();
    journal
        .fail_pending(&id)
        .expect("make the continuation non-pending");
    let restarted =
        context_free_owner_with_store(journal.clone(), policy_captures.clone(), resolver);

    let Err(non_pending) = restarted.load_pending_foreign_policy(&id) else {
        panic!("a non-pending continuation must fail closed");
    };
    assert_eq!(non_pending.failure(), McpFailure::Authority);
    assert_eq!(non_pending.delivery(), None);
    assert_eq!(journal.state_for(&id), Some(ContinuationState::Failed));
    assert_eq!(journal.events(), vec!["record", "fail_pending"]);
    assert_eq!(
        policy_captures
            .calls()
            .into_iter()
            .map(|(operation, _, _)| operation)
            .collect::<Vec<_>>(),
        vec![GenerationPolicyCaptureStoreOperation::Store]
    );
    assert_eq!(
        trace.events(),
        vec!["store", "record", "Elicitation", "fail_pending"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pending_policy_load_revalidates_durable_authority_after_store_read() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let (recorded, _policy_weak, digest) = attempt_initial_record(
        Arc::clone(&journal),
        Arc::clone(&policy_captures),
        Arc::clone(&resolver),
        HookMode::Continue,
        trace.clone(),
    )
    .await;
    let id = recorded
        .expect("initial input-required")
        .continuation_id()
        .to_owned();
    let changed_digest = GenerationPolicyCapture::new(
        CanonPath::new("/changed-during-policy-load"),
        Mode::Default,
        Ruleset::default(),
        None,
    )
    .expect("changed generation policy")
    .snapshot()
    .digest();
    assert_ne!(changed_digest, digest);
    let callback_journal = Arc::clone(&journal);
    let callback_id = id.clone();
    let callback_digest = changed_digest.clone();
    policy_captures.after_next_load(move || {
        callback_journal.replace_policy_digest(&callback_id, &callback_digest);
    });
    let restarted = context_free_owner_with_store(
        journal.clone(),
        Arc::clone(&policy_captures),
        Arc::clone(&resolver),
    );
    let resolve_calls_before_load = resolver.calls();

    let Err(error) = restarted.load_pending_foreign_policy(&id) else {
        panic!("a durable authority change during policy load must fail closed");
    };
    assert_eq!(error.failure(), McpFailure::Authority);
    assert_eq!(error.delivery(), None);
    assert_eq!(resolver.calls(), resolve_calls_before_load);
    assert_eq!(journal.recorded_policy_digest(), changed_digest);
    assert_eq!(journal.state_for(&id), Some(ContinuationState::Pending));
    assert_eq!(journal.events(), vec!["record"]);
    assert_eq!(
        trace.events(),
        vec!["store", "record", "Elicitation", "load"]
    );
    assert_eq!(
        policy_captures
            .calls()
            .into_iter()
            .map(|(operation, _, _)| operation)
            .collect::<Vec<_>>(),
        vec![
            GenerationPolicyCaptureStoreOperation::Store,
            GenerationPolicyCaptureStoreOperation::Load,
        ]
    );

    journal.replace_policy_digest(&id, &digest);
    let stored = policy_captures
        .stored_snapshot(&id, &digest)
        .expect("stored generation policy");
    restarted
        .bind_pending_foreign_continuation(&id, stored)
        .expect("failed phase one must not leave a hidden pin");
    assert_eq!(resolver.calls(), resolve_calls_before_load + 1);
}

#[tokio::test(flavor = "current_thread")]
async fn pending_bind_rejects_changed_or_unavailable_authority_and_never_replaces_a_pin() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let (recorded, _policy_weak, digest) = attempt_initial_record(
        Arc::clone(&journal),
        Arc::clone(&policy_captures),
        Arc::clone(&resolver),
        HookMode::Continue,
        trace.clone(),
    )
    .await;
    let id = recorded
        .expect("initial input-required")
        .continuation_id()
        .to_owned();
    let stable_capture = journal.recorded_capture();
    let restarted = context_free_owner_with_store(
        journal.clone(),
        Arc::clone(&policy_captures),
        Arc::clone(&resolver),
    );
    let loaded = restarted
        .load_pending_foreign_policy(&id)
        .expect("load pending generation policy");

    let different_snapshot = GenerationPolicyCapture::new(
        CanonPath::new("/wrong-recovered-policy"),
        Mode::Auto,
        Ruleset::default(),
        None,
    )
    .expect("different generation policy")
    .snapshot();
    let Err(mismatched_snapshot) =
        restarted.bind_pending_foreign_continuation(&id, different_snapshot)
    else {
        panic!("a supplied snapshot with another digest must be refused");
    };
    assert_eq!(mismatched_snapshot.failure(), McpFailure::Authority);
    assert_eq!(mismatched_snapshot.delivery(), None);

    drop(
        resolver
            .remove(&stable_capture)
            .expect("remove the recorded registry capture"),
    );
    let Err(unavailable_capture) =
        restarted.bind_pending_foreign_continuation(&id, Arc::clone(&loaded))
    else {
        panic!("an unavailable recorded registry capture must be refused");
    };
    assert_eq!(unavailable_capture.failure(), McpFailure::Authority);
    assert_eq!(unavailable_capture.delivery(), None);

    let restarted_factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&restarted), 64 * 1024, Duration::from_secs(30))
            .expect("restarted factory"),
    );
    let wrong_capture = registry_capture_with_schema(
        Arc::clone(&restarted_factory),
        json!({
            "type":"object",
            "properties":{"wrong":{"type":"string"}},
            "required":["wrong"],
            "additionalProperties":false
        }),
    );
    assert_ne!(wrong_capture.stable_fingerprint(), stable_capture);
    resolver.insert_for(&stable_capture, wrong_capture);
    let Err(wrong_capture) = restarted.bind_pending_foreign_continuation(&id, Arc::clone(&loaded))
    else {
        panic!("a resolver result with another capture identity must be refused");
    };
    assert_eq!(wrong_capture.failure(), McpFailure::Authority);
    assert_eq!(wrong_capture.delivery(), None);
    drop(
        resolver
            .remove(&stable_capture)
            .expect("remove the wrong registry capture"),
    );

    let (exact_capture, _tool) = captured_lookup(restarted_factory);
    assert_eq!(exact_capture.stable_fingerprint(), stable_capture);
    resolver.insert(exact_capture);
    let changed_digest = GenerationPolicyCapture::new(
        CanonPath::new("/changed-before-bind"),
        Mode::Default,
        Ruleset::default(),
        None,
    )
    .expect("changed generation policy")
    .snapshot()
    .digest();
    assert_ne!(changed_digest, digest);
    journal.replace_policy_digest(&id, &changed_digest);
    let Err(changed_record) = restarted.bind_pending_foreign_continuation(&id, Arc::clone(&loaded))
    else {
        panic!("a changed durable digest must be refused");
    };
    assert_eq!(changed_record.failure(), McpFailure::Authority);
    assert_eq!(changed_record.delivery(), None);
    journal.replace_policy_digest(&id, &digest);

    let first_bytes = loaded.encode_confidential();
    let first = Arc::new(
        GenerationPolicySnapshot::decode_confidential(&first_bytes)
            .expect("first equivalent snapshot"),
    );
    let first_weak = Arc::downgrade(&first);
    restarted
        .bind_pending_foreign_continuation(&id, Arc::clone(&first))
        .expect("first exact bind");
    drop(first);
    let replacement_bytes = loaded.encode_confidential();
    let replacement = Arc::new(
        GenerationPolicySnapshot::decode_confidential(&replacement_bytes)
            .expect("replacement equivalent snapshot"),
    );
    let replacement_weak = Arc::downgrade(&replacement);
    let Err(duplicate) = restarted.bind_pending_foreign_continuation(&id, Arc::clone(&replacement))
    else {
        panic!("an existing pin must never be replaced");
    };
    assert_eq!(duplicate.failure(), McpFailure::Authority);
    assert_eq!(duplicate.delivery(), None);
    drop(replacement);
    assert!(
        first_weak.upgrade().is_some(),
        "the original exact pin must remain retained"
    );
    assert!(
        replacement_weak.upgrade().is_none(),
        "a duplicate supplied snapshot must not replace the pin"
    );
    assert_eq!(journal.state_for(&id), Some(ContinuationState::Pending));
    assert_eq!(journal.events(), vec!["record"]);
    assert_eq!(
        trace.events(),
        vec!["store", "record", "Elicitation", "load"]
    );
    assert_eq!(
        policy_captures
            .calls()
            .into_iter()
            .map(|(operation, _, _)| operation)
            .collect::<Vec<_>>(),
        vec![
            GenerationPolicyCaptureStoreOperation::Store,
            GenerationPolicyCaptureStoreOperation::Load,
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pending_bind_revalidates_durable_authority_after_capture_read() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let (recorded, _policy_weak, digest) = attempt_initial_record(
        Arc::clone(&journal),
        Arc::clone(&policy_captures),
        Arc::clone(&resolver),
        HookMode::Continue,
        trace.clone(),
    )
    .await;
    let id = recorded
        .expect("initial input-required")
        .continuation_id()
        .to_owned();
    let restarted = context_free_owner_with_store(
        journal.clone(),
        Arc::clone(&policy_captures),
        Arc::clone(&resolver),
    );
    let loaded = restarted
        .load_pending_foreign_policy(&id)
        .expect("load pending generation policy");
    let capture_identity = journal.recorded_capture();
    let changed_capture = "changed-during-capture-resolution";
    assert_ne!(changed_capture, capture_identity);
    let callback_journal = Arc::clone(&journal);
    let callback_id = id.clone();
    resolver.after_next_resolve(move || {
        callback_journal.replace_capture_identity(&callback_id, changed_capture);
    });
    let resolve_calls_before_bind = resolver.calls();

    let Err(error) = restarted.bind_pending_foreign_continuation(&id, Arc::clone(&loaded)) else {
        panic!("a durable authority change during capture resolution must fail closed");
    };
    assert_eq!(error.failure(), McpFailure::Authority);
    assert_eq!(error.delivery(), None);
    assert_eq!(resolver.calls(), resolve_calls_before_bind + 1);
    assert_eq!(journal.recorded_policy_digest(), digest);
    assert_eq!(journal.recorded_capture(), changed_capture);
    assert_eq!(journal.state_for(&id), Some(ContinuationState::Pending));
    assert_eq!(journal.events(), vec!["record"]);
    assert_eq!(
        trace.events(),
        vec!["store", "record", "Elicitation", "load"]
    );
    assert_eq!(
        policy_captures
            .calls()
            .into_iter()
            .map(|(operation, _, _)| operation)
            .collect::<Vec<_>>(),
        vec![
            GenerationPolicyCaptureStoreOperation::Store,
            GenerationPolicyCaptureStoreOperation::Load,
        ]
    );

    journal.replace_capture_identity(&id, &capture_identity);
    restarted
        .bind_pending_foreign_continuation(&id, loaded)
        .expect("failed phase two must not leave a hidden pin");
    assert_eq!(resolver.calls(), resolve_calls_before_bind + 2);
}

#[tokio::test(flavor = "current_thread")]
async fn pending_digest_mismatch_is_refused_before_acceptance_or_effects() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner_with_store(
        journal.clone(),
        Arc::clone(&policy_captures),
        resolver.clone(),
    );
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(factory);
    resolver.insert(capture.clone());
    let auth = authorized(prepare_lookup(&tool));
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());
    let runtime = hooks_with_mode(HookMode::Continue, trace.clone());
    let policy_snapshot = GenerationPolicyCapture::new(
        CanonPath::new("/pinned-policy"),
        Mode::Default,
        Ruleset::default(),
        None,
    )
    .expect("generation policy capture")
    .snapshot();
    let policy_digest = policy_snapshot.digest();
    let policy_weak = Arc::downgrade(&policy_snapshot);
    let (call, guarded) = guarded_with_session_context_and_policy_snapshot(
        auth,
        resume,
        runtime.clone(),
        &capture,
        Arc::clone(&policy_snapshot),
        spawner.clone(),
    );
    let ToolExecution::InputRequired(required) =
        tool.execute(call, &guarded).await.expect("required")
    else {
        panic!("expected input-required");
    };
    let id = required.continuation_id().to_owned();
    let different_digest = GenerationPolicyCapture::new(
        CanonPath::new("/different-policy"),
        Mode::Auto,
        Ruleset::default(),
        None,
    )
    .expect("different generation policy capture")
    .snapshot()
    .digest();
    assert_ne!(different_digest, policy_digest);
    journal.replace_policy_digest(&id, &different_digest);
    drop(policy_snapshot);
    drop(guarded);

    let causality = settled_continuation_causality(&runtime, &required, r#"{"choice":"yes"}"#);
    let result = owner
        .accept_answer(
            &id,
            br#"{"choice":"yes"}"#,
            runtime,
            causality,
            floor,
            ports(spawner.clone()),
        )
        .await;

    let Err(error) = result else {
        panic!("a record whose digest differs from its pinned snapshot must be refused");
    };
    assert_eq!(error.failure(), McpFailure::Authority);
    assert_eq!(error.delivery(), None);
    assert_eq!(journal.state_for(&id), Some(ContinuationState::Pending));
    assert_eq!(journal.events(), vec!["record"]);
    assert_eq!(trace.events(), vec!["store", "record", "Elicitation"]);
    assert_eq!(spawner.calls(), 1, "the resumed effect must not run");
    assert!(policy_captures.contains(&id, &policy_digest));
    assert_eq!(policy_weak.strong_count(), 2);
    assert_eq!(policy_captures.calls().len(), 1, "delete must not run");
}

#[tokio::test(flavor = "current_thread")]
async fn capture_store_failure_precedes_record_elicitation_and_question_exposure() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    policy_captures.fail_next_store();
    let (result, policy_weak, _digest) = attempt_initial_record(
        Arc::clone(&journal),
        Arc::clone(&policy_captures),
        Arc::new(ExactCaptureResolver::default()),
        HookMode::Continue,
        trace.clone(),
    )
    .await;

    let Err(error) = result else {
        panic!("capture-store failure must not expose a foreign question");
    };
    assert_eq!(error.failure(), McpFailure::Authority);
    assert_eq!(error.delivery(), None);
    assert!(journal.events().is_empty());
    assert_eq!(journal.state(), None);
    assert_eq!(trace.events(), vec!["store"]);
    assert_eq!(policy_captures.calls().len(), 1);
    assert_eq!(
        policy_captures.calls()[0].0,
        GenerationPolicyCaptureStoreOperation::Store
    );
    assert!(
        policy_weak.upgrade().is_none(),
        "a failed store must release the local policy pin"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn journal_record_failure_retains_orphan_capture_but_releases_local_pin() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    journal.fail_next_record();
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let (result, policy_weak, digest) = attempt_initial_record(
        Arc::clone(&journal),
        Arc::clone(&policy_captures),
        Arc::new(ExactCaptureResolver::default()),
        HookMode::Continue,
        trace.clone(),
    )
    .await;

    let Err(error) = result else {
        panic!("journal failure must not expose a foreign question");
    };
    assert_eq!(error.failure(), McpFailure::Authority);
    assert_eq!(error.delivery(), None);
    assert!(
        error
            .to_string()
            .contains("scripted continuation record failure")
    );
    assert_eq!(journal.state(), None);
    assert_eq!(trace.events(), vec!["store", "record"]);
    let calls = policy_captures.calls();
    let [(GenerationPolicyCaptureStoreOperation::Store, id, stored_digest)] = calls.as_slice()
    else {
        panic!("only the policy store may run before journal failure");
    };
    assert_eq!(stored_digest, &digest);
    assert!(policy_captures.contains(id, &digest));
    policy_captures.evict(id, &digest);
    assert!(
        policy_weak.upgrade().is_none(),
        "after orphan eviction no in-memory pin may survive journal failure"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn elicitation_failure_terminalizes_pending_before_releasing_and_deleting_capture() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let (result, policy_weak, digest) = attempt_initial_record(
        Arc::clone(&journal),
        Arc::clone(&policy_captures),
        Arc::new(ExactCaptureResolver::default()),
        HookMode::BlockElicitation,
        trace.clone(),
    )
    .await;

    let Err(error) = result else {
        panic!("blocked Elicitation must not expose a foreign question");
    };
    assert_eq!(error.failure(), McpFailure::Authority);
    assert_eq!(error.delivery(), None);
    assert!(error.to_string().contains("Elicitation hook blocked"));
    assert_eq!(journal.state(), Some(ContinuationState::Failed));
    assert_eq!(journal.events(), vec!["record", "fail_pending"]);
    assert_eq!(
        trace.events(),
        vec!["store", "record", "Elicitation", "fail_pending", "delete"]
    );
    let id = journal.id();
    assert!(!policy_captures.contains(&id, &digest));
    assert_eq!(
        policy_captures.calls(),
        vec![
            (
                GenerationPolicyCaptureStoreOperation::Store,
                id.clone(),
                digest.clone(),
            ),
            (GenerationPolicyCaptureStoreOperation::Delete, id, digest,),
        ]
    );
    assert!(
        policy_weak.upgrade().is_none(),
        "durable failure cleanup must release both policy authorities"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_pending_terminalization_retains_pin_and_capture_without_delete() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    journal.fail_next_pending_failure();
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let (result, policy_weak, digest) = attempt_initial_record(
        Arc::clone(&journal),
        Arc::clone(&policy_captures),
        Arc::new(ExactCaptureResolver::default()),
        HookMode::BlockElicitation,
        trace.clone(),
    )
    .await;

    let Err(error) = result else {
        panic!("failed rollback must not expose a foreign question");
    };
    assert_eq!(error.failure(), McpFailure::Authority);
    assert_eq!(error.delivery(), None);
    assert_eq!(journal.state(), Some(ContinuationState::Pending));
    assert_eq!(journal.events(), vec!["record", "fail_pending"]);
    assert_eq!(
        trace.events(),
        vec!["store", "record", "Elicitation", "fail_pending"]
    );
    let id = journal.id();
    assert!(policy_captures.contains(&id, &digest));
    assert_eq!(
        policy_weak.strong_count(),
        2,
        "the store and in-memory pin must both retain the exact snapshot"
    );
    assert_eq!(policy_captures.calls().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_answer_validates_hooks_claims_and_retries_once() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner_with_store(
        journal.clone(),
        Arc::clone(&policy_captures),
        resolver.clone(),
    );
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(Arc::clone(&factory));
    resolver.insert(capture.clone());
    let call = prepare_lookup(&tool);
    let auth = authorized(call);
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());
    let runtime = hooks_with_mode(HookMode::Continue, trace.clone());
    let policy_snapshot = GenerationPolicyCapture::new(
        CanonPath::new("/completed-policy"),
        Mode::Default,
        Ruleset::default(),
        None,
    )
    .expect("generation policy capture")
    .snapshot();
    let policy_digest = policy_snapshot.digest();
    let policy_weak = Arc::downgrade(&policy_snapshot);
    let (call, guarded) = guarded_with_session_context_and_policy_snapshot(
        auth,
        resume,
        runtime.clone(),
        &capture,
        Arc::clone(&policy_snapshot),
        spawner.clone(),
    );
    let ToolExecution::InputRequired(foreign) =
        tool.execute(call, &guarded).await.expect("required")
    else {
        panic!("expected required")
    };
    let id = foreign.continuation_id().to_owned();
    drop(policy_snapshot);
    drop(guarded);
    let causality = settled_continuation_causality(&runtime, &foreign, r#"{"choice":"yes"}"#);
    let retried = owner
        .accept_answer(
            &id,
            br#"{"choice":"yes"}"#,
            runtime,
            causality,
            floor,
            ports(spawner.clone()),
        )
        .await
        .expect("resume");
    assert!(matches!(retried, ToolExecution::Complete(_)));
    assert_eq!(
        journal.events(),
        vec!["record", "accepted_answer", "claim", "settle"]
    );
    assert_eq!(
        trace.events(),
        vec![
            "store",
            "record",
            "Elicitation",
            "accepted_answer",
            "ElicitationResult",
            "claim",
            "settle",
            "delete"
        ]
    );
    assert_eq!(spawner.calls(), 2);
    assert_eq!(journal.state(), Some(ContinuationState::Completed));
    assert!(!policy_captures.contains(&id, &policy_digest));
    assert_eq!(
        policy_captures.calls(),
        vec![
            (
                GenerationPolicyCaptureStoreOperation::Store,
                id.clone(),
                policy_digest.clone(),
            ),
            (
                GenerationPolicyCaptureStoreOperation::Delete,
                id,
                policy_digest,
            ),
        ]
    );
    assert!(
        policy_weak.upgrade().is_none(),
        "completed settlement must precede release of both policy authorities"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn completed_effect_settlement_failure_retains_both_authorities_and_delivery() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    journal.fail_next_settlement();
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner_with_store(
        journal.clone(),
        Arc::clone(&policy_captures),
        resolver.clone(),
    );
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(factory);
    resolver.insert(capture.clone());
    let auth = authorized(prepare_lookup(&tool));
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());
    let runtime = hooks_with_mode(HookMode::Continue, trace.clone());
    let policy_snapshot = GenerationPolicyCapture::new(
        CanonPath::new("/failed-settlement-policy"),
        Mode::Default,
        Ruleset::default(),
        None,
    )
    .expect("generation policy capture")
    .snapshot();
    let digest = policy_snapshot.digest();
    let weak = Arc::downgrade(&policy_snapshot);
    let (call, guarded) = guarded_with_session_context_and_policy_snapshot(
        auth,
        resume,
        runtime.clone(),
        &capture,
        Arc::clone(&policy_snapshot),
        spawner.clone(),
    );
    let ToolExecution::InputRequired(required) =
        tool.execute(call, &guarded).await.expect("required")
    else {
        panic!("expected input-required");
    };
    let id = required.continuation_id().to_owned();
    drop(policy_snapshot);
    drop(guarded);
    let causality = settled_continuation_causality(&runtime, &required, r#"{"choice":"yes"}"#);
    let result = owner
        .accept_answer(
            &id,
            br#"{"choice":"yes"}"#,
            runtime,
            causality,
            floor,
            ports(spawner),
        )
        .await;

    let Err(error) = result else {
        panic!("failed durable settlement must take precedence over completion");
    };
    assert_eq!(error.failure(), McpFailure::Authority);
    assert_eq!(error.delivery(), Some(DeliveryCertainty::DefinitelySent));
    assert_eq!(journal.state_for(&id), Some(ContinuationState::Claimed));
    assert_eq!(
        trace.events(),
        vec![
            "store",
            "record",
            "Elicitation",
            "accepted_answer",
            "ElicitationResult",
            "claim",
            "settle",
        ]
    );
    assert!(policy_captures.contains(&id, &digest));
    assert_eq!(weak.strong_count(), 2);
    assert_eq!(policy_captures.calls().len(), 1, "delete must not run");
}

#[tokio::test(flavor = "current_thread")]
async fn effect_failure_settlement_failure_retains_both_authorities_and_delivery() {
    let cases = [
        ("definitely unsent", DeliveryCertainty::DefinitelyUnsent),
        ("possibly sent", DeliveryCertainty::PossiblySent),
    ];

    for (label, expected_delivery) in cases {
        let trace = Trace::default();
        let journal = Arc::new(Journal::with_trace(trace.clone()));
        journal.fail_next_settlement();
        let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
        let resolver = Arc::new(ExactCaptureResolver::default());
        let owner = context_free_owner_with_store(
            journal.clone(),
            Arc::clone(&policy_captures),
            resolver.clone(),
        );
        let factory = Arc::new(
            McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
                .expect("factory"),
        );
        let (capture, tool) = captured_lookup(factory);
        resolver.insert(capture.clone());
        let auth = authorized(prepare_lookup(&tool));
        let (sealer, floor) = prepared_resume();
        let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
        let spawner = Arc::new(ScriptedSpawner::with_resume_transport_failure(
            expected_delivery,
        ));
        let runtime = hooks_with_mode(HookMode::Continue, trace.clone());
        let policy_snapshot = GenerationPolicyCapture::new(
            CanonPath::new(format!("/effect-settlement-failure-{label}")),
            Mode::Default,
            Ruleset::default(),
            None,
        )
        .expect("generation policy capture")
        .snapshot();
        let digest = policy_snapshot.digest();
        let weak = Arc::downgrade(&policy_snapshot);
        let (call, guarded) = guarded_with_session_context_and_policy_snapshot(
            auth,
            resume,
            runtime.clone(),
            &capture,
            Arc::clone(&policy_snapshot),
            Arc::clone(&spawner),
        );
        let ToolExecution::InputRequired(required) =
            tool.execute(call, &guarded).await.expect("required")
        else {
            panic!("expected input-required for {label}");
        };
        let id = required.continuation_id().to_owned();
        drop(policy_snapshot);
        drop(guarded);
        let causality = settled_continuation_causality(&runtime, &required, r#"{"choice":"yes"}"#);
        let result = owner
            .accept_answer(
                &id,
                br#"{"choice":"yes"}"#,
                runtime,
                causality,
                floor,
                ports(Arc::clone(&spawner)),
            )
            .await;

        let Err(error) = result else {
            panic!("{label} settlement failure must take precedence");
        };
        assert_eq!(error.failure(), McpFailure::Authority, "{label}");
        assert_eq!(error.delivery(), Some(expected_delivery), "{label}");
        assert_eq!(
            error.to_string(),
            "MCP continuation terminal state could not be durably recorded",
            "{label}"
        );
        assert_eq!(
            journal.state_for(&id),
            Some(ContinuationState::Claimed),
            "{label}"
        );
        assert_eq!(
            trace.events(),
            vec![
                "store",
                "record",
                "Elicitation",
                "accepted_answer",
                "ElicitationResult",
                "claim",
                "settle",
            ],
            "{label}"
        );
        assert_eq!(spawner.calls(), 2, "{label}");
        assert!(policy_captures.contains(&id, &digest), "{label}");
        assert_eq!(weak.strong_count(), 2, "{label}");
        assert_eq!(
            policy_captures.calls().len(),
            1,
            "{label}: delete must not run"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn effect_failure_delivery_selects_durable_terminal_state_before_cleanup() {
    let cases = [
        (
            "definitely unsent",
            Arc::new(ScriptedSpawner::with_resume_transport_failure(
                DeliveryCertainty::DefinitelyUnsent,
            )),
            ContinuationState::Failed,
            DeliveryCertainty::DefinitelyUnsent,
        ),
        (
            "possibly sent",
            Arc::new(ScriptedSpawner::with_resume_transport_failure(
                DeliveryCertainty::PossiblySent,
            )),
            ContinuationState::Uncertain,
            DeliveryCertainty::PossiblySent,
        ),
    ];

    for (label, spawner, expected_state, expected_delivery) in cases {
        let trace = Trace::default();
        let journal = Arc::new(Journal::with_trace(trace.clone()));
        let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
        let resolver = Arc::new(ExactCaptureResolver::default());
        let owner = context_free_owner_with_store(
            journal.clone(),
            Arc::clone(&policy_captures),
            resolver.clone(),
        );
        let factory = Arc::new(
            McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
                .expect("factory"),
        );
        let (capture, tool) = captured_lookup(factory);
        resolver.insert(capture.clone());
        let auth = authorized(prepare_lookup(&tool));
        let (sealer, floor) = prepared_resume();
        let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
        let runtime = hooks_with_mode(HookMode::Continue, trace.clone());
        let policy_snapshot = GenerationPolicyCapture::new(
            CanonPath::new(format!("/effect-failure-{label}")),
            Mode::Default,
            Ruleset::default(),
            None,
        )
        .expect("generation policy capture")
        .snapshot();
        let digest = policy_snapshot.digest();
        let weak = Arc::downgrade(&policy_snapshot);
        let (call, guarded) = guarded_with_session_context_and_policy_snapshot(
            auth,
            resume,
            runtime.clone(),
            &capture,
            Arc::clone(&policy_snapshot),
            Arc::clone(&spawner),
        );
        let ToolExecution::InputRequired(required) =
            tool.execute(call, &guarded).await.expect("required")
        else {
            panic!("expected input-required for {label}");
        };
        let id = required.continuation_id().to_owned();
        drop(policy_snapshot);
        drop(guarded);
        let causality = settled_continuation_causality(&runtime, &required, r#"{"choice":"yes"}"#);
        let result = owner
            .accept_answer(
                &id,
                br#"{"choice":"yes"}"#,
                runtime,
                causality,
                floor,
                ports(spawner),
            )
            .await;

        let Err(error) = result else {
            panic!("{label} must remain an effect failure");
        };
        assert_eq!(error.failure(), McpFailure::Transport, "{label}");
        assert_eq!(error.delivery(), Some(expected_delivery), "{label}");
        assert!(
            error
                .to_string()
                .contains("scripted resumed effect failure"),
            "{label} must preserve the original effect diagnostic"
        );
        assert_eq!(journal.state_for(&id), Some(expected_state), "{label}");
        assert_eq!(
            trace.events(),
            vec![
                "store",
                "record",
                "Elicitation",
                "accepted_answer",
                "ElicitationResult",
                "claim",
                "settle",
                "delete",
            ],
            "{label}"
        );
        assert!(!policy_captures.contains(&id, &digest), "{label}");
        assert!(weak.upgrade().is_none(), "{label}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn foreign_question_port_reuses_owner_capture_floor_and_effects_for_successor_rounds() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner_with_store(
        journal.clone(),
        Arc::clone(&policy_captures),
        resolver.clone(),
    );
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(Arc::clone(&factory));
    resolver.insert(capture.clone());
    let call = prepare_lookup(&tool);
    let auth = authorized(call);
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::with_input_required_rounds(2));
    let runtime = hooks_with_mode(HookMode::Continue, trace.clone());
    let policy_snapshot = GenerationPolicyCapture::new(
        CanonPath::new("/pinned-generation"),
        Mode::Auto,
        Ruleset::default(),
        None,
    )
    .expect("valid generation policy capture")
    .snapshot();
    let policy_digest = policy_snapshot.digest();
    let policy_snapshot_weak = Arc::downgrade(&policy_snapshot);
    let (call, guarded) = guarded_with_session_context_and_policy_snapshot(
        auth,
        resume,
        runtime.clone(),
        &capture,
        Arc::clone(&policy_snapshot),
        spawner.clone(),
    );
    let ToolExecution::InputRequired(initial) =
        tool.execute(call, &guarded).await.expect("required")
    else {
        panic!("expected required");
    };
    assert_eq!(journal.recorded_policy_digest(), policy_snapshot.digest());
    drop(policy_snapshot);
    drop(guarded);
    let retained_by_initial = policy_snapshot_weak
        .upgrade()
        .expect("the initial continuation owner must retain the exact policy snapshot");
    assert_eq!(retained_by_initial.digest(), policy_digest);
    drop(retained_by_initial);
    let initial_id = initial.continuation_id().to_owned();
    let runtime_for_events = runtime.clone();
    let initial_causality =
        settled_continuation_causality(&runtime_for_events, &initial, r#"{"choice":"yes"}"#);
    let adapter = ego_mcp::McpForeignQuestionPort::new(
        Arc::clone(&owner),
        runtime,
        floor,
        ports(spawner.clone()),
    );
    let first = ForeignQuestionPort::resume(
        &adapter,
        &initial_id,
        r#"{"choice":"yes"}"#,
        initial_causality,
    )
    .await
    .expect("first adapter resume");
    let ToolExecution::InputRequired(successor) = first else {
        panic!("the first adapter resume must expose the persisted successor");
    };
    assert_ne!(successor.continuation_id(), initial_id);
    let successor_id = successor.continuation_id().to_owned();
    assert_eq!(journal.state(), Some(ContinuationState::Pending));
    assert_eq!(
        journal.state_for(&initial_id),
        Some(ContinuationState::Completed)
    );
    assert!(!policy_captures.contains(&initial_id, &policy_digest));
    assert!(policy_captures.contains(&successor_id, &policy_digest));
    assert_eq!(
        policy_captures.calls(),
        vec![
            (
                GenerationPolicyCaptureStoreOperation::Store,
                initial_id.clone(),
                policy_digest.clone(),
            ),
            (
                GenerationPolicyCaptureStoreOperation::Store,
                successor_id.clone(),
                policy_digest.clone(),
            ),
            (
                GenerationPolicyCaptureStoreOperation::Delete,
                initial_id.clone(),
                policy_digest.clone(),
            ),
        ]
    );
    let retained_by_successor = policy_snapshot_weak
        .upgrade()
        .expect("the successor continuation must retain the original policy snapshot");
    assert_eq!(retained_by_successor.digest(), policy_digest);
    drop(retained_by_successor);

    let successor_causality =
        settled_continuation_causality(&runtime_for_events, &successor, r#"{"choice":"yes"}"#);
    let second = ForeignQuestionPort::resume(
        &adapter,
        successor.continuation_id(),
        r#"{"choice":"yes"}"#,
        successor_causality,
    )
    .await
    .expect("successor adapter resume");
    let ToolExecution::Complete(result) = second else {
        panic!("the successor adapter resume must complete");
    };
    assert_eq!(result.model_text(), "retry-complete");
    assert!(!policy_captures.contains(&successor_id, &policy_digest));
    assert!(
        policy_snapshot_weak.upgrade().is_none(),
        "terminal completion must release the generation policy snapshot pin"
    );
    assert_eq!(spawner.calls(), 3);
    assert_eq!(journal.state(), Some(ContinuationState::Completed));
    assert_eq!(
        journal.events(),
        vec![
            "record",
            "accepted_answer",
            "claim",
            "record",
            "settle",
            "accepted_answer",
            "claim",
            "settle"
        ]
    );
    assert_eq!(
        trace.events(),
        vec![
            "store",
            "record",
            "Elicitation",
            "accepted_answer",
            "ElicitationResult",
            "claim",
            "store",
            "record",
            "Elicitation",
            "settle",
            "delete",
            "accepted_answer",
            "ElicitationResult",
            "claim",
            "settle",
            "delete"
        ]
    );
    assert_eq!(
        policy_captures.calls(),
        vec![
            (
                GenerationPolicyCaptureStoreOperation::Store,
                initial_id.clone(),
                policy_digest.clone(),
            ),
            (
                GenerationPolicyCaptureStoreOperation::Store,
                successor_id.clone(),
                policy_digest.clone(),
            ),
            (
                GenerationPolicyCaptureStoreOperation::Delete,
                initial_id,
                policy_digest.clone(),
            ),
            (
                GenerationPolicyCaptureStoreOperation::Delete,
                successor_id,
                policy_digest,
            ),
        ]
    );
    assert!(
        policy_captures
            .calls()
            .iter()
            .all(|(operation, _, _)| { *operation != GenerationPolicyCaptureStoreOperation::Load }),
        "same-process successor rounds must never load the durable policy capture"
    );
}

#[test]
fn schema_or_authority_failure_precedes_durable_acceptance() {
    let journal = Arc::new(Journal::default());
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner(journal.clone(), resolver.clone());
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(Arc::clone(&factory));
    resolver.insert(capture.clone());
    let call = prepare_lookup(&tool);
    let auth = authorized(call.clone());
    let (sealer, floor) = prepared_resume();
    let mapped = mapped_input();
    let runtime = hooks_with_mode(HookMode::Continue, Trace::default());
    let causality = tool_causality(&runtime);
    let envelope = call.mcp_envelope().expect("envelope").clone();
    let policy_digest = GenerationPolicyCapture::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        Ruleset::default(),
        None,
    )
    .expect("valid generation policy capture")
    .snapshot()
    .digest();
    journal
        .record(
            sealer
                .prepare(&auth, floor.clone())
                .expect("resume")
                .into_record(
                    "continuation-schema-authority",
                    policy_digest,
                    mapped.request_state().to_vec(),
                    mapped.correlation_key(),
                    serde_json::to_vec(mapped.schema()).expect("schema"),
                    envelope.transport_fingerprint(),
                )
                .expect("record"),
        )
        .expect("journal");
    assert!(
        block_on(owner.accept_answer(
            "continuation-schema-authority",
            br#"{"unexpected":true}"#,
            runtime,
            causality,
            floor,
            ports(Arc::new(ScriptedSpawner::new()))
        ))
        .is_err()
    );
    assert_eq!(journal.state(), Some(ContinuationState::Pending));
    assert_eq!(journal.events(), vec!["record"]);
}

#[tokio::test(flavor = "current_thread")]
async fn pre_block_or_uncertainty_leaves_accepted_answer_unclaimed_without_reentry() {
    for mode in [HookMode::BlockResult, HookMode::UncertainResult] {
        let trace = Trace::default();
        let journal = Arc::new(Journal::with_trace(trace.clone()));
        let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
        let resolver = Arc::new(ExactCaptureResolver::default());
        let owner = context_free_owner_with_store(
            journal.clone(),
            Arc::clone(&policy_captures),
            resolver.clone(),
        );
        let factory = Arc::new(
            McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
                .expect("factory"),
        );
        let (capture, tool) = captured_lookup(Arc::clone(&factory));
        resolver.insert(capture.clone());
        let call = prepare_lookup(&tool);
        let auth = authorized(call);
        let (sealer, floor) = prepared_resume();
        let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
        let spawner = Arc::new(ScriptedSpawner::new());
        let runtime = hooks_with_mode(mode, trace.clone());
        let policy_snapshot = GenerationPolicyCapture::new(
            CanonPath::new("/pre-claim-policy"),
            Mode::Default,
            Ruleset::default(),
            None,
        )
        .expect("generation policy capture")
        .snapshot();
        let digest = policy_snapshot.digest();
        let weak = Arc::downgrade(&policy_snapshot);
        let (call, guarded) = guarded_with_session_context_and_policy_snapshot(
            auth,
            resume,
            runtime.clone(),
            &capture,
            Arc::clone(&policy_snapshot),
            spawner.clone(),
        );
        let ToolExecution::InputRequired(foreign) =
            tool.execute(call, &guarded).await.expect("required")
        else {
            panic!("expected required")
        };
        let id = foreign.continuation_id().to_owned();
        drop(policy_snapshot);
        drop(guarded);
        let causality = settled_continuation_causality(&runtime, &foreign, r#"{"choice":"yes"}"#);
        assert!(
            owner
                .accept_answer(
                    foreign.continuation_id(),
                    br#"{"choice":"yes"}"#,
                    runtime,
                    causality,
                    floor,
                    ports(spawner.clone())
                )
                .await
                .is_err()
        );
        assert_eq!(journal.state(), Some(ContinuationState::Accepted));
        assert!(!journal.events().contains(&"claim"));
        assert_eq!(spawner.calls(), 1);
        assert!(policy_captures.contains(&id, &digest));
        assert_eq!(weak.strong_count(), 2);
        assert_eq!(policy_captures.calls().len(), 1, "delete must not run");
        assert_eq!(
            journal.trace.events(),
            vec![
                "store",
                "record",
                "Elicitation",
                "accepted_answer",
                "ElicitationResult"
            ]
        );
    }
}

#[derive(Default)]
struct RegistryJournal {
    events: Mutex<Vec<McpRegistryEvent>>,
}
impl McpRegistryJournal for RegistryJournal {
    fn append(&self, event: McpRegistryEvent) -> Result<(), McpRegistryJournalError> {
        self.events.lock().expect("registry").push(event);
        Ok(())
    }
    fn replay(&self) -> Result<Vec<McpRegistryEvent>, McpRegistryJournalError> {
        Ok(self.events.lock().expect("registry").clone())
    }
    fn begin_refresh(
        &self,
        request_id: String,
        trigger: String,
        server_scope: Option<String>,
    ) -> Result<u64, McpRegistryJournalError> {
        self.events
            .lock()
            .expect("registry")
            .push(McpRegistryEvent::RefreshRequested {
                request_id,
                trigger,
                server_scope,
                config_revision: 1,
            });
        Ok(1)
    }
}
struct OneCandidate(Mutex<VecDeque<McpAdmittedCandidate>>);
impl McpCandidateAdmission for OneCandidate {
    fn admit(
        &self,
        _request: ego_mcp::McpCandidateRequest,
    ) -> McpFuture<'static, Result<McpAdmittedCandidate, McpCandidateError>> {
        let candidate = self.0.lock().expect("candidate").pop_front();
        Box::pin(async move {
            candidate.ok_or_else(|| McpCandidateError::Invalid {
                cause: "candidate already used".into(),
            })
        })
    }
}

fn registry_capture(factory: Arc<McpStdioOperationFactory>) -> McpRegistryCapture {
    registry_capture_with_history(factory, false).1
}

fn registry_capture_with_history(
    factory: Arc<McpStdioOperationFactory>,
    equivalent_successor: bool,
) -> (Arc<McpRegistryController>, McpRegistryCapture) {
    registry_capture_with_history_and_schema(
        factory,
        equivalent_successor,
        json!({"type":"object","additionalProperties":false}),
    )
}

fn registry_capture_with_schema(
    factory: Arc<McpStdioOperationFactory>,
    schema: serde_json::Value,
) -> McpRegistryCapture {
    registry_capture_with_history_and_schema(factory, false, schema).1
}

fn registry_capture_with_history_and_schema(
    factory: Arc<McpStdioOperationFactory>,
    equivalent_successor: bool,
    schema: serde_json::Value,
) -> (Arc<McpRegistryController>, McpRegistryCapture) {
    let admission = server_admission();
    let inventory = McpCandidateInventory::new(
        McpCapabilities::tools(),
        vec![
            ego_core::generator::types::ToolDescriptor {
                name: "lookup".into(),
                description: "continuation fixture".into(),
                parameters: schema.clone(),
            },
            ego_core::generator::types::ToolDescriptor {
                name: "lookup-secondary".into(),
                description: "second session continuation fixture".into(),
                parameters: schema,
            },
        ],
        Vec::new(),
        Vec::new(),
        McpCacheMetadata::default(),
        None,
    );
    let candidate = McpAdmittedCandidate::new(
        Arc::clone(&admission),
        McpCandidateIdentity::validated("server"),
        inventory,
        factory,
    )
    .expect("candidate");
    let candidates = if equivalent_successor {
        VecDeque::from([candidate.clone(), candidate])
    } else {
        VecDeque::from([candidate])
    };
    let candidate_port = Arc::new(OneCandidate(Mutex::new(candidates)));
    let registry = McpRegistryController::new(Arc::new(RegistryJournal::default()), candidate_port)
        .expect("registry");
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.server]\ntransport='stdio'\ncommand='mcp-server'\n",
    )
    .expect("config");
    let provenance = resolve(&[layer], Trust::Trusted, None)
        .expect("resolved")
        .mcp
        .entries()
        .values()
        .find_map(|entry| match entry {
            ResolvedMcpEntry::Configured { provenance, .. } => Some(provenance.clone()),
            ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .expect("provenance");
    let refresh = McpRegistryRefresh::new(
        vec![McpRegistryCandidate::ready(admission, provenance)],
        RegistryTrigger::Add,
        RegistryScope::All,
    )
    .expect("refresh");
    block_on(registry.reconcile(refresh)).expect("publication");
    let capture = registry.capture().expect("capture");
    (registry, capture)
}

fn registry_reload() -> McpRegistryRefresh {
    let layer = ConfigLayer::parse(
        Source::User,
        "[mcp.servers.server]\ntransport='stdio'\ncommand='mcp-server'\n",
    )
    .expect("config");
    let provenance = resolve(&[layer], Trust::Trusted, None)
        .expect("resolved")
        .mcp
        .entries()
        .values()
        .find_map(|entry| match entry {
            ResolvedMcpEntry::Configured { provenance, .. } => Some(provenance.clone()),
            ResolvedMcpEntry::Tombstone { .. } => None,
        })
        .expect("provenance");
    McpRegistryRefresh::new(
        vec![McpRegistryCandidate::ready(server_admission(), provenance)],
        RegistryTrigger::Reload,
        RegistryScope::All,
    )
    .expect("reload refresh")
}

// The continuation owner is process/admission scoped.  Session lifecycle and
// capture identity belong to the one-shot guarded resume context instead of
// being ambient state on the owner.
#[derive(Default)]
struct ExactCaptureResolver {
    captures: Mutex<BTreeMap<String, McpRegistryCapture>>,
    calls: AtomicUsize,
    after_resolve: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl ExactCaptureResolver {
    fn insert(&self, capture: McpRegistryCapture) {
        self.captures
            .lock()
            .expect("capture resolver")
            .insert(capture.stable_fingerprint().to_owned(), capture);
    }

    fn insert_for(&self, stable_fingerprint: &str, capture: McpRegistryCapture) {
        self.captures
            .lock()
            .expect("capture resolver")
            .insert(stable_fingerprint.to_owned(), capture);
    }

    fn remove(&self, stable_fingerprint: &str) -> Option<McpRegistryCapture> {
        self.captures
            .lock()
            .expect("capture resolver")
            .remove(stable_fingerprint)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn after_next_resolve(&self, callback: impl FnOnce() + Send + 'static) {
        *self.after_resolve.lock().expect("capture resolve callback") = Some(Box::new(callback));
    }
}

impl McpCaptureResolver for ExactCaptureResolver {
    fn resolve_capture(
        &self,
        stable_fingerprint: &str,
    ) -> Result<McpRegistryCapture, ego_mcp::McpRegistryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let capture = self
            .captures
            .lock()
            .expect("capture resolver")
            .get(stable_fingerprint)
            .cloned()
            .ok_or_else(|| {
                ego_mcp::McpRegistryError::InvalidCandidate(
                    "capture identity is unavailable".into(),
                )
            })?;
        let callback = self
            .after_resolve
            .lock()
            .expect("capture resolve callback")
            .take();
        if let Some(callback) = callback {
            callback();
        }
        Ok(capture)
    }
}

fn context_free_owner(
    journal: Arc<dyn ContinuationJournal>,
    resolver: Arc<ExactCaptureResolver>,
) -> Arc<McpContinuationOwner> {
    context_free_owner_with_store(journal, Arc::new(CaptureStore::default()), resolver)
}

fn context_free_owner_with_store(
    journal: Arc<dyn ContinuationJournal>,
    policy_captures: Arc<CaptureStore>,
    resolver: Arc<ExactCaptureResolver>,
) -> Arc<McpContinuationOwner> {
    Arc::new(McpContinuationOwner::new(
        journal,
        policy_captures,
        resolver,
        Arc::new(FixedResumeKey),
        Arc::new(FixedClock),
    ))
}

fn guarded_with_session_context(
    authorized: Authorized,
    resume: ego_core::suspension::PreparedResume,
    runtime: SessionHookRuntime<'static>,
    capture: &McpRegistryCapture,
    spawner: Arc<ScriptedSpawner>,
) -> (PreparedCall, GuardedEffects<'static>) {
    let policy_snapshot = GenerationPolicyCapture::new(
        CanonPath::new("/workspace"),
        Mode::Default,
        Ruleset::default(),
        None,
    )
    .expect("valid generation policy capture")
    .snapshot();
    guarded_with_session_context_and_policy_snapshot(
        authorized,
        resume,
        runtime,
        capture,
        policy_snapshot,
        spawner,
    )
}

fn guarded_with_session_context_and_policy_snapshot(
    authorized: Authorized,
    resume: ego_core::suspension::PreparedResume,
    runtime: SessionHookRuntime<'static>,
    capture: &McpRegistryCapture,
    policy_snapshot: Arc<GenerationPolicySnapshot>,
    spawner: Arc<ScriptedSpawner>,
) -> (PreparedCall, GuardedEffects<'static>) {
    let context = resume_context(runtime, capture, policy_snapshot);
    ToolEffectPorts::new(
        Box::leak(Box::new(ego_core::permit::fake::FakeFs::default())),
        Box::leak(Box::new(NoProcess)),
        Box::leak(Box::new(EmptyEvidence)),
        Arc::new(OfflineResolver),
        Arc::new(OfflineOneHop),
        spawner,
    )
    .bind_mcp_resume(authorized, resume, Some(context))
}

fn resume_context(
    runtime: SessionHookRuntime<'static>,
    capture: &McpRegistryCapture,
    policy_snapshot: Arc<GenerationPolicySnapshot>,
) -> McpResumeContext<'static> {
    let causality = runtime
        .hook_state()
        .inspect(|state| {
            state.items.iter().find_map(|(item, state)| {
                state
                    .tool_call_started
                    .map(|parent| HookCausality::item(*item, state.turn, parent))
            })
        })
        .expect("session state inspection")
        .expect("tool causality");
    let context = McpResumeContext::new(
        runtime,
        capture.clone(),
        causality,
        Arc::clone(&policy_snapshot),
    );
    let actual: Arc<GenerationPolicySnapshot> = context.policy_snapshot();
    assert!(Arc::ptr_eq(&actual, &policy_snapshot));
    assert_eq!(actual.digest(), policy_snapshot.digest());
    assert_eq!(
        actual.encode_confidential().expose_secret_bytes(),
        policy_snapshot.encode_confidential().expose_secret_bytes()
    );
    context
}

fn guarded_resume_with_session_context(
    resume: ego_core::suspension::PreparedResume,
    runtime: SessionHookRuntime<'static>,
    capture: &McpRegistryCapture,
    policy_snapshot: Arc<GenerationPolicySnapshot>,
) -> GuardedMcpResume<'static> {
    GuardedMcpResume::new(
        resume,
        Some(resume_context(runtime, capture, policy_snapshot)),
    )
}

fn guarded_without_session_context(
    authorized: Authorized,
    resume: ego_core::suspension::PreparedResume,
    spawner: Arc<ScriptedSpawner>,
) -> (PreparedCall, GuardedEffects<'static>) {
    ToolEffectPorts::new(
        Box::leak(Box::new(ego_core::permit::fake::FakeFs::default())),
        Box::leak(Box::new(NoProcess)),
        Box::leak(Box::new(EmptyEvidence)),
        Arc::new(OfflineResolver),
        Arc::new(OfflineOneHop),
        spawner,
    )
    .bind_mcp_resume(authorized, resume, None)
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_operation_carries_exact_session_and_stable_capture_context() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner(journal.clone(), resolver.clone());
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(Arc::clone(&factory));
    resolver.insert(capture.clone());
    let auth = authorized(prepare_lookup(&tool));
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor).expect("resume");
    let dispatcher = hooks_with_mode(HookMode::Continue, trace.clone());
    let spawner = Arc::new(ScriptedSpawner::new());
    let (call, guarded) =
        guarded_with_session_context(auth, resume, dispatcher, &capture, spawner.clone());

    let ToolExecution::InputRequired(required) =
        tool.execute(call, &guarded).await.expect("required")
    else {
        panic!("expected input-required");
    };
    assert_eq!(required.continuation_id(), journal.id());
    assert_eq!(journal.events(), vec!["record"]);
    assert_eq!(trace.events(), vec!["record", "Elicitation"]);
    assert_eq!(spawner.calls(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn successor_reconcile_before_input_projection_keeps_the_old_exact_capture_pinned() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner(journal.clone(), resolver.clone());
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (registry, capture) = registry_capture_with_history(Arc::clone(&factory), true);
    let stable = capture.stable_fingerprint().to_owned();
    let generation = capture.generation().to_owned();
    resolver.insert(capture.clone());
    let tool = captured_tool(&capture, "lookup");
    let auth = authorized(prepare_lookup(&tool));
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());

    registry
        .reconcile(registry_reload())
        .await
        .expect("publish equivalent successor before projection");
    assert_ne!(registry.publication().generation(), generation);
    assert_eq!(
        registry.publication().capture().stable_fingerprint(),
        stable
    );

    let (call, guarded) = guarded_with_session_context(
        auth,
        resume,
        hooks_with_mode(HookMode::Continue, trace.clone()),
        &capture,
        spawner.clone(),
    );
    let ToolExecution::InputRequired(required) =
        tool.execute(call, &guarded).await.expect("required")
    else {
        panic!("expected required");
    };
    assert_eq!(required.continuation_id(), journal.id());
    assert_eq!(journal.recorded_capture(), stable);
    assert_eq!(
        resolver
            .resolve_capture(&stable)
            .expect("pinned old capture")
            .generation(),
        generation
    );
}

#[tokio::test(flavor = "current_thread")]
async fn one_owner_routes_interleaved_answers_to_the_originating_session_dispatcher() {
    let trace_a = Trace::default();
    let trace_b = Trace::default();
    let journal = Arc::new(MultiJournal::default());
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner(journal.clone(), resolver.clone());
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool_a) = captured_lookup(Arc::clone(&factory));
    let tool_b = captured_tool(&capture, "lookup-secondary");
    resolver.insert(capture.clone());

    let first_auth = authorized(prepare_lookup(&tool_a));
    let (sealer, floor) = prepared_resume();
    let first_resume = sealer.prepare(&first_auth, floor.clone()).expect("resume");
    let first_spawner = Arc::new(ScriptedSpawner::new());
    let first_runtime = hooks_with_mode(HookMode::Continue, trace_a.clone());
    let (first_call, first_guarded) = guarded_with_session_context(
        first_auth,
        first_resume,
        first_runtime.clone(),
        &capture,
        first_spawner.clone(),
    );
    let ToolExecution::InputRequired(first) = tool_a
        .execute(first_call, &first_guarded)
        .await
        .expect("first required")
    else {
        panic!("expected first input-required");
    };
    let first_id = first.continuation_id().to_owned();
    let second_auth = authorized(prepare_lookup(&tool_b));
    let second_resume = sealer.prepare(&second_auth, floor.clone()).expect("resume");
    let second_spawner = Arc::new(ScriptedSpawner::new());
    let second_runtime = hooks_with_mode(HookMode::Continue, trace_b.clone());
    let (second_call, second_guarded) = guarded_with_session_context(
        second_auth,
        second_resume,
        second_runtime.clone(),
        &capture,
        second_spawner.clone(),
    );
    let ToolExecution::InputRequired(second) = tool_b
        .execute(second_call, &second_guarded)
        .await
        .expect("second required")
    else {
        panic!("expected second input-required");
    };
    assert_ne!(first_id, second.continuation_id());
    assert_eq!(journal.events(), vec!["record", "record"]);

    let second_causality =
        settled_continuation_causality(&second_runtime, &second, r#"{"choice":"two"}"#);
    let first_causality =
        settled_continuation_causality(&first_runtime, &first, r#"{"choice":"one"}"#);
    owner
        .accept_answer(
            second.continuation_id(),
            br#"{"choice":"two"}"#,
            second_runtime,
            second_causality,
            floor.clone(),
            ports(second_spawner),
        )
        .await
        .expect("second answer");
    owner
        .accept_answer(
            &first_id,
            br#"{"choice":"one"}"#,
            first_runtime,
            first_causality,
            floor,
            ports(first_spawner),
        )
        .await
        .expect("first answer");

    assert_eq!(trace_a.events(), vec!["Elicitation", "ElicitationResult"]);
    assert_eq!(trace_b.events(), vec!["Elicitation", "ElicitationResult"]);
    assert_eq!(
        journal.events(),
        vec![
            "record",
            "record",
            "accepted_answer",
            "claim",
            "settle",
            "accepted_answer",
            "claim",
            "settle"
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn answer_resume_resolves_recorded_capture_and_adapter_never_uses_loop_capture() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner(journal.clone(), resolver.clone());
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(Arc::clone(&factory));
    let stable = capture.stable_fingerprint().to_owned();
    resolver.insert(capture.clone());
    let auth = authorized(prepare_lookup(&tool));
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());
    let runtime = hooks_with_mode(HookMode::Continue, trace.clone());
    let (call, guarded) =
        guarded_with_session_context(auth, resume, runtime.clone(), &capture, spawner.clone());
    let ToolExecution::InputRequired(required) =
        tool.execute(call, &guarded).await.expect("required")
    else {
        panic!("expected required");
    };
    assert_eq!(journal.recorded_capture(), stable);
    let causality = settled_continuation_causality(&runtime, &required, r#"{"choice":"yes"}"#);
    let adapter =
        ego_mcp::McpForeignQuestionPort::new(Arc::clone(&owner), runtime, floor, ports(spawner));
    let result = ForeignQuestionPort::resume(
        &adapter,
        required.continuation_id(),
        r#"{"choice":"yes"}"#,
        causality,
    )
    .await
    .expect("resume");
    assert!(matches!(result, ToolExecution::Complete(_)));
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_result_keeps_the_pinned_capture_with_accepted_unclaimed_continuation() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let policy_captures = Arc::new(CaptureStore::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner_with_store(
        journal.clone(),
        Arc::clone(&policy_captures),
        resolver.clone(),
    );
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(Arc::clone(&factory));
    let stable = capture.stable_fingerprint().to_owned();
    resolver.insert(capture.clone());
    let auth = authorized(prepare_lookup(&tool));
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor.clone()).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());
    let runtime = hooks_with_mode(HookMode::BlockResult, trace.clone());
    let policy_snapshot = GenerationPolicyCapture::new(
        CanonPath::new("/blocked-result-policy"),
        Mode::Default,
        Ruleset::default(),
        None,
    )
    .expect("generation policy capture")
    .snapshot();
    let digest = policy_snapshot.digest();
    let weak = Arc::downgrade(&policy_snapshot);
    let (call, guarded) = guarded_with_session_context_and_policy_snapshot(
        auth,
        resume,
        runtime.clone(),
        &capture,
        Arc::clone(&policy_snapshot),
        spawner.clone(),
    );
    let ToolExecution::InputRequired(required) =
        tool.execute(call, &guarded).await.expect("required")
    else {
        panic!("expected required");
    };
    let id = required.continuation_id().to_owned();
    drop(guarded);
    drop(policy_snapshot);
    drop(capture);
    assert!(resolver.remove(&stable).is_some());

    let causality = settled_continuation_causality(&runtime, &required, r#"{"choice":"yes"}"#);
    let result = owner
        .accept_answer(
            required.continuation_id(),
            br#"{"choice":"yes"}"#,
            runtime,
            causality,
            floor,
            ports(spawner),
        )
        .await;
    assert!(result.is_err(), "blocked result must not enter egress");
    assert_eq!(journal.state(), Some(ContinuationState::Accepted));
    assert_eq!(journal.events(), vec!["record", "accepted_answer"]);
    assert_eq!(
        trace.events(),
        vec![
            "store",
            "record",
            "Elicitation",
            "accepted_answer",
            "ElicitationResult"
        ]
    );
    // The owner-held exact capture is still usable after the external map and
    // the operation's temporary capture handle have been released.
    assert_eq!(journal.recorded_capture(), stable);
    assert!(policy_captures.contains(&id, &digest));
    assert_eq!(weak.strong_count(), 2);
    assert_eq!(policy_captures.calls().len(), 1, "delete must not run");
}

#[tokio::test(flavor = "current_thread")]
async fn missing_session_context_fails_before_question_exposure_or_resumed_egress() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner(journal.clone(), resolver.clone());
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture, tool) = captured_lookup(Arc::clone(&factory));
    resolver.insert(capture);
    let auth = authorized(prepare_lookup(&tool));
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());
    let (call, guarded) = guarded_without_session_context(auth, resume, spawner.clone());

    assert!(tool.execute(call, &guarded).await.is_err());
    assert!(journal.events().is_empty());
    assert!(trace.events().is_empty());
    assert_eq!(spawner.calls(), 1, "only the initial admitted call ran");
}

#[tokio::test(flavor = "current_thread")]
async fn mismatched_capture_context_fails_closed_before_foreign_question_or_resume_egress() {
    let trace = Trace::default();
    let journal = Arc::new(Journal::with_trace(trace.clone()));
    let resolver = Arc::new(ExactCaptureResolver::default());
    let owner = context_free_owner(journal.clone(), resolver.clone());
    let factory = Arc::new(
        McpStdioOperationFactory::new(Arc::clone(&owner), 64 * 1024, Duration::from_secs(30))
            .expect("factory"),
    );
    let (capture_a, tool_a) = captured_lookup(Arc::clone(&factory));
    let capture_b = registry_capture_with_schema(
        factory,
        json!({
            "type":"object",
            "properties":{"different":{"type":"string"}},
            "required":["different"],
            "additionalProperties":false
        }),
    );
    assert_ne!(
        capture_a.stable_fingerprint(),
        capture_b.stable_fingerprint(),
        "mismatch fixture must use distinct stable capture identities"
    );
    resolver.insert(capture_a.clone());
    resolver.insert(capture_b.clone());

    let auth = authorized(prepare_lookup(&tool_a));
    let (sealer, floor) = prepared_resume();
    let resume = sealer.prepare(&auth, floor).expect("resume");
    let spawner = Arc::new(ScriptedSpawner::new());
    let (call, guarded) = guarded_with_session_context(
        auth,
        resume,
        hooks_with_mode(HookMode::Continue, trace.clone()),
        &capture_b,
        spawner.clone(),
    );

    assert!(
        tool_a.execute(call, &guarded).await.is_err(),
        "a prepared call from capture A cannot execute with capture B context"
    );
    assert!(journal.events().is_empty());
    assert!(trace.events().is_empty());
    assert_eq!(spawner.calls(), 1, "only the initial admitted call ran");
}

#[test]
fn mcp_hook_adapter_wire_retains_the_admitted_hook_identity() {
    let hook = HookId::derive(
        HookSourceAuthority::Project,
        &HookSourceLocation::parse("hooks/mcp.toml").expect("hook source"),
        &HookContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("hook digest"),
        0,
    );
    let request = HookInvocationRequest::new(
        HookStartedEvidence::new(
            EventId::new(),
            EventId::new(),
            hook,
            HookInvocationId::new(),
            HookActivation::Persistent,
        ),
        HookCausality::session(SessionId::new(), TurnId::new()),
        HookHandlerConfig::McpTool {
            server: "server".to_owned(),
            tool: "lookup".to_owned(),
            input: json!({}),
            timeout_seconds: Some(30),
        },
        HookEvent::turn(
            HookLifecycle::PreToolUse,
            ego_core::event::hook::HookSubject::Session,
        ),
        EffectScope::new(Duration::from_secs(30)),
    );
    let wire = serde_json::to_value(
        ego_core::hook::HookEventV1::from_runtime(&request).expect("strict hook event"),
    )
    .expect("hook event wire");
    assert!(
        wire.get("hook_id").is_some(),
        "the complete invocation request must carry hook identity through MCP"
    );
}
