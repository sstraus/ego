//! RED contract for the typed skill operation and authority epoch (M4 S6).
//!
//! These tests deliberately cross the existing blob, session-journal, turn,
//! permit, child, and hook seams. They do not define a second skill parser,
//! authority object, or effect runner.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use serde_json::json;

use ego_core::budget::{BudgetGate, SharedBudgetGate};
use ego_core::config::{ModelRef, Trust};
use ego_core::event::domain::{DomainEvent, EventKind};
use ego_core::event::hook::{HookDecision, HookLifecycle, HookOutcome};
use ego_core::event::id::{EventId, ItemId, Producer, SessionId, SubagentInvocationId, TurnId};
use ego_core::event::skill::{
    SkillContentDigest, SkillDescriptorEvidence, SkillId, SkillInvocationId, SkillSourceAuthority,
    SkillSourceLocation,
};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{EvidenceError, EvidenceWrite};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::hook::{
    HookCatalogue, HookConfigEntry, HookFuture, HookHandlerConfig, HookInvocationPort,
    HookInvocationRequest, HookLifecycleDispatcher, HookRunner, SessionHookRuntime,
    SessionHookState,
};
use ego_core::permit::{GuardedEffects, GuardedIo, Op, PreparedCall};
use ego_core::run::{
    EventSinkFactory, IdSource, RecoveryEvidence, RequestId, RunAttachment, RunController,
    RunExecutionBindings, RunExecutor, RunExecutorError, RunFuture, RunHandle, RunId,
    RunPreparation, RunPrepared, RunPrompt, RunRetention, RunRoot, RunSelections, RunSettlement,
};
use ego_core::sink::NullSink;
use ego_core::skill::{
    ActiveSkillScope, SkillCatalogue, SkillInvocation, SkillInvocationOutcome, SkillInvocationPort,
    SkillOperation, SkillSpec,
};
use ego_core::state::SessionState;
use ego_core::store::blob::{
    BlobError, BlobId, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter,
};
use ego_core::store::evidence::{
    EvidenceError as StoreEvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::subagent::{
    ChildJournal, ChildJournalError, ChildProfileResolver, ChildRequest, ChildRequestOrigin,
    ChildResultEvidence, ChildResultReader, ChildResultReaderError, ChildResultStore,
    ChildRunService, ChildSelectionAdmission, ChildServicePorts, ChildTaskReader, ChildWait,
};
use ego_core::turn::{CancelToken, Journal, RegisteredTool, SharedJournal};

const DIGEST_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const DIGEST_B: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const BODY: &[u8] = b"Explain $ARGUMENTS and keep $1 inert.";

type BlobRecord = (BlobRef, Vec<u8>);
type CommittedBlobs = Arc<Mutex<BTreeMap<String, BlobRecord>>>;

#[derive(Clone)]
struct RecordingBlobStore {
    committed: CommittedBlobs,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl Default for RecordingBlobStore {
    fn default() -> Self {
        Self {
            committed: Arc::new(Mutex::new(BTreeMap::new())),
            order: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl RecordingBlobStore {
    fn with_order(order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            committed: Arc::new(Mutex::new(BTreeMap::new())),
            order,
        }
    }
}

struct RecordingBlobWriter {
    owner: RecordingBlobStore,
    bytes: Vec<u8>,
}

impl BlobWriter for RecordingBlobWriter {
    fn append(&mut self, chunk: &[u8]) -> Result<(), BlobError> {
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn commit(self: Box<Self>) -> Result<BlobRef, BlobError> {
        let id = BlobId::parse(ego_core::digest::sha256_hex(&self.bytes))?;
        let reference = BlobRef::new(id, self.bytes.len() as u64);
        self.owner
            .committed
            .lock()
            .expect("blob records")
            .insert(reference.id.to_string(), (reference.clone(), self.bytes));
        self.owner.order.lock().expect("commit order").push("blob");
        Ok(reference)
    }

    fn abort(self: Box<Self>) -> Result<(), BlobError> {
        Ok(())
    }
}

impl BlobStore for RecordingBlobStore {
    fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError> {
        Ok(Box::new(RecordingBlobWriter {
            owner: self.clone(),
            bytes: Vec::new(),
        }))
    }

    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
        self.committed
            .lock()
            .expect("blob records")
            .get(&reference.id.to_string())
            .filter(|(stored, _)| stored == reference)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| BlobError::NotFound {
                reference: reference.id.clone(),
            })
    }

    fn read_page<'a>(
        &'a self,
        request: &'a BlobRead,
    ) -> BoxFuture<'a, Result<BlobPage, BlobError>> {
        let result = self
            .read(&request.reference)
            .and_then(|bytes| BlobPage::new(request.reference.clone(), request.offset, bytes));
        Box::pin(futures::future::ready(result))
    }

    fn collect_garbage(&self, reachable: &[BlobRef]) -> Result<usize, BlobError> {
        let mut committed = self.committed.lock().expect("blob records");
        let before = committed.len();
        committed.retain(|_, (reference, _)| reachable.contains(reference));
        Ok(before - committed.len())
    }
}

struct RecordingSessionStore {
    entries: Mutex<Vec<(SessionId, SessionEntry)>>,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl Default for RecordingSessionStore {
    fn default() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            order: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl RecordingSessionStore {
    fn with_order(order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            order,
        }
    }
}

impl RecordingSessionStore {
    fn entries(&self) -> Vec<SessionEntry> {
        self.entries
            .lock()
            .expect("session entries")
            .iter()
            .map(|(_, entry)| entry.clone())
            .collect()
    }
}

impl SessionStore for RecordingSessionStore {
    fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.entries
            .lock()
            .expect("session entries")
            .push((*id, SessionEntry::Event(event.clone())));
        self.order.lock().expect("append order").push("event");
        Ok(())
    }

    fn read(
        &self,
        id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let entries = self
            .entries
            .lock()
            .expect("session entries")
            .iter()
            .filter(|(entry_id, _)| entry_id == id)
            .map(|(_, entry)| entry.clone())
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Err(StoreError::NotFound);
        }
        Ok(Box::pin(stream::iter(entries.into_iter().map(Ok))))
    }

    fn compare_and_append_run(
        &self,
        session: &SessionId,
        expected_revision: Option<ego_core::run::RunRevision>,
        transaction: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        let mut entries = self.entries.lock().expect("session entries");
        let current = entries.iter().rev().find_map(|(entry_session, entry)| {
            if entry_session != session {
                return None;
            }
            match entry {
                SessionEntry::RunTransaction(transaction) => Some(transaction.revision()),
                SessionEntry::Event(_) | SessionEntry::Evidence(_) => None,
            }
        });
        if current != expected_revision {
            return Ok(ego_core::run::RunAppend::Conflict {
                actual: current.unwrap_or_else(|| ego_core::run::RunRevision::new(0)),
            });
        }
        let revision = transaction.revision();
        entries.push((*session, SessionEntry::RunTransaction(transaction)));
        Ok(ego_core::run::RunAppend::Committed { revision })
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::EPOCH
    }
}

