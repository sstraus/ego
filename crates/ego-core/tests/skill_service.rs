//! RED contract for the typed skill operation and authority epoch (M4 S6).
//!
//! These tests deliberately cross the existing blob, session-journal, turn,
//! permit, child, and hook seams. They do not define a second skill parser,
//! authority object, or effect runner.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use serde_json::json;

use ego_core::budget::BudgetGate;
use ego_core::config::{ModelRef, Trust};
use ego_core::event::domain::{DomainEvent, EventKind};
use ego_core::event::hook::{
    HookActivation, HookDecision, HookInvocationId, HookLifecycle, HookOutcome, HookPlan,
    HookResult, HookSource,
};
use ego_core::event::id::{Producer, SessionId, SubagentInvocationId};
use ego_core::event::skill::{
    SkillContentDigest, SkillDescriptorEvidence, SkillId, SkillInvocationId, SkillSourceAuthority,
    SkillSourceLocation,
};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{EvidenceError, EvidenceWrite};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::hook::{
    HookCatalogue, HookConfigEntry, HookEvent, HookFuture, HookHandlerConfig,
    HookLifecycleDispatcher, HookOccurrence, HookPort, HookReachFuture, HookReachedEvidence,
    HookRecordFuture, HookRunner, HookSpec, HookStartFuture, HookStartedEvidence,
};
use ego_core::permit::{GuardedEffects, GuardedIo, Op, PreparedCall};
use ego_core::run::{
    EventSinkFactory, IdSource, RecoveryEvidence, RunController, RunExecutionBindings, RunExecutor,
    RunExecutorError, RunFuture, RunHandle, RunId, RunPreparation, RunPrepared, RunRetention,
    RunRoot, RunSelections, RunSettlement, RunStart,
};
use ego_core::sink::NullSink;
use ego_core::skill::{
    ActiveSkillScope, SkillCatalogue, SkillInvocation, SkillOperation, SkillSpec,
};
use ego_core::store::blob::{
    BlobError, BlobId, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter,
};
use ego_core::store::evidence::{
    EvidenceError as StoreEvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::subagent::{
    ChildJournal, ChildJournalError, ChildProfileResolver, ChildRequest, ChildResultEvidence,
    ChildResultReader, ChildResultReaderError, ChildResultStore, ChildRunService,
    ChildSelectionAdmission, ChildServicePorts, ChildTaskReader, ChildWait,
};
use ego_core::turn::{Journal, RegisteredTool};

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
        _deadline: Option<std::time::Instant>,
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
    fn execute(
        &self,
        _connect: ego_core::permit::ResolvedConnect,
        _request: ego_core::permit::HttpRequest,
        _deadline: Option<std::time::Instant>,
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
    let mut journal = Journal::new(
        session,
        &store,
        &clock,
        &sink,
        Producer::Main,
        Default::default(),
    );
    let first = catalogue("explain", DIGEST_A);
    let invocation = SkillInvocation::admit(
        &first,
        SkillOperation::Invoke {
            name: "explain".to_owned(),
            arguments: "captured".to_owned(),
        },
        &blobs,
        &mut journal,
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
    let resumed = SkillInvocation::resume(invocation.id(), &changed, &blobs, &mut journal).unwrap();
    assert_eq!(resumed.admission_blob(&blobs).unwrap(), admission);
    assert_eq!(
        resumed.rendered(&blobs).unwrap(),
        invocation.rendered(&blobs).unwrap()
    );
}

#[test]
fn resource_reads_cross_prepared_call_and_guarded_effects_before_durable_bytes() {
    let path = ego_core::permit::CanonPath::new("/workspace/skills/explain/refs/data.txt");
    let prepared = PreparedCall::new("skill_resource", vec![Op::Read { path: path.clone() }]);
    assert!(prepared.ops.iter().any(|op| matches!(op, Op::Read { .. })));

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
    let policy = ego_core::permit::PolicyEngine::new(
        ego_core::permit::CanonPath::new("/workspace"),
        ego_core::config::Mode::Default,
        ego_core::config::Ruleset::default(),
    );
    let fs = ego_core::permit::fake::FakeFs::default();
    let process = NoProcess;
    let evidence = NoEvidence::default();
    let tools = ego_core::turn::Tools::from_registry(
        &registered,
        &policy,
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
fn skill_service_bridges_real_child_service_for_both_fork_modes() {
    let (child, child_events, child_waits) = real_child_service();
    let service = ego_core::skill::SkillService::new(child);
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
        spec("explain", DIGEST_A, true, true),
        SkillOperation::Invoke {
            name: "explain".to_owned(),
            arguments: "background".to_owned(),
        },
    )
    .expect("typed background service invocation");
    let background_id = background_invocation.id();

    let foreground = service
        .fork(foreground_invocation, ChildWait::Foreground)
        .expect("foreground skill fork");
    let background = service
        .fork(background_invocation, ChildWait::Background)
        .expect("background skill fork");
    assert_eq!(foreground.wait_mode(), ChildWait::Foreground);
    assert_eq!(background.wait_mode(), ChildWait::Background);
    assert_ne!(foreground_id, background_id);
    let child_events = child_events.lock().expect("child journal");
    assert!(
        child_events
            .iter()
            .any(|event| matches!(&event.kind, EventKind::SubagentRequested { .. }))
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

    fn start<'a>(
        &'a self,
        request: &'a RunStart,
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
        _prompt: &'a str,
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
}

#[derive(Clone, Default)]
struct ChildJournalFixture {
    events: Arc<Mutex<Vec<DomainEvent>>>,
}

impl ChildJournal for ChildJournalFixture {
    fn append(&self, event: DomainEvent) -> Result<(), ChildJournalError> {
        self.events.lock().expect("child events").push(event);
        Ok(())
    }

    fn read(
        &self,
        _invocation: SubagentInvocationId,
    ) -> Result<Vec<DomainEvent>, ChildJournalError> {
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
    started: Arc<Mutex<Vec<(HookActivation, HookSource)>>>,
}

impl HookPort for RecordingHookPort {
    fn reached<'a>(
        &'a self,
        producer: &'a Producer,
        occurrence: &'a HookOccurrence,
        plan: &'a HookPlan,
    ) -> HookReachFuture<'a> {
        let evidence = HookReachedEvidence::new(
            ego_core::event::id::EventId::new(),
            occurrence.key().clone(),
            occurrence.event().subject(),
            producer.clone(),
            plan.clone(),
        );
        Box::pin(futures::future::ready(Ok(
            ego_core::hook::HookReachOutcome::Fresh(evidence),
        )))
    }

    fn started<'a>(
        &'a self,
        spec: &'a HookSpec,
        _occurrence: &'a HookOccurrence,
        reached: &'a HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> HookStartFuture<'a> {
        self.started
            .lock()
            .expect("hook starts")
            .push((activation, spec.source()));
        Box::pin(futures::future::ready(Ok(HookStartedEvidence::new(
            reached.event(),
            reached.event(),
            spec.id(),
            invocation,
            activation,
        ))))
    }

    fn invoke<'a>(&'a self, _spec: &'a HookSpec, _event: HookEvent) -> HookFuture<'a> {
        *self.invocations.lock().expect("hook invocations") += 1;
        Box::pin(futures::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
    }

    fn settled<'a>(
        &'a self,
        _spec: &'a HookSpec,
        _started: HookStartedEvidence,
        _result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
        Box::pin(futures::future::ready(Ok(())))
    }
}

type RealChildService = (
    ChildRunService,
    Arc<Mutex<Vec<DomainEvent>>>,
    Arc<Mutex<Vec<ChildWait>>>,
);

fn real_child_service() -> RealChildService {
    let controller = Arc::new(RunController::new(
        Arc::new(RecordingSessionStore::default()),
        Arc::new(ChildExecutor),
        Arc::new(FixedClock),
        Arc::new(FixedIds),
        RunRetention::new(std::num::NonZeroUsize::new(4).expect("retention")).expect("retention"),
    ));
    let gate = Arc::new(Mutex::new(BudgetGate::unlimited(
        ego_core::cost::CurrencyCode::new("USD").expect("currency"),
    )));
    let bindings =
        RunExecutionBindings::new(Arc::new(NullFactory), None).with_budget_gate(Arc::clone(&gate));
    let child_journal = ChildJournalFixture::default();
    let child_events = Arc::clone(&child_journal.events);
    let child_selection = ChildSelection::default();
    let child_waits = Arc::clone(&child_selection.waits);
    let ports = ChildServicePorts::new(
        RunId::new(SessionId::new(), uuid::Uuid::now_v7()),
        Arc::new(child_journal),
        Arc::new(ChildTasks),
        Arc::new(ChildResults),
        Arc::new(ParentResults),
        Arc::new(ChildProfile),
        Arc::new(child_selection),
        Arc::new(HookLifecycleDispatcher::new(
            HookRunner::new(RecordingHookPort::default()),
            HookCatalogue::admit_typed(&[], &[], Trust::Trusted).expect("empty hook catalogue"),
        )),
        Producer::Main,
    );
    (
        ChildRunService::new(
            controller,
            gate,
            RunRoot::new("/workspace").expect("root"),
            bindings,
            ports,
        ),
        child_events,
        child_waits,
    )
}