struct NoProcess;

impl ego_core::permit::ProcessExecutor for NoProcess {
    fn execute<'a>(
        &'a self,
        _spec: &'a ego_core::permit::SpawnSpec,
    ) -> BoxFuture<'a, Result<ego_core::permit::ToolResult, ego_core::permit::ProcessError>> {
        Box::pin(futures::future::ready(Err(
            ego_core::permit::ProcessError::Invariant("skill resource test".to_owned()),
        )))
    }
}

#[derive(Default)]
struct NoEvidence {
    blobs: RecordingBlobStore,
}

impl EvidenceStore for NoEvidence {
    fn blob_store(&self) -> &dyn BlobStore {
        &self.blobs
    }

    fn begin(&self) -> Result<Box<dyn EvidenceWriter>, StoreEvidenceError> {
        Err(StoreEvidenceError::Limit { limit: 0 })
    }

    fn read_page<'a>(
        &'a self,
        request: &'a EvidenceRead,
    ) -> BoxFuture<'a, Result<EvidencePage, StoreEvidenceError>> {
        Box::pin(futures::future::ready(Err(StoreEvidenceError::NotFound {
            reference: request.reference.id.clone(),
        })))
    }
}

struct NoResolver;

impl ego_core::permit::Resolver for NoResolver {
    fn resolve(
        &self,
        _target: ego_core::permit::NetworkTarget,
        _deadline: std::time::Instant,
    ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, ego_core::permit::TransportError>>
    {
        Box::pin(futures::future::ready(Err(
            ego_core::permit::TransportError::definitely_unsent(
                ego_core::permit::TransportFailure::Offline,
            ),
        )))
    }
}

struct NoHop;

impl ego_core::permit::OneHop for NoHop {
    fn begin_call(&self) -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(60)
    }
    fn execute(
        &self,
        _connect: ego_core::permit::ResolvedConnect,
        _request: ego_core::permit::HttpRequest,
        _deadline: std::time::Instant,
    ) -> BoxFuture<
        'static,
        Result<ego_core::permit::OneHopResponse, ego_core::permit::TransportError>,
    > {
        Box::pin(futures::future::ready(Err(
            ego_core::permit::TransportError::definitely_unsent(
                ego_core::permit::TransportFailure::Offline,
            ),
        )))
    }
}

struct NoDuplex;

impl ego_core::permit::DuplexSpawner for NoDuplex {
    fn spawn(
        &self,
        _spec: ego_core::permit::SpawnSpec,
    ) -> BoxFuture<'static, Result<ego_core::permit::DuplexHandle, ego_core::permit::TransportError>>
    {
        Box::pin(futures::future::ready(Err(
            ego_core::permit::TransportError::definitely_unsent(
                ego_core::permit::TransportFailure::Offline,
            ),
        )))
    }
}

fn descriptor(
    source: SkillSourceAuthority,
    name: &str,
    digest: &str,
    user_invocable: bool,
    model_invocable: bool,
) -> SkillDescriptorEvidence {
    let location = SkillSourceLocation::parse(&format!("skills/{name}/SKILL.md")).unwrap();
    let source_digest = SkillContentDigest::parse(digest).unwrap();
    SkillDescriptorEvidence {
        id: SkillId::derive(source, &location, name, &source_digest),
        name: name.to_owned(),
        description: format!("Description for {name}"),
        trigger: None,
        argument_hint: Some("QUERY".to_owned()),
        source,
        location,
        source_digest,
        user_invocable,
        model_invocable,
    }
}

fn spec(name: &str, digest: &str, user_invocable: bool, model_invocable: bool) -> SkillSpec {
    spec_with_body(name, digest, user_invocable, model_invocable, BODY)
}

fn spec_with_body(
    name: &str,
    digest: &str,
    user_invocable: bool,
    model_invocable: bool,
    body: &[u8],
) -> SkillSpec {
    SkillSpec::new(
        descriptor(
            SkillSourceAuthority::User,
            name,
            digest,
            user_invocable,
            model_invocable,
        ),
        body.to_vec(),
    )
    .expect("admitted skill spec")
    .with_model(ModelRef("ordinary-model".to_owned()), "balanced")
    .with_tool_ceiling(["read", "grep"])
    .with_fork(ChildWait::Foreground)
    .with_hook_entries(vec![
        HookConfigEntry::new(
            HookLifecycle::PreToolUse,
            None,
            HookHandlerConfig::Prompt {
                prompt: "fixture".to_owned(),
                model: None,
                timeout_seconds: None,
            },
        ),
        HookConfigEntry::new(
            HookLifecycle::PostToolUse,
            None,
            HookHandlerConfig::Prompt {
                prompt: "fixture".to_owned(),
                model: None,
                timeout_seconds: None,
            },
        ),
    ])
}

fn catalogue(name: &str, digest: &str) -> SkillCatalogue {
    SkillCatalogue::new(vec![spec(name, digest, true, true)]).expect("admitted catalogue")
}

#[test]
fn skill_operation_wire_is_closed_and_has_only_invoke_or_read_resource() {
    let invoke = SkillOperation::Invoke {
        name: "explain".to_owned(),
        arguments: "sh -c 'touch /tmp/nope'".to_owned(),
    };
    let read = SkillOperation::ReadResource {
        invocation: SkillInvocationId::new(),
        path: "refs/data.txt".to_owned(),
    };
    for operation in [invoke, read] {
        let wire = serde_json::to_value(&operation).expect("typed operation wire");
        assert!(serde_json::from_value::<SkillOperation>(wire).is_ok());
    }
    assert!(
        serde_json::from_value::<SkillOperation>(json!({
            "kind": "invoke",
            "name": "explain",
            "arguments": "x",
            "extra": true,
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<SkillOperation>(json!({
            "kind": "execute",
            "name": "explain",
            "arguments": "x",
        }))
        .is_err()
    );
}

#[test]
fn renderer_is_one_inert_pass_and_preserves_the_exact_raw_argument_tail() {
    let skill = spec("explain", DIGEST_A, true, true);
    let operation = SkillOperation::Invoke {
        name: "explain".to_owned(),
        arguments: "$(touch /tmp/nope)".to_owned(),
    };
    let rendered = skill.render(&operation).expect("inert skill rendering");
    assert_eq!(rendered, "Explain $(touch /tmp/nope) and keep $1 inert.");
    assert_eq!(skill.render(&operation).unwrap(), rendered);
}

#[test]
fn active_scope_pins_model_and_narrows_effect_tools_monotonically() {
    let mut scope = ActiveSkillScope::new(["read", "grep", "write"]);
    scope.pin_model("ordinary-model", "balanced").unwrap();
    assert_eq!(scope.model(), Some("ordinary-model"));
    assert_eq!(scope.effort(), Some("balanced"));
    scope.narrow_effect_tools(["read", "grep"]).unwrap();
    scope.narrow_effect_tools(["read"]).unwrap();
    assert_eq!(scope.effect_tools(), ["read"]);
    assert!(scope.pin_model("other-model", "balanced").is_err());
    assert!(scope.narrow_effect_tools(["read", "write"]).is_err());
}

#[test]
fn four_real_visibility_configs_announce_and_authorize_only_their_admitted_audience() {
    let both = SkillCatalogue::new(vec![spec("both", DIGEST_A, true, true)]).unwrap();
    let user = SkillCatalogue::new(vec![spec("user", DIGEST_A, true, false)]).unwrap();
    let model = SkillCatalogue::new(vec![spec("model", DIGEST_A, false, true)]).unwrap();
    assert!(
        both.announce_for_user()
            .iter()
            .any(|item| item.name() == "both")
    );
    assert!(
        both.announce_for_model()
            .iter()
            .any(|item| item.name() == "both")
    );
    assert!(both.authorize_user("both").is_ok());
    assert!(both.authorize_model("both").is_ok());
    assert!(
        user.announce_for_user()
            .iter()
            .any(|item| item.name() == "user")
    );
    assert!(user.announce_for_model().is_empty());
    assert!(user.authorize_user("user").is_ok());
    assert!(user.authorize_model("user").is_err());
    assert!(model.announce_for_user().is_empty());
    assert!(
        model
            .announce_for_model()
            .iter()
            .any(|item| item.name() == "model")
    );
    assert!(model.authorize_user("model").is_err());
    assert!(model.authorize_model("model").is_ok());
    assert!(SkillCatalogue::new(vec![spec("disabled", DIGEST_A, false, false)]).is_err());
}

#[test]
fn durable_admission_and_rendered_blobs_are_forced_in_order_and_resume_uses_captured_authority() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let blobs = RecordingBlobStore::with_order(Arc::clone(&order));
    let store = RecordingSessionStore::with_order(Arc::clone(&order));
    let clock = FixedClock;
    let sink = NullSink;
    let session = SessionId::new();
    let journal = SharedJournal::new(Journal::new(
        session,
        &store,
        &clock,
        &sink,
        Producer::Main,
        Default::default(),
    ));
    let first = catalogue("explain", DIGEST_A);
    let invocation = SkillInvocation::admit(
        &first,
        SkillOperation::Invoke {
            name: "explain".to_owned(),
            arguments: "captured".to_owned(),
        },
        &blobs,
        &journal,
    )
    .unwrap();
    let records = store.entries();
    assert!(matches!(
        records[0],
        SessionEntry::Event(DomainEvent {
            kind: EventKind::SkillInstructionAdmitted { .. },
            ..
        })
    ));
    let SessionEntry::Event(DomainEvent {
        kind:
            EventKind::SkillInstructionAdmitted {
                admission,
                rendered,
                model,
                effort,
                ..
            },
        ..
    }) = &records[0]
    else {
        panic!("skill admission event must carry all captured authority");
    };
    assert_eq!(admission.bytes, invocation.admission_ref().unwrap().bytes);
    assert_eq!(rendered.bytes, invocation.rendered_ref().unwrap().bytes);
    assert_eq!(model.as_deref(), Some("ordinary-model"));
    assert_eq!(effort.as_deref(), Some("balanced"));
    assert_eq!(&*order.lock().unwrap(), &["blob", "blob", "event"]);
    assert_eq!(invocation.model(), Some("ordinary-model"));
    assert_eq!(invocation.effort(), Some("balanced"));
    assert!(invocation.admission_ref().is_some());
    assert!(invocation.rendered_ref().is_some());
    assert_eq!(blobs.committed.lock().unwrap().len(), 2);
    let admission = invocation.admission_blob(&blobs).unwrap();
    assert!(serde_json::to_vec(&admission).unwrap().len() <= 64 * 1024);
    assert!(
        serde_json::to_vec(&admission)
            .unwrap()
            .windows(BODY.len())
            .all(|window| window != BODY)
    );

    let changed = SkillCatalogue::new(vec![spec_with_body(
        "explain",
        DIGEST_B,
        true,
        true,
        b"Changed source bytes must not be reread.",
    )])
    .unwrap();
    let resumed = SkillInvocation::resume(invocation.id(), &changed, &blobs, &journal).unwrap();
    assert_eq!(resumed.admission_blob(&blobs).unwrap(), admission);
    assert_eq!(
        serde_json::to_value(resumed.admission_blob(&blobs).unwrap()).unwrap(),
        serde_json::to_value(&admission).unwrap(),
        "resume must reconstruct from the admitted snapshot, never current skill source"
    );
    assert_eq!(
        resumed.rendered(&blobs).unwrap(),
        invocation.rendered(&blobs).unwrap()
    );
}

#[test]
fn admitted_skill_snapshot_persists_versioned_executable_hooks_and_descriptor_evidence() {
    let blobs = RecordingBlobStore::default();
    let store = RecordingSessionStore::default();
    let clock = FixedClock;
    let sink = NullSink;
    let session = SessionId::new();
    let journal = SharedJournal::new(Journal::new(
        session,
        &store,
        &clock,
        &sink,
        Producer::Main,
        Default::default(),
    ));
    let spec = spec("snapshot", DIGEST_A, true, true)
        .with_resource_paths(["refs/**"])
        .expect("bounded resource policy");
    let catalogue = SkillCatalogue::new(vec![spec]).expect("complete skill catalogue");
    let invocation = SkillInvocation::admit(
        &catalogue,
        SkillOperation::Invoke {
            name: "snapshot".to_owned(),
            arguments: "captured".to_owned(),
        },
        &blobs,
        &journal,
    )
    .expect("skill admission");

    let value =
        serde_json::to_value(invocation.admission_blob(&blobs).unwrap()).expect("admission JSON");
    assert_eq!(value["version"], 1, "the hook snapshot has its own version");
    let hooks = value["hooks"].as_array().expect("complete hook records");
    assert_eq!(hooks.len(), 2);
    for hook in hooks {
        assert!(hook.get("lifecycle").is_some(), "lifecycle is required");
        assert!(hook.get("matcher").is_some(), "matcher is required");
        assert!(hook.get("once").is_some(), "once policy is required");
        let handler = hook.get("handler").expect("handler configuration");
        assert_eq!(handler["type"], "prompt");
        assert_eq!(handler["prompt"], "fixture");
        let evidence = hook.get("evidence").expect("descriptor evidence");
        for field in ["id", "source", "location", "digest", "ordinal"] {
            assert!(evidence.get(field).is_some(), "missing {field} evidence");
        }
    }
    assert!(serde_json::to_vec(&value).expect("admission bytes").len() <= 64 * 1024);
}

#[test]
fn admitted_skill_snapshot_rejects_lifecycle_only_unknown_missing_mismatched_and_oversized_shapes()
{
    let blobs = RecordingBlobStore::default();
    let store = RecordingSessionStore::default();
    let clock = FixedClock;
    let sink = NullSink;
    let session = SessionId::new();
    let journal = SharedJournal::new(Journal::new(
        session,
        &store,
        &clock,
        &sink,
        Producer::Main,
        Default::default(),
    ));
    let catalogue = catalogue("snapshot", DIGEST_A);
    let invocation = SkillInvocation::admit(
        &catalogue,
        SkillOperation::Invoke {
            name: "snapshot".to_owned(),
            arguments: "captured".to_owned(),
        },
        &blobs,
        &journal,
    )
    .expect("skill admission");
    let valid =
        serde_json::to_value(invocation.admission_blob(&blobs).unwrap()).expect("admission JSON");

    let mut lifecycle_only = valid.clone();
    lifecycle_only["hooks"] = json!([{"lifecycle": "pre_tool_use"}]);
    assert!(
        serde_json::from_value::<ego_core::skill::SkillInvocationAdmission>(lifecycle_only)
            .is_err(),
        "lifecycle-only records cannot execute"
    );

    let mut unknown = valid.clone();
    unknown["unknown"] = json!(true);
    assert!(
        serde_json::from_value::<ego_core::skill::SkillInvocationAdmission>(unknown).is_err(),
        "unknown admission fields fail closed"
    );

    let mut missing_version = valid.clone();
    missing_version
        .as_object_mut()
        .expect("admission object")
        .remove("version");
    assert!(
        serde_json::from_value::<ego_core::skill::SkillInvocationAdmission>(missing_version)
            .is_err(),
        "missing snapshot version fails closed"
    );

    let mut missing_hooks = valid.clone();
    missing_hooks
        .as_object_mut()
        .expect("admission object")
        .remove("hooks");
    assert!(
        serde_json::from_value::<ego_core::skill::SkillInvocationAdmission>(missing_hooks).is_err(),
        "missing executable hook snapshot fails closed"
    );

    let mut mismatched = valid.clone();
    mismatched["hooks"][0]["evidence"]["digest"] = json!(DIGEST_B);
    assert!(
        serde_json::from_value::<ego_core::skill::SkillInvocationAdmission>(mismatched).is_err(),
        "descriptor evidence mismatch fails closed"
    );

    let mut oversized = valid;
    oversized["hooks"][0]["handler"] = json!({"type": "prompt", "prompt": "x".repeat(64 * 1024)});
    assert!(
        serde_json::from_value::<ego_core::skill::SkillInvocationAdmission>(oversized).is_err(),
        "oversized executable hook configuration fails closed"
    );
}

#[test]
fn resource_reads_cross_prepared_call_and_guarded_effects_before_durable_bytes() {
    let path = ego_core::permit::CanonPath::new("/workspace/skills/explain/refs/data.txt");
    let prepared = PreparedCall::new("skill_resource", Op::Read { path: path.clone() });
    assert!(
        prepared
            .ops()
            .iter()
            .any(|op| matches!(op, Op::Read { .. }))
    );

    let policy = ego_core::permit::PolicyEngine::new(
        ego_core::permit::CanonPath::new("/workspace"),
        ego_core::config::Mode::Default,
        ego_core::config::Ruleset::from_layer(
            ego_core::config::PermCfg {
                judge: None,
                allow: vec![
                    ego_core::permit::rule::Rule::load("read(**)").expect("resource read rule"),
                ],
                deny: Vec::new(),
            },
            ego_core::config::Source::Project,
        ),
    );
    let ego_core::permit::Classification::Settled(Ok(authorized)) = policy.classify(prepared)
    else {
        panic!("the admitted resource read must be authorised");
    };

    let fs = ego_core::permit::fake::FakeFs::default();
    fs.file("/workspace/skills/explain/refs/data.txt", "exact\nbytes\0");
    let process = NoProcess;
    let evidence = NoEvidence::default();
    let resolver = Arc::new(NoResolver);
    let one_hop = Arc::new(NoHop);
    let duplex = Arc::new(NoDuplex);
    let (_, permit) = authorized.into_parts();
    let guarded = GuardedEffects::new(permit, &fs, &process, &evidence, resolver, one_hop, duplex);
    let bytes = guarded.read(&path).expect("guarded exact resource bytes");
    assert_eq!(bytes, b"exact\nbytes\0");
}

#[test]
fn turn_batch_selection_rejects_skill_and_effect_before_either_executes() {
    let catalogue = Arc::new(catalogue("explain", DIGEST_A));
    let registered = [
        RegisteredTool::skill(Arc::clone(&catalogue)),
        RegisteredTool::from(Arc::new(ego_core::permit::Typed(
            ego_core::permit::fake::FakeWriteTool,
        ))),
    ];
    let _policy = ego_core::permit::PolicyEngine::new(
        ego_core::permit::CanonPath::new("/workspace"),
        ego_core::config::Mode::Default,
        ego_core::config::Ruleset::default(),
    );
    let fs = ego_core::permit::fake::FakeFs::default();
    let process = NoProcess;
    let evidence = NoEvidence::default();
    let tools = ego_core::turn::Tools::from_registry(
        &registered,
        ego_core::turn::ToolEffectPorts::new(
            &fs,
            &process,
            &evidence,
            Arc::new(NoResolver),
            Arc::new(NoHop),
            Arc::new(NoDuplex),
        ),
        ego_core::permit::CanonPath::new("/workspace"),
    );
    let result = tools.prepare_batch([
        (
            "explain",
            r#"{"kind":"invoke","name":"explain","arguments":"one"}"#,
        ),
        ("fake_write", r#"{"path":"/workspace/out","contents":"x"}"#),
    ]);
    let error = result.expect_err("mixed skill/effect batch must reject before execution");
    assert!(format!("{error:?}").contains("MixedAuthority"));
    assert!(
        <ego_core::permit::fake::FakeFs as ego_core::permit::Fs>::read(
            &fs,
            &ego_core::permit::CanonPath::new("/workspace/out"),
        )
        .is_err()
    );
}

#[test]
fn skill_invocation_port_bridges_real_child_service_for_both_fork_modes() {
    let RealChildFixture {
        child,
        parent,
        child_events,
        child_waits,
        origins: [foreground_origin, background_origin],
    } = real_child_service();
    let port = ChildServiceInvocationPort {
        child: child.clone(),
        parent: parent.clone(),
    };
    let foreground_invocation = SkillInvocation::admit_for_service(
        spec("explain", DIGEST_A, true, true),
        SkillOperation::Invoke {
            name: "explain".to_owned(),
            arguments: "foreground".to_owned(),
        },
    )
    .expect("typed service invocation");
    let foreground_id = foreground_invocation.id();
    let background_invocation = SkillInvocation::admit_for_service(
        spec("explain", DIGEST_A, true, true).with_fork(ChildWait::Background),
        SkillOperation::Invoke {
            name: "explain".to_owned(),
            arguments: "background".to_owned(),
        },
    )
    .expect("typed background service invocation");
    let background_id = background_invocation.id();

    let foreground = futures::executor::block_on(port.invoke(
        foreground_invocation,
        foreground_origin.turn,
        foreground_origin.item,
        foreground_origin.event,
        BTreeSet::new(),
    ))
    .expect("foreground skill invocation");
    let background = futures::executor::block_on(port.invoke(
        background_invocation,
        background_origin.turn,
        background_origin.item,
        background_origin.event,
        BTreeSet::new(),
    ))
    .expect("background skill invocation");
    assert!(matches!(foreground, SkillInvocationOutcome::Foreground(_)));
    assert!(matches!(background, SkillInvocationOutcome::Background(_)));
    assert_ne!(foreground_id, background_id);
    let child_events = child_events.lock().expect("child journal");
    assert_eq!(
        child_events
            .iter()
            .filter(|event| matches!(&event.kind, EventKind::SubagentRequested { .. }))
            .map(|event| (event.env.turn, event.env.item))
            .collect::<Vec<_>>(),
        vec![
            (foreground_origin.turn, Some(foreground_origin.item)),
            (background_origin.turn, Some(background_origin.item)),
        ],
        "each fork preserves the originating tool turn and item",
    );
    assert_eq!(
        &*child_waits.lock().expect("child waits"),
        &[ChildWait::Foreground, ChildWait::Background]
    );
}

struct NullFactory;

impl EventSinkFactory for NullFactory {
    fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn ego_core::sink::EventSink> {
        Arc::new(NullSink)
    }
}

#[derive(Default)]
struct FixedIds;

impl IdSource for FixedIds {
    fn next_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::now_v7()
    }
}

struct ChildExecutor;

impl RunExecutor for ChildExecutor {
    type Prepared = ();

    fn prepare<'a>(
        &'a self,
        _request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        Box::pin(futures::future::ready(Ok(RunPrepared::new(
            ModelRef("ordinary-model".to_owned()),
            (),
        ))))
    }

    fn admit_run_context<'a>(
        &'a self,
        _prepared: &'a Self::Prepared,
    ) -> RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
        Box::pin(futures::future::ready(Ok(EvidenceWrite::Published)))
    }

    fn attach<'a>(
        &'a self,
        request: &'a RunAttachment,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        Box::pin(futures::future::ready(Ok(RunHandle::new(
            request.run_id,
            (),
        ))))
    }

    fn recover<'a>(
        &'a self,
        _run: &'a RunId,
    ) -> BoxFuture<'a, Result<RecoveryEvidence, RunExecutorError>> {
        Box::pin(futures::future::ready(Ok(RecoveryEvidence::Ambiguous)))
    }

    fn observe<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(futures::future::ready(Ok(RunSettlement::Completed)))
    }

    fn submit<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _prompt: &'a RunPrompt,
        _prepared: Self::Prepared,
        _bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(futures::future::ready(Ok(RunSettlement::Completed)))
    }

    fn interrupt<'a>(
        &'a self,
        _handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(futures::future::ready(Ok(())))
    }

    /// A no-op for the same reason `interrupt` is one: this double never
    /// runs a turn, so there is nothing here that a hold could hold back.
    fn pause<'a>(
        &'a self,
        _handle: &'a RunHandle,
        _request: RequestId,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(futures::future::ready(Ok(())))
    }
}

#[derive(Clone, Default)]
struct ChildJournalFixture {
    events: Arc<Mutex<Vec<DomainEvent>>>,
}

impl ChildJournal for ChildJournalFixture {
    fn read(
        &self,
        _invocation: SubagentInvocationId,
    ) -> Result<Vec<DomainEvent>, ChildJournalError> {
        Ok(Vec::new())
    }
}

impl SessionStore for ChildJournalFixture {
    fn append(&self, _session: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.events
            .lock()
            .expect("child events")
            .push(event.clone());
        Ok(())
    }

    fn read(
        &self,
        _session: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        Ok(Box::pin(stream::iter(
            self.events
                .lock()
                .expect("child events")
                .clone()
                .into_iter()
                .map(SessionEntry::Event)
                .map(Ok),
        )))
    }

    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _expected_revision: Option<ego_core::run::RunRevision>,
        _transaction: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        Err(StoreError::Io(std::io::Error::other(
            "parent hook journal does not store run transactions",
        )))
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        Ok(Vec::new())
    }
}

struct ChildTasks;

impl ChildTaskReader for ChildTasks {
    fn read_rendered_task(&self, _reference: &BlobRef) -> Result<Vec<u8>, ChildResultReaderError> {
        Ok(BODY.to_vec())
    }
}

struct ChildResults;

impl ChildResultReader for ChildResults {
    fn read_final_assistant(
        &self,
        _run: RunId,
    ) -> Result<ChildResultEvidence, ChildResultReaderError> {
        let bytes = b"child result".to_vec();
        Ok(ChildResultEvidence::new(
            BlobRef::new(
                BlobId::parse(ego_core::digest::sha256_hex(&bytes)).expect("result id"),
                bytes.len() as u64,
            ),
            bytes,
        ))
    }
}

struct ParentResults;

impl ChildResultStore for ParentResults {
    fn copy_to_parent(
        &self,
        _child: RunId,
        evidence: &ChildResultEvidence,
    ) -> Result<BlobRef, ChildResultReaderError> {
        Ok(evidence.item().clone())
    }
}

struct ChildProfile;

impl ChildProfileResolver for ChildProfile {
    fn resolve_profile(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct ChildSelection {
    waits: Arc<Mutex<Vec<ChildWait>>>,
}

impl ChildSelectionAdmission for ChildSelection {
    fn admit(
        &self,
        request: &ChildRequest,
        bindings: &RunExecutionBindings,
    ) -> Result<(RunSelections, RunExecutionBindings), String> {
        self.waits.lock().expect("child waits").push(request.wait());
        Ok((request.selections().clone(), bindings.clone()))
    }
}

#[derive(Clone, Default)]
struct RecordingHookPort {
    invocations: Arc<Mutex<usize>>,
}

impl HookInvocationPort for RecordingHookPort {
    fn invoke<'a>(&'a self, _request: HookInvocationRequest) -> HookFuture<'a> {
        *self.invocations.lock().expect("hook invocations") += 1;
        Box::pin(futures::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
    }
}

#[derive(Clone, Copy)]
struct ParentToolOrigin {
    turn: TurnId,
    item: ItemId,
    event: EventId,
}

struct ChildServiceInvocationPort {
    child: ChildRunService,
    parent: SessionHookRuntime<'static>,
}

impl SkillInvocationPort for ChildServiceInvocationPort {
    fn invoke<'a>(
        &'a self,
        invocation: SkillInvocation,
        parent_turn: TurnId,
        parent_item: ItemId,
        parent_event: EventId,
        effect_tools: BTreeSet<String>,
    ) -> BoxFuture<'a, Result<SkillInvocationOutcome, ego_core::skill::SkillError>> {
        Box::pin(async move {
            let Some(wait) = invocation.fork_wait() else {
                return Ok(SkillInvocationOutcome::Parent(Box::new(invocation)));
            };
            let id = invocation.subagent_invocation();
            let selections = RunSelections::new(
                invocation.model().map(|model| ModelRef(model.to_owned())),
                invocation
                    .effort()
                    .and_then(ego_core::skill::parse_skill_effort),
                None,
                None,
                invocation.profile().map(str::to_owned),
            );
            let request = ChildRequest::new(
                id,
                ChildRequestOrigin::new(
                    self.child.parent_run(),
                    parent_turn,
                    parent_item,
                    parent_event,
                ),
                invocation
                    .rendered_ref()
                    .cloned()
                    .expect("rendered child task"),
                selections,
                wait,
            )
            .with_requested_tools(effect_tools);
            let admission = self
                .child
                .start(request, &self.parent)
                .await
                .map_err(|error| ego_core::skill::SkillError::new(error.to_string()))?;
            if wait == ChildWait::Foreground {
                if let Some(execution) = self
                    .child
                    .take_execution(id)
                    .map_err(|error| ego_core::skill::SkillError::new(error.to_string()))?
                {
                    self.child
                        .execute(execution)
                        .await
                        .map_err(|error| ego_core::skill::SkillError::new(error.to_string()))?;
                }
                return Ok(SkillInvocationOutcome::Foreground(
                    admission
                        .wait(&self.parent)
                        .await
                        .map_err(|error| ego_core::skill::SkillError::new(error.to_string()))?,
                ));
            }
            Ok(SkillInvocationOutcome::Background(id))
        })
    }
}

struct RealChildFixture {
    child: ChildRunService,
    parent: SessionHookRuntime<'static>,
    child_events: Arc<Mutex<Vec<DomainEvent>>>,
    child_waits: Arc<Mutex<Vec<ChildWait>>>,
    origins: [ParentToolOrigin; 2],
}

fn real_child_service() -> RealChildFixture {
    let controller = Arc::new(RunController::new(
        Arc::new(RecordingSessionStore::default()),
        Arc::new(ChildExecutor),
        Arc::new(FixedClock),
        Arc::new(FixedIds),
        RunRetention::new(std::num::NonZeroUsize::new(4).expect("retention")).expect("retention"),
    ));
    let gate = SharedBudgetGate::new(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    ));
    let bindings =
        RunExecutionBindings::new(Arc::new(NullFactory), None).with_budget_gate(gate.clone());
    let child_journal = ChildJournalFixture::default();
    let child_events = Arc::clone(&child_journal.events);
    let child_selection = ChildSelection::default();
    let child_waits = Arc::clone(&child_selection.waits);
    let parent_run = RunId::new(SessionId::new(), uuid::Uuid::now_v7());
    let ports = ChildServicePorts::new(
        parent_run,
        Arc::new(child_journal.clone()),
        Arc::new(ChildTasks),
        Arc::new(ChildResults),
        Arc::new(ParentResults),
        Arc::new(ChildProfile),
        Arc::new(child_selection),
    );
    let journal_store: &'static ChildJournalFixture = Box::leak(Box::new(child_journal));
    let clock: &'static FixedClock = Box::leak(Box::new(FixedClock));
    let sink: &'static NullSink = Box::leak(Box::new(NullSink));
    let shared = SharedJournal::new(Journal::new(
        parent_run.session_id(),
        journal_store,
        clock,
        sink,
        Producer::Main,
        SessionState::default(),
    ));
    let turn = TurnId::new();
    let mut origins = [
        ParentToolOrigin {
            turn,
            item: ItemId::new(),
            event: EventId::new(),
        },
        ParentToolOrigin {
            turn,
            item: ItemId::new(),
            event: EventId::new(),
        },
    ];
    for (ordinal, origin) in origins.iter_mut().enumerate() {
        let event = shared
            .append(
                origin.turn,
                Some(origin.item),
                None,
                EventKind::ToolCallStarted {
                    name: "skill".to_owned(),
                    args: "{}".to_owned(),
                    call_id: Some(format!("skill-parent-{ordinal}")),
                },
            )
            .expect("parent tool origin");
        origin.event = event;
    }
    let parent_state = SessionHookState::new(
        HookLifecycleDispatcher::new(
            HookRunner::new(),
            HookCatalogue::admit_typed(&[], &[], Trust::Trusted).expect("empty hook catalogue"),
        ),
        shared,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(60),
    );
    let parent = parent_state.bind(Arc::new(RecordingHookPort::default()));
    RealChildFixture {
        child: ChildRunService::new(
            controller,
            gate,
            RunRoot::new("/workspace").expect("root"),
            bindings,
            ports,
        ),
        parent,
        child_events,
        child_waits,
        origins,
    }
}
