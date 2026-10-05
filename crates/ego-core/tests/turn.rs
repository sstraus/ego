//! The turn loop: what a turn writes down, and what it does when something
//! goes wrong.
//!
//! See `docs/decisions/2026-08-21-the-turn-loop.md` and
//! `plans/m0b-vertical-slice.md` Step 10.

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::ffi::OsString;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use ego_core::budget::{
    BudgetGate, Ledger, Metering, PricingQuote, ResolvedRates, SettlementSource, SharedBudgetGate,
};
use ego_core::config::{
    ConfigLayer, Mode, ModelRef, PermCfg, Ruleset, Source, SystemPrompt, Trust,
};
use ego_core::cost::{
    Amount, CostAttribution, CostDelta, CurrencyCode, MeteringRevision, Money, PricingRevision,
    ProviderCostRevision, RateSource, UnknownCost,
};
use ego_core::event::domain::{
    CancelCause, DomainEvent, Envelope, EventKind, ExitReason, GenerationRole,
};
use ego_core::event::hook::{HookDecision, HookLifecycle, HookOutcome};
use ego_core::event::id::{
    EventId, ItemId, Producer, QueuedPromptId, Seq, SessionId, SubagentInvocationId, TurnId,
};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::evidence::{
    EvidenceAdmissionService, EvidenceError as AdmissionError, EvidenceKey, EvidenceRecord,
    EvidenceStore as AdmissionStore, EvidenceWrite, RunInvocationId,
};
use ego_core::generator::attempt::{AttemptStream, GenerationAttempt};
use ego_core::generator::capabilities::{
    CacheRetention, CancelSemantics, ModelCapabilities, ReasoningCarryover, ReasoningEffortSupport,
    Transport,
};
use ego_core::generator::fake::FakeGenerator;
use ego_core::generator::history::conversation_from;
use ego_core::generator::port::{AiEvent, Generator, RequestExpressibilityError};
use ego_core::generator::types::{
    ContentBlock, ErrorKind, GenRequest, ReasoningState, Role, StopReason, ThinkingLevel,
    ToolDescriptor,
};
use ego_core::hook::{
    HookCatalogue, HookCausality, HookConfigEntry, HookFuture, HookHandlerConfig,
    HookInvocationBinding, HookInvocationPort, HookInvocationRequest, HookLifecycleDispatcher,
    HookRunner, SessionHookState,
};
use ego_core::mcp::{ForeignQuestionPort, McpRuntimeBinding, ToolExecution};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    Authority, CanonPath, Confinement, EnvPolicy, ExecCaps, Fs, GuardedIo, HumanApprover, OneHop,
    OneHopResponse, Op, PermissionDecision, PermissionRequest, PolicyEngine, PrepareCtx,
    PreparedCall, ProcessError, ProcessExecutor, ResolvedConnect, Resolver, RunConfinement,
    SpawnSpec, ToolError, ToolResult, TransportError, Typed, TypedTool,
};
use ego_core::public::PublicEvent;
use ego_core::run::RunTermination;
use ego_core::run::{RequestId, RunAppend, RunRevision, RunTransaction};
use ego_core::sink::{CollectingSink, EventSink};
use ego_core::state::{ItemKind, PauseState, SessionState, Usage};
use ego_core::store::blob::{BlobId, BlobRef};
use ego_core::store::evidence::{
    EvidenceError, EvidencePage, EvidenceRead, EvidenceStore, EvidenceWriter,
};
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::subagent::{ChildWait, ChildWaitResult};
use ego_core::suspension::ResumeFloor;
use ego_core::turn::{
    CancelToken, ControlError, GenerationPolicyCapture, GenerationPolicySnapshot,
    GenerationPolicySource, GenerationRuntime, Journal, JudgeContext, PauseToken, RegisteredTool,
    Session, SharedJournal, ToolEffectPorts, Tools, TurnConfig, TurnLoop as CoreTurnLoop,
};
use futures::StreamExt;
use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

mod common;

mod turn {
    mod destructive;
    use super::*;
    use ego_core::permit::fake::{FakeFs, FakeWriteTool};

    struct TurnLoop;

    impl TurnLoop {
        fn open<'a>(
            session: Session<'a>,
            runtime: GenerationRuntime,
            tools: Tools<'a>,
        ) -> CoreTurnLoop<'a, 'a> {
            CoreTurnLoop::new(session, runtime, tools).expect("synchronized budget gate")
        }
    }

    struct NoResolver;

    impl Resolver for NoResolver {
        fn resolve(
            &self,
            _target: ego_core::permit::NetworkTarget,
            _deadline: std::time::Instant,
        ) -> BoxFuture<'static, Result<ego_core::permit::DnsResolution, TransportError>> {
            panic!("turn test unexpectedly resolved an HTTP target")
        }
    }

    struct NoHop;

    impl OneHop for NoHop {
        fn begin_call(&self) -> std::time::Instant {
            std::time::Instant::now() + std::time::Duration::from_secs(60)
        }
        fn execute(
            &self,
            _connect: ResolvedConnect,
            _request: ego_core::permit::HttpRequest,
            _deadline: std::time::Instant,
        ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
            panic!("turn test unexpectedly executed an HTTP hop")
        }
    }

    struct NoDuplex;

    impl ego_core::permit::DuplexSpawner for NoDuplex {
        fn spawn(
            &self,
            _spec: ego_core::permit::SpawnSpec,
        ) -> BoxFuture<'static, Result<ego_core::permit::DuplexHandle, TransportError>> {
            panic!("turn test unexpectedly spawned a duplex")
        }
    }

    /// A `SessionStore` that keeps the log in memory, and can be told to fail
    /// on the nth append — which is the only way to test what the loop does
    /// when the log cannot be written.
    #[derive(Default)]
    struct MemStore {
        events: Mutex<Vec<DomainEvent>>,
        appends: AtomicUsize,
        /// Fail the append with this 1-based index. `0` never fails.
        fail_at: AtomicUsize,
        fail_prompt_queued: AtomicBool,
        permission_requested: Arc<AtomicBool>,
    }

    impl MemStore {
        fn fail_at(&self, nth: usize) {
            self.fail_at.store(nth, Ordering::SeqCst);
        }

        fn fail_prompt_queued(&self) {
            self.fail_prompt_queued.store(true, Ordering::SeqCst);
        }

        fn kinds(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|ev| ev.kind.name().to_string())
                .collect()
        }

        fn events(&self) -> Vec<DomainEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl SessionStore for MemStore {
        fn append(&self, _id: &SessionId, ev: &DomainEvent) -> Result<(), StoreError> {
            let nth = self.appends.fetch_add(1, Ordering::SeqCst) + 1;
            if nth == self.fail_at.load(Ordering::SeqCst)
                || (self.fail_prompt_queued.load(Ordering::SeqCst)
                    && matches!(ev.kind, EventKind::PromptQueued { .. }))
            {
                return Err(StoreError::Io(std::io::Error::other("the disk said no")));
            }
            self.events.lock().unwrap().push(ev.clone());
            if matches!(ev.kind, EventKind::PermissionRequested { .. }) {
                self.permission_requested.store(true, Ordering::SeqCst);
            }
            Ok(())
        }

        fn compare_and_append_run(
            &self,
            _session: &SessionId,
            _expected_revision: Option<RunRevision>,
            _transaction: RunTransaction,
        ) -> Result<RunAppend, StoreError> {
            Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this event-only fixture does not accept run transactions",
            )))
        }

        fn read(
            &self,
            _id: &SessionId,
        ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
            unimplemented!("the turn loop only appends")
        }

        fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
            unimplemented!("the turn loop only appends")
        }
    }

    /// A clock that does not move. Envelope timestamps are observation time,
    /// not ordering (`event/time.rs`), so a turn that runs in an instant is a
    /// perfectly valid thing to log.
    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(self.0)
        }
    }

    /// A generator that cancels the turn part-way through its own stream.
    ///
    /// [`FakeGenerator`] covers every other test here — it is the scripted
    /// `Generator` this crate already ships. What it cannot express is a
    /// cancellation arriving *during* a generation, which is the one case
    /// where settling the attempt is hard, so that case gets a fake of its
    /// own rather than a hook bolted onto the general one.
    /// A generator that fires a caller-supplied signal once the stream has
    /// yielded `after` events, and only then finishes.
    ///
    /// The signal is a closure rather than a token, because the two things a
    /// turn can be asked mid-stream — stop, and stop-and-hold — are answered
    /// by two different tokens and one identical piece of scaffolding.
    struct SignallingGenerator {
        events: Mutex<Vec<AiEvent>>,
        /// Fire the signal once the stream has yielded this many events.
        after: usize,
        signal: Arc<dyn Fn() + Send + Sync>,
    }

    /// A human seat whose answers are explicit and finite. Exhausting the
    /// script is a test defect: production EOF is represented by an explicit
    /// fail-closed `Deny`, never by the approver inventing an answer.
    #[derive(Debug)]
    struct ScriptedHuman {
        answers: Mutex<VecDeque<PermissionDecision>>,
    }

    #[derive(Debug)]
    struct RequestObservingHuman {
        permission_requested: Arc<AtomicBool>,
    }

    impl HumanApprover for RequestObservingHuman {
        fn decide<'a>(
            &'a self,
            _item: ItemId,
            _request: &'a PermissionRequest,
        ) -> BoxFuture<'a, PermissionDecision> {
            assert!(
                self.permission_requested.load(Ordering::SeqCst),
                "PermissionRequested must be durable before the human is called"
            );
            Box::pin(futures::future::ready(PermissionDecision::Allow))
        }
    }

    #[derive(Debug)]
    struct PendingHuman;

    impl HumanApprover for PendingHuman {
        fn decide<'a>(
            &'a self,
            _item: ItemId,
            _request: &'a PermissionRequest,
        ) -> BoxFuture<'a, PermissionDecision> {
            Box::pin(futures::future::pending())
        }
    }

    impl ScriptedHuman {
        fn new(answers: impl IntoIterator<Item = PermissionDecision>) -> Self {
            Self {
                answers: Mutex::new(answers.into_iter().collect()),
            }
        }
    }

    fn forked_descriptor(model_invocable: bool) -> ego_core::event::skill::SkillDescriptorEvidence {
        let location = ego_core::event::skill::SkillSourceLocation::parse("skills/forked/SKILL.md")
            .expect("skill source location");
        let source_digest = ego_core::event::skill::SkillContentDigest::parse(
            "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
        )
        .expect("skill source digest");
        ego_core::event::skill::SkillDescriptorEvidence {
            id: ego_core::event::skill::SkillId::derive(
                ego_core::event::skill::SkillSourceAuthority::User,
                &location,
                "forked",
                &source_digest,
            ),
            name: "forked".to_owned(),
            description: "Run in an isolated child".to_owned(),
            trigger: None,
            argument_hint: None,
            source: ego_core::event::skill::SkillSourceAuthority::User,
            location,
            source_digest,
            user_invocable: true,
            model_invocable,
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SkillPortCall {
        invocation: ego_core::event::skill::SkillInvocationId,
        parent_turn: TurnId,
        parent_item: ItemId,
        parent_event: EventId,
        effect_tools: BTreeSet<String>,
        wait: Option<ChildWait>,
    }

    #[derive(Default)]
    struct RecordingSkillInvocationPort {
        calls: Mutex<Vec<SkillPortCall>>,
    }

    impl RecordingSkillInvocationPort {
        fn calls(&self) -> Vec<SkillPortCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ego_core::skill::SkillInvocationPort for RecordingSkillInvocationPort {
        fn invoke<'a>(
            &'a self,
            invocation: ego_core::skill::SkillInvocation,
            parent_turn: TurnId,
            parent_item: ItemId,
            parent_event: EventId,
            effect_tools: BTreeSet<String>,
        ) -> BoxFuture<
            'a,
            Result<ego_core::skill::SkillInvocationOutcome, ego_core::skill::SkillError>,
        > {
            let wait = invocation.fork_wait();
            self.calls.lock().unwrap().push(SkillPortCall {
                invocation: invocation.id(),
                parent_turn,
                parent_item,
                parent_event,
                effect_tools,
                wait,
            });
            Box::pin(futures::future::ready(Ok(match wait {
                None => ego_core::skill::SkillInvocationOutcome::Parent(Box::new(invocation)),
                Some(ChildWait::Foreground) => {
                    let id = BlobId::parse(
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    )
                    .expect("verified child result blob id");
                    ego_core::skill::SkillInvocationOutcome::Foreground(ChildWaitResult::Returned {
                        final_message: BlobRef::new(id, 19),
                    })
                }
                Some(ChildWait::Background) => ego_core::skill::SkillInvocationOutcome::Background(
                    invocation.subagent_invocation(),
                ),
            })))
        }
    }

    #[test]
    fn typed_user_fork_invocation_creates_child_and_returns_only_verified_child_result() {
        let descriptor = forked_descriptor(false);
        let catalogue = Arc::new(
            ego_core::skill::SkillCatalogue::new(vec![
                ego_core::skill::SkillSpec::new(
                    descriptor.clone(),
                    b"child task: $ARGUMENTS".to_vec(),
                )
                .expect("forked skill")
                .with_fork(ChildWait::Foreground)
                .with_profile("trusted")
                .with_model(ModelRef("admitted-model".to_owned()), "high")
                .with_tool_ceiling(["read"]),
            ])
            .expect("admitted catalogue"),
        );
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "parent must not render the fork body".to_owned(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);
        let skill_evidence = SkillEvidence::new();
        let mut gate = fx.gate.borrow_mut();
        let skill_port = Arc::new(RecordingSkillInvocationPort::default());
        let registry = [
            RegisteredTool::skill(Arc::clone(&catalogue)),
            RegisteredTool::Effect(Arc::new(Typed(FakeReadTool))),
        ];
        let tools = Tools::from_registry(
            &registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &skill_evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        )
        .with_skill_invocation_port(skill_port.clone());
        let operation =
            ego_core::skill::AdmittedSkillOperation::invoke(&descriptor, "typed arguments");
        let outcome = block_on(turn.run_admitted_skill(operation));
        let outcome = outcome.expect("typed fork invocation must complete");
        assert_eq!(
            outcome.skill,
            Some(ego_core::skill::SkillInvocationOutcome::Foreground(
                ChildWaitResult::Returned {
                    final_message: BlobRef::new(
                        BlobId::parse(
                            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                        )
                        .expect("verified child result blob id"),
                        19,
                    ),
                },
            ))
        );
        let events = fx.store.events();
        let admitted = events.iter().find_map(|event| match &event.kind {
            EventKind::SkillInstructionAdmitted { invocation, .. } => {
                Some((*invocation, event.env.id, event.env.turn, event.env.item))
            }
            _ => None,
        });
        let (invocation, admitted_event, admitted_turn, admitted_item) =
            admitted.expect("user fork admission is durable");
        let calls = skill_port.calls();
        assert_eq!(calls.len(), 1, "the product owner receives one user fork");
        assert_eq!(calls[0].invocation, invocation);
        assert_eq!(calls[0].parent_event, admitted_event);
        assert_eq!(calls[0].parent_turn, admitted_turn);
        assert_eq!(calls[0].parent_item, admitted_item.expect("user fork item"));
        assert_eq!(calls[0].effect_tools, BTreeSet::from(["read".to_owned()]));
        assert_eq!(calls[0].wait, Some(ChildWait::Foreground));
        assert_eq!(
            fx.generator.calls().len(),
            0,
            "foreground user fork returns the verified child result, not parent generation"
        );
    }

    #[test]
    fn model_fork_tool_invocation_creates_independent_child_with_durable_identity() {
        let descriptor = forked_descriptor(true);
        let catalogue = Arc::new(
            ego_core::skill::SkillCatalogue::new(vec![
                ego_core::skill::SkillSpec::new(
                    descriptor,
                    b"model child task: $ARGUMENTS".to_vec(),
                )
                .expect("forked skill")
                .with_fork(ChildWait::Background)
                .with_profile("trusted")
                .with_model(ModelRef("admitted-model".to_owned()), "high")
                .with_tool_ceiling(["read"]),
            ])
            .expect("admitted catalogue"),
        );
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "skill-call".to_owned(),
                    name: "skill".to_owned(),
                    args: r#"{"kind":"invoke","name":"forked","arguments":"model arguments"}"#
                        .to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "parent continuation".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ]);
        let skill_evidence = SkillEvidence::new();
        let mut gate = fx.gate.borrow_mut();
        let skill_port = Arc::new(RecordingSkillInvocationPort::default());
        let registry = [
            RegisteredTool::skill(Arc::clone(&catalogue)),
            RegisteredTool::Effect(Arc::new(Typed(FakeReadTool))),
        ];
        let tools = Tools::from_registry(
            &registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &skill_evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        )
        .with_skill_invocation_port(skill_port.clone());
        let outcome = block_on(turn.run("model invokes forked skill"));
        let _outcome = outcome.expect("model fork invocation must complete");
        let events = fx.store.events();
        let admitted = events.iter().find_map(|event| match &event.kind {
            EventKind::SkillInstructionAdmitted { invocation, .. } => {
                Some((*invocation, event.env.id, event.env.turn, event.env.item))
            }
            _ => None,
        });
        let (invocation, admitted_event, admitted_turn, admitted_item) =
            admitted.expect("model fork admission is durable");
        assert!(!invocation.to_string().is_empty());
        let calls = skill_port.calls();
        assert_eq!(calls.len(), 1, "the product owner receives one model fork");
        assert_eq!(calls[0].invocation, invocation);
        assert_eq!(calls[0].parent_event, admitted_event);
        assert_eq!(calls[0].parent_turn, admitted_turn);
        assert_eq!(
            calls[0].parent_item,
            admitted_item.expect("model fork item")
        );
        assert_eq!(calls[0].effect_tools, BTreeSet::from(["read".to_owned()]));
        assert_eq!(calls[0].wait, Some(ChildWait::Background));
        assert_eq!(
            SubagentInvocationId::from_uuid(calls[0].invocation.uuid())
                .expect("durable child identity")
                .to_string(),
            invocation.to_string(),
            "the background outcome retains the durable child identity"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, EventKind::SkillInstructionAdmitted { .. })),
            "forked model skills durably record the admitted invocation"
        );
    }

    #[test]
    fn typed_fork_without_product_owner_fails_closed_before_parent_generation() {
        let descriptor = forked_descriptor(false);
        let catalogue = Arc::new(
            ego_core::skill::SkillCatalogue::new(vec![
                ego_core::skill::SkillSpec::new(
                    descriptor.clone(),
                    b"child task: $ARGUMENTS".to_vec(),
                )
                .expect("forked skill")
                .with_fork(ChildWait::Foreground)
                .with_profile("trusted")
                .with_model(ModelRef("admitted-model".to_owned()), "high")
                .with_tool_ceiling(["read"]),
            ])
            .expect("admitted catalogue"),
        );
        let fx = Fixture::new(vec![]);
        let skill_evidence = SkillEvidence::new();
        let mut gate = fx.gate.borrow_mut();
        let registry = [
            RegisteredTool::skill(Arc::clone(&catalogue)),
            RegisteredTool::Effect(Arc::new(Typed(FakeReadTool))),
        ];
        let tools = Tools::from_registry(
            &registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &skill_evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        );
        let operation = ego_core::skill::AdmittedSkillOperation::invoke(&descriptor, "arguments");
        let abandoned = block_on(turn.run_admitted_skill(operation))
            .expect_err("a fork without a product owner must be refused");
        assert!(
            abandoned
                .cause
                .to_string()
                .contains("skill invocation has no product runtime owner")
        );
        assert!(
            fx.generator.calls().is_empty(),
            "a fail-closed fork must not activate parent generation"
        );
        assert!(
            fx.store
                .events()
                .iter()
                .any(|event| matches!(&event.kind, EventKind::SkillInstructionAdmitted { .. }))
        );
    }

    #[test]
    fn model_fork_without_product_owner_completes_with_a_failed_tool_result() {
        let descriptor = forked_descriptor(true);
        let catalogue = Arc::new(
            ego_core::skill::SkillCatalogue::new(vec![
                ego_core::skill::SkillSpec::new(
                    descriptor,
                    b"model child task: $ARGUMENTS".to_vec(),
                )
                .expect("forked skill")
                .with_fork(ChildWait::Background)
                .with_profile("trusted")
                .with_model(ModelRef("admitted-model".to_owned()), "high")
                .with_tool_ceiling(["read"]),
            ])
            .expect("admitted catalogue"),
        );
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "skill-call".to_owned(),
                    name: "skill".to_owned(),
                    args: r#"{"kind":"invoke","name":"forked","arguments":"model arguments"}"#
                        .to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "parent continuation".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ]);
        let skill_evidence = SkillEvidence::new();
        let mut gate = fx.gate.borrow_mut();
        let registry = [
            RegisteredTool::skill(Arc::clone(&catalogue)),
            RegisteredTool::Effect(Arc::new(Typed(FakeReadTool))),
        ];
        let tools = Tools::from_registry(
            &registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &skill_evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        );
        let outcome = block_on(turn.run("model invokes forked skill"))
            .expect("missing product owner is reported as a failed tool result");
        assert_eq!(outcome.exit_reason, ExitReason::Completed);
        let events = fx.store.events();
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, EventKind::SkillInstructionAdmitted { .. }))
        );
        let failed_tool = events
            .iter()
            .find(|event| matches!(&event.kind, EventKind::ToolCallCompleted { ok: false, .. }));
        assert!(
            failed_tool.is_some(),
            "a missing product owner must complete the model tool call as failed"
        );
        assert!(events.iter().all(|event| !matches!(
            &event.kind,
            EventKind::SubagentRequested { .. } | EventKind::SubagentSpawned { .. }
        )));
    }

    impl HumanApprover for ScriptedHuman {
        fn decide<'a>(
            &'a self,
            _item: ItemId,
            _request: &'a PermissionRequest,
        ) -> BoxFuture<'a, PermissionDecision> {
            let answer = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("the test supplied one human answer per question");
            Box::pin(futures::future::ready(answer))
        }
    }

    impl Generator for SignallingGenerator {
        fn ensure_expressible(&self, _req: &GenRequest) -> Result<(), RequestExpressibilityError> {
            Ok(())
        }

        fn generate(&self, _req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
            let events = std::mem::take(&mut *self.events.lock().unwrap());
            let after = self.after;
            let signal = Arc::clone(&self.signal);
            let stream =
                futures::stream::iter(events.into_iter().enumerate()).map(move |(at, event)| {
                    if at + 1 == after {
                        (signal)();
                    }
                    event
                });
            AttemptStream::new(attempt, Box::pin(stream))
        }
    }

    /// A request id a host could have minted, stable per test.
    fn request_id(n: u128) -> RequestId {
        RequestId::from_uuid(uuid::Uuid::from_u128(n))
    }

    /// The mid-stream signal that cancels a turn.
    fn cancels(token: &CancelToken) -> Arc<dyn Fn() + Send + Sync> {
        let token = token.clone();
        Arc::new(move || token.cancel(CancelCause::User))
    }

    /// The mid-stream signal that asks a turn to hold at its next boundary.
    fn pauses(token: &PauseToken, request: RequestId) -> Arc<dyn Fn() + Send + Sync> {
        let token = token.clone();
        Arc::new(move || token.pause(request))
    }

    /// A generator that says something and then never finishes.
    ///
    /// The only way to prove *live* rather than *eventually*: a batch handed
    /// over at turn end and a sink fed as events happen are indistinguishable
    /// once the turn is over, so the turn must be caught while it is still
    /// running. This one yields its events and then pends forever, so a
    /// single poll of `run` provably cannot have returned.
    struct StallingGenerator {
        events: Mutex<Vec<AiEvent>>,
    }

    impl Generator for StallingGenerator {
        fn ensure_expressible(&self, _req: &GenRequest) -> Result<(), RequestExpressibilityError> {
            Ok(())
        }

        fn generate(&self, _req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
            let events = std::mem::take(&mut *self.events.lock().unwrap());
            let stream = futures::stream::iter(events).chain(futures::stream::pending());
            AttemptStream::new(attempt, Box::pin(stream))
        }
    }

    struct ContinuouslyReadyGenerator;

    impl Generator for ContinuouslyReadyGenerator {
        fn ensure_expressible(&self, _req: &GenRequest) -> Result<(), RequestExpressibilityError> {
            Ok(())
        }

        fn generate(&self, _req: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
            AttemptStream::new(
                attempt,
                Box::pin(
                    futures::stream::repeat(AiEvent::TextDelta {
                        text: "ready delta".to_owned(),
                    })
                    .take(32),
                ),
            )
        }
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct ReadParams {
        path: String,
    }

    /// A read-only tool, the shape Step 11 ships for real: it resolves its
    /// path at `prepare` and reads it through `GuardedIo`, never through `Fs`.
    struct FakeReadTool;

    impl TypedTool for FakeReadTool {
        type Params = ReadParams;

        fn name(&self) -> &str {
            "read"
        }

        fn description(&self) -> &str {
            "reads a file"
        }

        fn prepare_typed(
            &self,
            params: ReadParams,
            ctx: &PrepareCtx<'_>,
        ) -> Result<PreparedCall, ToolError> {
            Ok(PreparedCall::new(
                self.name().to_string(),
                Op::Read {
                    path: ctx.resolver.canonicalize(Path::new(&params.path))?,
                },
            ))
        }

        fn execute<'a, 'io>(
            &'a self,
            call: PreparedCall,
            io: &'a dyn GuardedIo<'io>,
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
        where
            'io: 'a,
        {
            Box::pin(async move {
                let Some(Op::Read { path }) = call.into_parts().1.into_iter().next() else {
                    return Err(ToolError::InvalidParams("expected one Read op".into()));
                };
                let bytes = io.read(&path)?;
                Ok(ToolResult::builtin(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ))
            })
        }
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct ExecParams {
        argv: Vec<String>,
    }

    struct FakeExecTool;

    impl TypedTool for FakeExecTool {
        type Params = ExecParams;

        fn name(&self) -> &str {
            "exec"
        }

        fn description(&self) -> &str {
            "executes a process"
        }

        fn prepare_typed(
            &self,
            params: ExecParams,
            _ctx: &PrepareCtx<'_>,
        ) -> Result<PreparedCall, ToolError> {
            let spec = SpawnSpec::new(
                params.argv.into_iter().map(OsString::from).collect(),
                None,
                None,
                EnvPolicy::Inherited,
            )
            .map_err(|err| ToolError::InvalidParams(err.to_string()))?;
            Ok(PreparedCall::new(
                self.name().to_string(),
                Op::Spawn { spec },
            ))
        }

        fn execute<'a, 'io>(
            &'a self,
            call: PreparedCall,
            io: &'a dyn GuardedIo<'io>,
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
        where
            'io: 'a,
        {
            Box::pin(async move {
                let Some(Op::Spawn { spec }) = call.into_parts().1.into_iter().next() else {
                    return Err(ToolError::InvalidParams("expected one Spawn op".into()));
                };
                io.exec(&spec).await.map_err(ToolError::from)
            })
        }
    }

    struct GatedCancellableProcess {
        calls: Arc<AtomicUsize>,
        released: Arc<AtomicBool>,
        release_seen: Arc<AtomicBool>,
    }

    impl ProcessExecutor for GatedCancellableProcess {
        fn execute<'a>(
            &'a self,
            _spec: &'a ego_core::permit::SpawnSpec,
        ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let released = Arc::clone(&self.released);
            let release_seen = Arc::clone(&self.release_seen);
            Box::pin(async move {
                futures::future::poll_fn(move |cx| {
                    if released.load(Ordering::SeqCst) && release_seen.swap(true, Ordering::SeqCst)
                    {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                Err(ProcessError::Cancelled)
            })
        }
    }

    struct GatedReadTool {
        released: Arc<AtomicBool>,
        release_seen: Arc<AtomicBool>,
    }

    impl TypedTool for GatedReadTool {
        type Params = ReadParams;

        fn name(&self) -> &str {
            "builtin_read"
        }

        fn description(&self) -> &str {
            "reads through a non-cancellable builtin"
        }

        fn prepare_typed(
            &self,
            params: ReadParams,
            ctx: &PrepareCtx<'_>,
        ) -> Result<PreparedCall, ToolError> {
            Ok(PreparedCall::new(
                self.name().to_string(),
                Op::Read {
                    path: ctx.resolver.canonicalize(Path::new(&params.path))?,
                },
            ))
        }

        fn execute<'a, 'io>(
            &'a self,
            call: PreparedCall,
            io: &'a dyn GuardedIo<'io>,
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
        where
            'io: 'a,
        {
            let released = Arc::clone(&self.released);
            let release_seen = Arc::clone(&self.release_seen);
            Box::pin(async move {
                futures::future::poll_fn(move |cx| {
                    if released.load(Ordering::SeqCst) && release_seen.swap(true, Ordering::SeqCst)
                    {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                let Some(Op::Read { path }) = call.into_parts().1.into_iter().next() else {
                    return Err(ToolError::InvalidParams("expected one Read op".into()));
                };
                let bytes = io.read(&path)?;
                Ok(ToolResult::builtin(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ))
            })
        }
    }

    struct CancellingReadTool {
        started: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
        released: Arc<AtomicBool>,
        release_seen: Arc<AtomicBool>,
    }

    impl TypedTool for CancellingReadTool {
        type Params = ReadParams;

        fn name(&self) -> &str {
            "cancel_after_read"
        }

        fn description(&self) -> &str {
            "reads once and interrupts the remaining batch"
        }

        fn prepare_typed(
            &self,
            params: ReadParams,
            ctx: &PrepareCtx<'_>,
        ) -> Result<PreparedCall, ToolError> {
            Ok(PreparedCall::new(
                self.name().to_string(),
                Op::Read {
                    path: ctx.resolver.canonicalize(Path::new(&params.path))?,
                },
            ))
        }

        fn execute<'a, 'io>(
            &'a self,
            call: PreparedCall,
            io: &'a dyn GuardedIo<'io>,
        ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
        where
            'io: 'a,
        {
            self.started.fetch_add(1, Ordering::SeqCst);
            let calls = Arc::clone(&self.calls);
            let released = Arc::clone(&self.released);
            let release_seen = Arc::clone(&self.release_seen);
            Box::pin(async move {
                futures::future::poll_fn(move |cx| {
                    if released.load(Ordering::SeqCst) && release_seen.swap(true, Ordering::SeqCst)
                    {
                        Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                let Some(Op::Read { path }) = call.into_parts().1.into_iter().next() else {
                    return Err(ToolError::InvalidParams("expected one Read op".into()));
                };
                let bytes = io.read(&path)?;
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(ToolResult::builtin(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ))
            })
        }
    }

    fn usd(decimal: &str) -> Money {
        Money::new(
            decimal.parse::<Amount>().expect("a valid test amount"),
            CurrencyCode::new("USD").expect("USD is a valid code"),
        )
    }

    fn quote() -> PricingQuote {
        let rate = |d: &str| Some(d.parse::<Amount>().expect("a valid test rate"));
        PricingQuote {
            revision: PricingRevision("test-2026-08".to_string()),
            currency: CurrencyCode::new("USD").expect("USD is a valid code"),
            uncached_input: rate("0.000003"),
            cache_read: rate("0.0000003"),
            cache_write: rate("0.00000375"),
            cache_write_extended: None,
            output: rate("0.000015"),
        }
    }

    /// The fixture quote as a metering: a table that both bounds and
    /// settles. Every turn test here was written against that arrangement;
    /// the other three arrangements are exercised in `tests/metering.rs`.
    fn metering() -> Metering {
        Metering::Currency {
            settlement: SettlementSource::Quote,
        }
    }

    fn caps() -> ModelCapabilities {
        ModelCapabilities {
            schema_version: ModelCapabilities::SCHEMA_VERSION,
            max_context_tokens: Some(200_000),
            max_output_tokens: Some(64_000),
            max_tools: Some(128),
            supports_parallel_tool_calls: true,
            reasoning_carryover: ReasoningCarryover::Unsupported,
            reasoning_efforts: ReasoningEffortSupport::Any,
            reasoning_state_dialect: None,
            cancel: CancelSemantics::Cooperative,
            cache: CacheRetention::Short,
            transport: Transport::Sse,
            image_input: ego_core::generator::capabilities::ImageInputSupport::Unknown,
            default_reasoning_effort: None,
        }
    }

    struct TestGenerationSettings {
        model: String,
        thinking: ThinkingLevel,
        max_output_tokens: Option<u64>,
        parallel_tool_calls: bool,
        system: Option<SystemPrompt>,
        reasoning_efforts: ReasoningEffortSupport,
    }

    /// Default policy source used by legacy turn fixtures after policy became
    /// an explicit per-generation runtime dependency.
    struct FixedPolicySource {
        mode: Mode,
        permissions: Ruleset,
        human: Option<Arc<dyn HumanApprover>>,
    }

    fn default_policy_source() -> Arc<FixedPolicySource> {
        Arc::new(FixedPolicySource {
            mode: Mode::Default,
            permissions: Ruleset::default(),
            human: None,
        })
    }

    impl GenerationPolicySource for FixedPolicySource {
        fn current(&self) -> Result<Arc<PolicyEngine>, String> {
            Ok(Arc::new(match &self.human {
                Some(human) => PolicyEngine::with_human(
                    CanonPath::new("/ws"),
                    self.mode,
                    self.permissions.clone(),
                    Arc::clone(human),
                ),
                None => {
                    PolicyEngine::new(CanonPath::new("/ws"), self.mode, self.permissions.clone())
                }
            }))
        }

        fn capture(&self) -> Result<GenerationPolicyCapture, String> {
            GenerationPolicyCapture::new(
                CanonPath::new("/ws"),
                self.mode,
                self.permissions.clone(),
                self.human.clone(),
            )
            .map_err(|error| error.to_string())
        }

        fn restore(
            &self,
            snapshot: Arc<GenerationPolicySnapshot>,
        ) -> Result<GenerationPolicyCapture, String> {
            GenerationPolicyCapture::restore(snapshot, self.human.clone())
                .map_err(|error| error.to_string())
        }
    }

    fn config() -> TestGenerationSettings {
        TestGenerationSettings {
            model: "test-model".to_string(),
            thinking: ThinkingLevel::Off,
            max_output_tokens: Some(1_024),
            parallel_tool_calls: false,
            system: None,
            reasoning_efforts: ReasoningEffortSupport::Any,
        }
    }

    fn gate() -> BudgetGate {
        BudgetGate::new(usd("1000"))
    }

    fn committed(gate: &SharedBudgetGate) -> Option<Money> {
        match gate.snapshot().expect("budget snapshot").ledger() {
            Ledger::Unlimited => None,
            Ledger::Limited { committed, .. } => Some(*committed),
        }
    }

    fn outstanding(gate: &SharedBudgetGate) -> Option<Money> {
        match gate.snapshot().expect("budget snapshot").ledger() {
            Ledger::Unlimited => None,
            Ledger::Limited { outstanding, .. } => Some(*outstanding),
        }
    }

    struct NoProcess;

    impl ProcessExecutor for NoProcess {
        fn execute<'a>(
            &'a self,
            _spec: &'a ego_core::permit::SpawnSpec,
        ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
            Box::pin(futures::future::ready(Err(ProcessError::Invariant(
                "process execution is not used by core turn tests".to_string(),
            ))))
        }
    }

    struct NoEvidence;

    impl EvidenceStore for NoEvidence {
        fn blob_store(&self) -> &dyn ego_core::store::blob::BlobStore {
            &common::EMPTY_BLOB_STORE
        }

        fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
            Err(EvidenceError::Limit { limit: 0 })
        }

        fn read_page<'a>(
            &'a self,
            request: &'a EvidenceRead,
        ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
            Box::pin(futures::future::ready(Err(EvidenceError::NotFound {
                reference: request.reference.id.clone(),
            })))
        }
    }

    struct NoAdmissionEvidence;

    impl AdmissionStore for NoAdmissionEvidence {
        fn write(
            &self,
            _key: EvidenceKey,
            _record: EvidenceRecord,
        ) -> Result<EvidenceWrite, AdmissionError> {
            Ok(EvidenceWrite::Published)
        }
    }

    static NO_ADMISSION_EVIDENCE: NoAdmissionEvidence = NoAdmissionEvidence;

    #[derive(Clone, Default)]
    struct RecordingHookInvoker {
        lifecycles: Arc<Mutex<Vec<HookLifecycle>>>,
        timeline: Option<Arc<Mutex<Vec<String>>>>,
        block_prompt: bool,
    }

    #[derive(Default)]
    struct RecordingMcpRuntime {
        floors: Mutex<Vec<ResumeFloor>>,
        pending_policy: Mutex<Option<Arc<GenerationPolicySnapshot>>>,
        bound_policies: Mutex<Vec<Arc<GenerationPolicySnapshot>>>,
    }

    impl McpRuntimeBinding for RecordingMcpRuntime {
        fn load_pending_foreign_policy(
            &self,
            _continuation_id: &str,
        ) -> Result<Arc<GenerationPolicySnapshot>, String> {
            self.pending_policy
                .lock()
                .unwrap()
                .as_ref()
                .map(Arc::clone)
                .ok_or_else(|| "pending generation policy is unavailable".into())
        }

        fn bind_pending_foreign_continuation(
            &self,
            _continuation_id: &str,
            snapshot: Arc<GenerationPolicySnapshot>,
        ) -> Result<(), String> {
            self.bound_policies.lock().unwrap().push(snapshot);
            Ok(())
        }

        fn hook_invocation_port<'a>(
            &self,
            _policy: Arc<PolicyEngine>,
            _effects: ToolEffectPorts<'a>,
        ) -> Result<Arc<dyn HookInvocationPort + 'a>, String> {
            Ok(Arc::new(RecordingHookInvoker::default()))
        }

        fn foreign_question_port<'session, 'turn>(
            &self,
            runtime: ego_core::hook::SessionHookRuntime<'session>,
            floor: ResumeFloor,
            _effects: ToolEffectPorts<'turn>,
        ) -> Arc<dyn ForeignQuestionPort + 'turn>
        where
            'session: 'turn,
        {
            self.floors.lock().unwrap().push(floor);
            Arc::new(RecordingForeignPort { _runtime: runtime })
        }
    }

    struct RecordingForeignPort<'s> {
        _runtime: ego_core::hook::SessionHookRuntime<'s>,
    }

    impl ForeignQuestionPort for RecordingForeignPort<'_> {
        fn resume<'a>(
            &'a self,
            _continuation_id: &'a str,
            _canonical_json: &'a str,
            _causality: HookCausality,
        ) -> futures::future::BoxFuture<'a, Result<ToolExecution, String>> {
            Box::pin(futures::future::ready(Err("not exercised".into())))
        }
    }

    impl RecordingHookInvoker {
        fn lifecycles(&self) -> Vec<HookLifecycle> {
            self.lifecycles.lock().unwrap().clone()
        }
    }

    impl HookInvocationPort for RecordingHookInvoker {
        fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
            self.lifecycles.lock().unwrap().push(request.lifecycle());
            if let Some(timeline) = &self.timeline {
                timeline.lock().unwrap().push("prompt_hook".into());
            }
            let decision = if self.block_prompt {
                HookDecision::Block
            } else {
                HookDecision::Continue
            };
            Box::pin(futures::future::ready(Ok(HookOutcome::new(decision, None))))
        }
    }

    /// Evidence facade for the direct-skill tests: skill admission writes its
    /// two durable blobs through the generic blob port, while ordinary tool
    /// evidence remains unavailable in this fixture.
    struct SkillEvidence {
        blobs: common::MemoryBlobStore,
    }

    impl SkillEvidence {
        fn new() -> Self {
            Self {
                blobs: common::MemoryBlobStore::default(),
            }
        }
    }

    impl EvidenceStore for SkillEvidence {
        fn blob_store(&self) -> &dyn ego_core::store::blob::BlobStore {
            &self.blobs
        }

        fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
            Err(EvidenceError::Limit { limit: 0 })
        }

        fn read_page<'a>(
            &'a self,
            request: &'a EvidenceRead,
        ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
            Box::pin(futures::future::ready(Err(EvidenceError::NotFound {
                reference: request.reference.id.clone(),
            })))
        }
    }

    /// Everything a `TurnLoop` borrows, kept alive in one place so a test can
    /// build a loop in one line without fighting the borrow checker.
    struct Fixture {
        store: MemStore,
        clock: FixedClock,
        generator: Arc<FakeGenerator>,
        policy: PolicyEngine,
        policy_source: Arc<FixedPolicySource>,
        fs: FakeFs,
        process: Arc<dyn ProcessExecutor>,
        evidence: NoEvidence,
        registry: Vec<std::sync::Arc<dyn ego_core::permit::Tool>>,
        gate: RefCell<SharedBudgetGate>,
        /// Where the public projection goes. Collecting rather than null, so
        /// a test can assert on what a host would have seen — which is now
        /// the only way to see it.
        sink: CollectingSink,
    }

    impl Fixture {
        fn new(script: Vec<Vec<AiEvent>>) -> Self {
            let policy_source = Arc::new(FixedPolicySource {
                mode: Mode::Default,
                permissions: Ruleset::default(),
                human: None,
            });
            Self {
                store: MemStore::default(),
                clock: FixedClock(1_787_000_000_000),
                generator: Arc::new(FakeGenerator::new(script)),
                policy: PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default()),
                policy_source,
                fs: FakeFs::default(),
                process: Arc::new(NoProcess),
                evidence: NoEvidence,
                registry: Vec::new(),
                gate: RefCell::new(SharedBudgetGate::new(gate())),
                sink: CollectingSink::new(),
            }
        }

        fn with_read_tool(mut self) -> Self {
            self.registry = vec![std::sync::Arc::new(Typed(FakeReadTool))];
            self
        }

        fn with_exec_tool(mut self) -> Self {
            self.registry = vec![std::sync::Arc::new(Typed(FakeExecTool))];
            self
        }

        fn with_gated_read_tool(&mut self, released: Arc<AtomicBool>) -> Arc<AtomicBool> {
            let release_seen = Arc::new(AtomicBool::new(false));
            self.registry = vec![std::sync::Arc::new(Typed(GatedReadTool {
                released,
                release_seen: Arc::clone(&release_seen),
            }))];
            release_seen
        }

        fn with_cancelling_read_tool(
            &mut self,
            calls: Arc<AtomicUsize>,
            released: Arc<AtomicBool>,
        ) -> Arc<AtomicUsize> {
            let started = Arc::new(AtomicUsize::new(0));
            self.registry = vec![std::sync::Arc::new(Typed(CancellingReadTool {
                started: Arc::clone(&started),
                calls,
                released,
                release_seen: Arc::new(AtomicBool::new(false)),
            }))];
            started
        }

        fn with_process(&mut self, process: Arc<dyn ProcessExecutor>) {
            self.process = process;
        }

        /// A tool that mutates, which is what it takes to reach the third
        /// classification: under the default mode a read inside the workspace
        /// is settled and only a write has nobody to answer for it.
        fn with_write_tool(mut self) -> Self {
            self.registry = vec![std::sync::Arc::new(Typed(FakeWriteTool))];
            self
        }

        fn set_mode(&mut self, mode: Mode) {
            self.policy = PolicyEngine::new(CanonPath::new("/ws"), mode, Ruleset::default());
            self.policy_source = Arc::new(FixedPolicySource {
                mode,
                permissions: Ruleset::default(),
                human: None,
            });
        }

        /// Rules an operator wrote, so a call can be settled at §6 step 3 —
        /// by a rule with an ID and a layer — rather than at step 5 by the
        /// mode. `Project` because that is the layer an operator edits.
        fn with_rules(mut self, allow: &[&str], deny: &[&str]) -> Self {
            let load = |raws: &[&str]| {
                raws.iter()
                    .map(|raw| Rule::load(raw).expect("a rule this build enforces"))
                    .collect()
            };
            let permissions = Ruleset::from_layer(
                PermCfg {
                    judge: None,
                    allow: load(allow),
                    deny: load(deny),
                },
                Source::Project,
            );
            self.policy =
                PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, permissions.clone());
            self.policy_source = Arc::new(FixedPolicySource {
                mode: Mode::Default,
                permissions,
                human: None,
            });
            self
        }

        fn with_human(mut self, answers: impl IntoIterator<Item = PermissionDecision>) -> Self {
            let human: Arc<dyn HumanApprover> = Arc::new(ScriptedHuman::new(answers));
            self.policy = PolicyEngine::with_human(
                CanonPath::new("/ws"),
                Mode::Default,
                Ruleset::default(),
                Arc::clone(&human),
            );
            self.policy_source = Arc::new(FixedPolicySource {
                mode: Mode::Default,
                permissions: Ruleset::default(),
                human: Some(human),
            });
            self
        }

        fn with_request_observing_human(mut self) -> Self {
            let human: Arc<dyn HumanApprover> = Arc::new(RequestObservingHuman {
                permission_requested: Arc::clone(&self.store.permission_requested),
            });
            self.policy = PolicyEngine::with_human(
                CanonPath::new("/ws"),
                Mode::Default,
                Ruleset::default(),
                Arc::clone(&human),
            );
            self.policy_source = Arc::new(FixedPolicySource {
                mode: Mode::Default,
                permissions: Ruleset::default(),
                human: Some(human),
            });
            self
        }

        fn with_pending_human(mut self) -> Self {
            let human: Arc<dyn HumanApprover> = Arc::new(PendingHuman);
            self.policy = PolicyEngine::with_human(
                CanonPath::new("/ws"),
                Mode::Default,
                Ruleset::default(),
                Arc::clone(&human),
            );
            self.policy_source = Arc::new(FixedPolicySource {
                mode: Mode::Default,
                permissions: Ruleset::default(),
                human: Some(human),
            });
            self
        }

        fn with_human_and_rules(
            mut self,
            answers: impl IntoIterator<Item = PermissionDecision>,
            deny: &[&str],
        ) -> Self {
            let denied = deny
                .iter()
                .map(|raw| Rule::load(raw).expect("a rule this build enforces"))
                .collect();
            let permissions = Ruleset::from_layer(
                PermCfg {
                    judge: None,
                    allow: Vec::new(),
                    deny: denied,
                },
                Source::Project,
            );
            let human: Arc<dyn HumanApprover> = Arc::new(ScriptedHuman::new(answers));
            self.policy = PolicyEngine::with_human(
                CanonPath::new("/ws"),
                Mode::Default,
                permissions.clone(),
                Arc::clone(&human),
            );
            self.policy_source = Arc::new(FixedPolicySource {
                mode: Mode::Default,
                permissions,
                human: Some(human),
            });
            self
        }
    }

    fn session<'a>(
        state: SessionState,
        store: &'a dyn SessionStore,
        clock: &'a FixedClock,
        sink: &'a dyn EventSink,
        cancel: CancelToken,
    ) -> Session<'a> {
        Session {
            id: SessionId::new(),
            state,
            store,
            evidence: &NO_ADMISSION_EVIDENCE,
            clock,
            sink,
            cancel,
        }
    }

    /// The fixture quote as the run resolved it: operator-supplied, which is
    /// the only source `models.toml` has and the first of the three §5
    /// consults.
    struct RoutingJudge {
        status: ego_core::turn::RouterStatus,
        calls: AtomicUsize,
    }
    impl ego_core::turn::TurnRouter for RoutingJudge {
        fn route<'a>(&'a self, _text: &'a str) -> BoxFuture<'a, ego_core::turn::RouterOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(futures::future::ready(ego_core::turn::RouterOutcome {
                status: self.status,
                probability: Some(serde_json::from_value(json!(0.9)).unwrap()),
                cost_usd: Some("0.000012642".into()),
            }))
        }
    }

    // Catches: ignored strong choice, fixed fallback changed, per-generation rejudging,
    // model choice leaking across turns, or missing durable cost/outcome metadata.
    #[test]
    fn router_selects_once_per_turn_and_keeps_fixed_fallback_for_non_strong_outcomes() {
        use ego_core::turn::RouterStatus;
        for status in [
            RouterStatus::Strong,
            RouterStatus::Cheap,
            RouterStatus::Grey,
            RouterStatus::Off,
            RouterStatus::Uncalibrated,
            RouterStatus::Timeout,
            RouterStatus::Error,
        ] {
            let script = |call_id: &str| {
                vec![
                    vec![
                        AiEvent::ToolCall {
                            call_id: call_id.into(),
                            name: "read".into(),
                            args: r#"{"path":"/ws/notes.md"}"#.into(),
                        },
                        AiEvent::Stop(StopReason::ToolUse),
                    ],
                    vec![
                        AiEvent::TextDelta {
                            text: "answer".into(),
                        },
                        AiEvent::Stop(StopReason::Stop),
                    ],
                ]
            };
            let fx = Fixture::new([script("read-1"), script("read-2")].concat()).with_read_tool();
            fx.fs.file("/ws/notes.md", "notes");
            let router = Arc::new(RoutingJudge {
                status,
                calls: AtomicUsize::new(0),
            });
            let mut gate = fx.gate.borrow_mut();
            let mut runtime = model_for(&fx, &fx.generator, &mut gate);
            runtime.config.fallbacks = vec![ModelRef("openai/frontier".into())];
            let fixed = config().model;
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut loop_ = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                runtime,
                tools,
            )
            .with_router(router.clone());
            for n in 1..=2 {
                assert!(
                    block_on(loop_.run("inspect notes")).unwrap().ok,
                    "{status:?}, turn {n}"
                );
                assert_eq!(router.calls.load(Ordering::SeqCst), n);
            }
            let expected = if status == RouterStatus::Strong {
                "openai/frontier"
            } else {
                fixed.as_str()
            };
            assert_eq!(
                fx.generator
                    .calls()
                    .iter()
                    .map(|r| r.model.as_str())
                    .collect::<Vec<_>>(),
                vec![expected; 4],
                "{status:?}"
            );
            let decisions: Vec<_> = fx
                .store
                .events()
                .into_iter()
                .filter_map(|event| match event.kind {
                    EventKind::RouterDecision { model, outcome } => Some((model, outcome)),
                    _ => None,
                })
                .collect();
            assert_eq!(decisions.len(), 2);
            for (model, outcome) in decisions {
                assert_eq!(
                    model,
                    (status == RouterStatus::Strong).then(|| ModelRef("openai/frontier".into()))
                );
                assert_eq!(outcome.status, status);
                assert_eq!(outcome.cost_usd.as_deref(), Some("0.000012642"));
                let encoded =
                    serde_json::to_string(&EventKind::RouterDecision { model, outcome }).unwrap();
                assert!(!encoded.contains("inspect notes"));
                assert!(serde_json::from_str::<EventKind>(&encoded).is_ok());
            }
        }
    }

    // Catches: a routed strong candidate becoming its own fixed fallback after refusal,
    // or judging a single-model turn that has no alternative tier to choose.
    #[test]
    fn router_safe_strong_refusal_returns_to_fixed_model_and_single_tier_skips_judge() {
        use ego_core::{generator::attempt::DeliveryCertainty, turn::RouterStatus};
        for has_tiers in [true, false] {
            let mut scripts = Vec::new();
            if has_tiers {
                scripts.push(vec![
                    AiEvent::Error {
                        message: "unavailable".into(),
                        kind: ErrorKind::Refused,
                    },
                    AiEvent::DeliveryObserved(DeliveryCertainty::DefinitelyUnsent),
                    AiEvent::Stop(StopReason::Error),
                ]);
            }
            scripts.push(vec![
                AiEvent::TextDelta {
                    text: "fixed answer".into(),
                },
                AiEvent::Stop(StopReason::Stop),
            ]);
            let fx = Fixture::new(scripts);
            let router = Arc::new(RoutingJudge {
                status: RouterStatus::Strong,
                calls: AtomicUsize::new(0),
            });
            let mut gate = fx.gate.borrow_mut();
            let mut runtime = model_for(&fx, &fx.generator, &mut gate);
            if has_tiers {
                runtime.config.fallbacks = vec![ModelRef("openai/frontier".into())];
            }
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut loop_ = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                runtime,
                tools,
            )
            .with_router(router.clone());
            assert!(block_on(loop_.run("reason about concurrency")).unwrap().ok);
            assert_eq!(router.calls.load(Ordering::SeqCst), usize::from(has_tiers));
            let calls = fx.generator.calls();
            assert_eq!(calls.last().unwrap().model, config().model);
            if has_tiers {
                assert_eq!(calls[0].model, "openai/frontier");
            }
        }
    }

    fn rates() -> ResolvedRates {
        ResolvedRates {
            quote: quote(),
            source: RateSource::Operator,
        }
    }

    struct FixedGenerationSource {
        generator: Arc<dyn Generator>,
        settings: TestGenerationSettings,
        metering: Metering,
        rates: Option<ResolvedRates>,
    }

    impl ego_core::generation_model_source::GenerationModelSource for FixedGenerationSource {
        fn resolve<'a>(
            &'a self,
            request: ego_core::generation_model_source::GenerationModelRequest,
        ) -> BoxFuture<
            'a,
            Result<
                ego_core::generation_model_source::GenerationProfile,
                ego_core::generation_model_source::GenerationModelError,
            >,
        > {
            let mut capabilities = caps();
            capabilities.max_output_tokens = self.settings.max_output_tokens;
            capabilities.supports_parallel_tool_calls = self.settings.parallel_tool_calls;
            capabilities.reasoning_efforts = self.settings.reasoning_efforts.clone();
            let request_model = request
                .model()
                .map_or_else(|| self.settings.model.clone(), |model| model.0.clone());
            Box::pin(futures::future::ready(Ok(
                ego_core::generation_model_source::GenerationProfile {
                    identity: ModelRef(request_model.clone()),
                    request_model,
                    thinking: self.settings.thinking,
                    generator: self.generator.clone(),
                    capabilities,
                    metering: self.metering.clone(),
                    rates: self.rates.clone(),
                },
            )))
        }
    }

    fn model<G: Generator + 'static>(
        generator: &Arc<G>,
        gate: &mut SharedBudgetGate,
    ) -> GenerationRuntime {
        model_with(generator, gate, metering(), Some(rates()), None, config())
    }

    fn model_for<G: Generator + 'static>(
        fx: &Fixture,
        generator: &Arc<G>,
        gate: &mut SharedBudgetGate,
    ) -> GenerationRuntime {
        let mut runtime = model(generator, gate);
        runtime.policy_source = fx.policy_source.clone();
        runtime
    }

    fn model_with_reasoning<G: Generator + 'static>(
        generator: &Arc<G>,
        gate: &mut SharedBudgetGate,
        reasoning_efforts: ReasoningEffortSupport,
    ) -> GenerationRuntime {
        let mut settings = config();
        settings.reasoning_efforts = reasoning_efforts;
        model_with(generator, gate, metering(), Some(rates()), None, settings)
    }

    fn model_with<G: Generator + 'static>(
        generator: &Arc<G>,
        gate: &mut SharedBudgetGate,
        metering: Metering,
        rates: Option<ResolvedRates>,
        token_ceiling: Option<u64>,
        settings: TestGenerationSettings,
    ) -> GenerationRuntime {
        let system = settings.system.clone();
        let source = FixedGenerationSource {
            generator: generator.clone(),
            settings,
            metering,
            rates,
        };
        GenerationRuntime {
            policy_source: default_policy_source(),
            source: Arc::new(source),
            gate: gate.clone(),
            config: TurnConfig {
                cache: ego_core::generator::capabilities::CacheRetention::Short,
                system,
                identity: None,
                token_ceiling,
                retry: 0,
                fallbacks: Vec::new(),
            },
            admission: Arc::new(EvidenceAdmissionService::new()),
            invocation: RunInvocationId::new(),
        }
    }

    /// Run one turn against `fx`, starting from `state`, and hand back where
    /// the session got to.
    fn run(fx: &Fixture, state: SessionState, prompt: &str) -> (ExitReason, bool, SessionState) {
        run_with(fx, state, prompt, metering(), Some(rates()), None, config())
    }

    fn tool_result_text(call: &ego_core::state::ItemState) -> String {
        call.tool_result
            .as_ref()
            .expect("a completed tool call has a semantic result")
            .model_text()
    }

    /// The same, under a metering of the caller's choosing.
    fn run_metered(
        fx: &Fixture,
        state: SessionState,
        prompt: &str,
        metering: Metering,
    ) -> (ExitReason, bool, SessionState) {
        run_with(fx, state, prompt, metering, Some(rates()), None, config())
    }

    /// The same, under a metering, rates, token ceiling and config the
    /// caller chose.
    fn run_with(
        fx: &Fixture,
        state: SessionState,
        prompt: &str,
        metering: Metering,
        rates: Option<ResolvedRates>,
        token_ceiling: Option<u64>,
        config: TestGenerationSettings,
    ) -> (ExitReason, bool, SessionState) {
        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut runtime = model_with(
            &fx.generator,
            &mut gate,
            metering,
            rates,
            token_ceiling,
            config,
        );
        runtime.policy_source = fx.policy_source.clone();
        let mut turn = TurnLoop::open(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            runtime,
            tools,
        );
        let outcome = block_on(turn.run(prompt)).expect("the store accepted every append");
        (
            outcome.exit_reason,
            outcome.ok,
            turn.into_state().expect("unique turn journal state"),
        )
    }

    #[test]
    fn failed_tool_diagnostic_does_not_leak_known_secret_to_model_or_log() {
        // Catches: execution or preparation error-to-result conversion bypasses disclosure masking.
        const SECRET: &str = "example-regression-token";
        struct DiagnosticTool {
            fail_prepare: bool,
        }

        impl TypedTool for DiagnosticTool {
            type Params = ReadParams;

            fn name(&self) -> &str {
                "read"
            }

            fn description(&self) -> &str {
                "reports a failed read diagnostic"
            }

            fn prepare_typed(
                &self,
                params: ReadParams,
                ctx: &PrepareCtx<'_>,
            ) -> Result<PreparedCall, ToolError> {
                if self.fail_prepare {
                    return Err(ToolError::InvalidParams(format!(
                        "credential rejected: {SECRET}"
                    )));
                }
                FakeReadTool.prepare_typed(params, ctx)
            }

            fn execute<'a, 'io>(
                &'a self,
                _call: PreparedCall,
                _io: &'a dyn GuardedIo<'io>,
            ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
            where
                'io: 'a,
            {
                Box::pin(futures::future::ready(Err(ToolError::InvalidParams(
                    format!("credential rejected: {SECRET}"),
                ))))
            }
        }

        for fail_prepare in [false, true] {
            let mut fx = Fixture::new(vec![
                vec![
                    AiEvent::ToolCall {
                        call_id: "diagnostic".to_owned(),
                        name: "read".to_owned(),
                        args: r#"{"path":"/ws/input"}"#.to_owned(),
                    },
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![AiEvent::Stop(StopReason::Stop)],
            ]);
            fx.registry = vec![Arc::new(Typed(DiagnosticTool { fail_prepare }))];
            fx.fs.file("/ws/input", "input");
            let mut budget = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                )
                .with_secret_values(ego_core::secrets::SecretValues::new([SECRET
                    .as_bytes()
                    .to_vec()])),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                model_for(&fx, &fx.generator, &mut budget),
                tools,
            );
            let outcome = block_on(turn.run("read the input")).expect("turn completes");
            assert_eq!(outcome.exit_reason, ExitReason::Completed);
            let events = fx.store.events();
            let diagnostic = events
                .iter()
                .find_map(|event| match &event.kind {
                    EventKind::ToolCallCompleted { ok: false, result } => Some(result.model_text()),
                    _ => None,
                })
                .expect("failed tool result is durable");
            let calls = fx.generator.calls();
            assert_eq!(calls.len(), 2, "the error is fed back to the model");
            let request = serde_json::to_string(&calls[1]).expect("serialize model request");
            assert!(
                !diagnostic.contains(SECRET) && !request.contains(SECRET),
                "failed diagnostic leaked known secret (prepare={fail_prepare}): log={diagnostic}; model={request}"
            );
            assert!(diagnostic.contains("[REDACTED]"));
        }
    }

    /// Every `seq` in the log, in the order it was written.
    fn seqs_of(fx: &Fixture) -> Vec<Seq> {
        fx.store.events().iter().map(|ev| ev.env.seq).collect()
    }

    /// `n` sequence values counted from the first one. `Seq` has no accessor
    /// on purpose — it is an ordering, not a number — so the expectation is
    /// built the same way the journal builds it.
    fn counting_from_one(n: usize) -> Vec<Seq> {
        let mut seq = Seq::ZERO;
        (0..n).map(|_| seq.next()).collect()
    }

    fn usage() -> Usage {
        tokens(100, 0, 0, 20)
    }

    fn tokens(uncached_input: u64, cache_read: u64, cache_write: u64, output: u64) -> Usage {
        Usage {
            uncached_input_tokens: uncached_input,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            cache_write_extended_tokens: 0,
            output_tokens: output,
            reasoning_tokens: None,
        }
    }

    #[test]
    fn a_plain_turn_writes_the_log_it_promised() {
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "the answer".to_string(),
            },
            AiEvent::Usage(usage()),
            AiEvent::Stop(StopReason::Stop),
        ]]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "what is the answer?");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        assert_eq!(
            fx.store.kinds(),
            vec![
                "user_prompt",
                "generation_started",
                "assistant_delta",
                "usage_updated",
                "assistant_message",
                "attempt_settled",
                // The loop is the only place that sees a generation boundary,
                // so it is the only place that can say how full the context
                // was at one.
                "context_window_updated",
                "turn_completed",
            ]
        );
    }

    #[test]
    fn the_log_is_numbered_in_the_order_it_was_written() {
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "hi".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);

        run(&fx, SessionState::default(), "hello");

        assert_eq!(seqs_of(&fx), counting_from_one(fx.store.events().len()));
    }

    #[test]
    fn a_second_turn_keeps_counting_where_the_first_stopped() {
        let fx = Fixture::new(vec![
            vec![AiEvent::Stop(StopReason::Stop)],
            vec![AiEvent::Stop(StopReason::Stop)],
        ]);

        let (_, _, state) = run(&fx, SessionState::default(), "first");
        let first_last = state.last_seq;
        let (_, _, state) = run(&fx, state, "second");

        assert!(
            state.last_seq > first_last,
            "a resumed session must not restart its numbering: {:?} then {:?}",
            first_last,
            state.last_seq
        );
        assert_eq!(seqs_of(&fx), counting_from_one(fx.store.events().len()));
    }

    #[test]
    fn every_timestamp_comes_from_the_injected_clock() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        run(&fx, SessionState::default(), "hello");

        for event in fx.store.events() {
            assert_eq!(
                event.env.ts,
                Timestamp::from_millis(1_787_000_000_000),
                "an envelope read a clock this test did not inject"
            );
        }
    }

    // Catches: task batching moves a child read ahead of an earlier parent write,
    // making the child analyze stale workspace contents.
    #[test]
    fn task_child_reads_the_write_that_precedes_it_in_the_parent_batch() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "parent-write".into(),
                    name: "fake_write".into(),
                    args: json!({"path":"/ws/note.txt", "contents":"new contents"}).to_string(),
                },
                AiEvent::ToolCall {
                    call_id: "child-task".into(),
                    name: "task".into(),
                    args: json!({"name":"reviewer", "prompt":"Read note.txt"}).to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_rules(&["write(*)"], &[]);
        fx.fs.file("/ws/note.txt", "old contents");
        let registry = vec![
            RegisteredTool::Effect(Arc::new(Typed(FakeWriteTool))),
            RegisteredTool::Effect(Arc::new(Typed(FakeReadTool))),
            RegisteredTool::Task,
        ];
        let tools = Tools::from_registry(
            &registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut gate = fx.gate.borrow_mut();
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        );
        let outcome = block_on(turn.run("Update note.txt, then ask the reviewer to read it"))
            .expect("store accepts the batch");
        assert!(outcome.ok);
        let calls = fx.generator.calls();
        assert_eq!(calls.len(), 2, "a refused mixed batch must not run a child");
        let feedback: Vec<_> = calls[1]
            .messages
            .messages()
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    call_id,
                    output,
                    is_error,
                    ..
                } => Some((call_id.as_str(), output.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(feedback.len(), 2, "every mixed-batch call needs feedback");
        for (call_id, output, is_error) in feedback {
            assert!(matches!(call_id, "parent-write" | "child-task"));
            assert!(is_error);
            assert_eq!(
                output,
                "task calls cannot be mixed with other tool calls in one provider response"
            );
        }
        assert_eq!(
            fx.fs.read(&CanonPath::new("/ws/note.txt")).unwrap(),
            b"old contents",
            "a refused batch must not write"
        );
    }

    #[test]
    fn a_tool_call_and_its_result_reach_the_next_request() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_abc".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/note.txt"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "it says hello".to_string(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_read_tool();
        fx.fs.file("/ws/note.txt", "hello from the file");

        let (reason, ok, state) = run(&fx, SessionState::default(), "what is in note.txt?");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);

        // The call ran, and its output was logged against the item that
        // started it.
        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the tool call became an item");
        assert_eq!(call.call_id.as_deref(), Some("call_abc"));
        assert_eq!(call.args, r#"{"path":"/ws/note.txt"}"#);
        assert_eq!(tool_result_text(call), "hello from the file");
        assert_eq!(call.ok, Some(true));

        // And the second generation was handed a history containing both
        // halves — the round trip this whole step exists for.
        let second = &fx.generator.calls()[1];
        let blocks: Vec<&ContentBlock> = second
            .messages
            .messages()
            .iter()
            .flat_map(|message| message.content.iter())
            .collect();
        assert!(blocks.iter().any(|block| matches!(
            block,
            ContentBlock::ToolUse { call_id, name, .. } if call_id == "call_abc" && name == "read"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            ContentBlock::ToolResult { call_id, output, is_error }
                if call_id == "call_abc" && output == "hello from the file" && !is_error
        )));
    }

    #[test]
    fn a_queued_prompt_forces_a_follow_up_after_a_no_tool_final() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![
            vec![
                AiEvent::TextDelta {
                    text: "the final answer".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "the queued follow-up".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ]);
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        let mut submit = Box::pin(ingress.submit("deliver this after the final".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(submit.as_mut().poll(&mut cx), Poll::Pending));
        let (outcome, accepted) =
            block_on(async { futures::join!(turn.run("initial prompt"), submit) });
        let outcome = outcome.expect("the first no-tool turn closes normally");
        let accepted = accepted.expect("the queue acceptance is durable before acknowledgement");
        assert_eq!(accepted.pending_steers, 1);
        assert_eq!(accepted.queued, 0);
        assert!(!accepted.interrupt_pending);
        let events = fx.store.events();
        let first_assistant = events
            .iter()
            .position(|event| {
                matches!(
                    &event.kind,
                    EventKind::AssistantMessage { text } if text == "the final answer"
                )
            })
            .expect("the active turn records its first final answer");
        assert_eq!(fx.generator.calls().len(), 2);
        let correlated = events
            .iter()
            .enumerate()
            .find_map(|(index, event)| match &event.kind {
                EventKind::UserPrompt {
                    text,
                    queued_prompt: Some(_),
                    ..
                } if text == "deliver this after the final" => Some(index),
                _ => None,
            })
            .expect("the normal steer is durably correlated in this turn");
        let second_assistant = events
            .iter()
            .position(|event| {
                matches!(
                    &event.kind,
                    EventKind::AssistantMessage { text } if text == "the queued follow-up"
                )
            })
            .expect("the correlated follow-up is generated in this turn");
        let completed = events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 1, "normal delivery remains one turn");
        let completed_index = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
            .expect("the same turn closes after the follow-up");
        assert!(first_assistant < correlated);
        assert!(correlated < second_assistant);
        assert!(second_assistant < completed_index);
        assert_eq!(events[completed_index].env.turn, outcome.turn);
        let generations = fx.generator.calls();
        let blocks: Vec<&ContentBlock> = generations[1]
            .messages
            .messages()
            .iter()
            .flat_map(|message| message.content.iter())
            .collect();
        assert!(blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Text { text } if text == "deliver this after the final"
        )));
    }

    #[test]
    fn an_ordinary_sibling_batch_delivers_a_pending_steer_at_the_safe_boundary() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let calls = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "ordinary-first".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/first.txt"}"#.to_owned(),
                },
                AiEvent::ToolCall {
                    call_id: "ordinary-second".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/second.txt"}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "ordinary sibling follow-up".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_rules(&["read(**)"], &[]);
        let started = fx.with_cancelling_read_tool(Arc::clone(&calls), Arc::clone(&released));
        fx.fs.file("/ws/first.txt", "first");
        fx.fs.file("/ws/second.txt", "second");

        let mut turn_config = config();
        turn_config.parallel_tool_calls = true;
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_with(
                &fx.generator,
                &mut budget,
                metering(),
                Some(rates()),
                None,
                turn_config,
            ),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("run ordinary siblings");
        let drive = async {
            futures::future::poll_fn(|cx| {
                if started.load(Ordering::SeqCst) >= 1 {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            let accepted = ingress
                .submit("ordinary sibling follow-up".to_owned())
                .await
                .expect("the ordinary steer is durably accepted while the first sibling runs");
            assert_eq!(accepted.pending_steers, 1);
            assert_eq!(accepted.queued, 0);
            assert!(!accepted.interrupt_pending);
            released.store(true, Ordering::SeqCst);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("ordinary sibling delivery completes");
        assert_eq!(outcome.exit_reason, ExitReason::Completed);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(fx.generator.calls().len(), 2);
        let events = fx.store.events();
        let completions: Vec<usize> = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                matches!(event.kind, EventKind::ToolCallCompleted { .. }).then_some(index)
            })
            .collect();
        assert_eq!(completions.len(), 2, "both sibling calls complete");
        let correlated = events
            .iter()
            .enumerate()
            .find_map(|(index, event)| match &event.kind {
                EventKind::UserPrompt {
                    text,
                    queued_prompt: Some(_),
                    ..
                } if text == "ordinary sibling follow-up" => Some(index),
                _ => None,
            })
            .expect("the ordinary follow-up is correlated at the safe boundary");
        assert!(completions.into_iter().all(|index| index < correlated));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
                .count(),
            1,
            "ordinary delivery stays in one turn"
        );
        let generations = fx.generator.calls();
        let blocks: Vec<&ContentBlock> = generations[1]
            .messages
            .messages()
            .iter()
            .flat_map(|message| message.content.iter())
            .collect();
        assert!(blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Text { text } if text == "ordinary sibling follow-up"
        )));
    }

    #[test]
    fn partial_correlated_delivery_refreshes_status_after_each_durable_append() {
        let (first, second) = (QueuedPromptId::new(), QueuedPromptId::new());
        let seeded_turn = TurnId::new();
        let mut seq = Seq::default();
        let mut seeded = |kind| DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn: seeded_turn,
                item: None,
                parent: None,
                ts: Timestamp::from_millis(1_787_000_000_000),
                producer: Producer::Main,
            },
            kind,
        };
        let durable = vec![
            seeded(EventKind::PromptQueued {
                id: first,
                text: "first recovery prompt".to_owned(),
                images: Vec::new(),
            }),
            seeded(EventKind::PromptQueued {
                id: second,
                text: "second recovery prompt".to_owned(),
                images: Vec::new(),
            }),
        ];
        let fx = Fixture::new(vec![]);
        fx.store
            .events
            .lock()
            .unwrap()
            .extend(durable.iter().cloned());
        fx.store.fail_at(2);
        let mut state = SessionState::default();
        for event in &durable {
            state.apply(event);
        }
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model_for(&fx, &fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);

        let abandoned = block_on(turn.run_queued_turn())
            .expect_err("the second correlated UserPrompt append fails");
        drop(turn);
        assert_eq!(abandoned.state.queue_status().pending_steers, 0);
        assert_eq!(abandoned.state.queue_status().queued, 1);
        assert!(
            fx.store.events().iter().any(|event| matches!(
                event.kind,
                EventKind::UserPrompt {
                    queued_prompt: Some(id),
                    ..
                } if id == first
            )),
            "the first correlation remains durable before the second append fails"
        );
        assert!(!fx.store.events().iter().any(|event| matches!(
            event.kind,
            EventKind::UserPrompt {
                queued_prompt: Some(id),
                ..
            } if id == second
        )));
        let mut closed_submit = Box::pin(ingress.submit("after partial failure".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            closed_submit.as_mut().poll(&mut cx),
            Poll::Ready(Err(ControlError::Closed))
        ));
    }

    #[test]
    fn an_attached_empty_queue_run_closes_admission_without_leaking_a_pending_submit() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![]);
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        assert!(
            block_on(turn.run_queued_turn())
                .expect("empty queue run is not a store failure")
                .is_none()
        );
        let _state = turn.into_state().expect("unique turn journal state");
        drop(budget);

        let mut closed_submit = Box::pin(ingress.submit("after empty queue run".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            closed_submit.as_mut().poll(&mut cx),
            Poll::Ready(Err(ControlError::Closed))
        ));
    }

    #[test]
    fn admission_barrier_keeps_prompts_submitted_before_and_after_turn_completion() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![
            vec![
                AiEvent::TextDelta {
                    text: "first final".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "first follow-up".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "reopened final".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "reopened follow-up".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ]);
        let mut submit_before = Box::pin(ingress.submit("before completion".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            submit_before.as_mut().poll(&mut cx),
            Poll::Pending
        ));

        let state = {
            let mut budget = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                model_for(&fx, &fx.generator, &mut budget),
                tools,
            )
            .with_interactive_control(&mut control);
            let (outcome, accepted) =
                block_on(async { futures::join!(turn.run("initial prompt"), submit_before) });
            assert_eq!(
                outcome
                    .expect("the before-boundary prompt does not interrupt the turn")
                    .exit_reason,
                ExitReason::Completed
            );
            assert_eq!(
                accepted
                    .expect("the before-boundary prompt is durably admitted")
                    .pending_steers,
                1
            );
            let state = turn.into_state().expect("unique turn journal state");
            assert_eq!(state.queue_status().queued, 0);
            state
        };

        let mut closed_submit = Box::pin(ingress.submit("after close".to_owned()));
        assert!(matches!(
            closed_submit.as_mut().poll(&mut cx),
            Poll::Ready(Err(ControlError::Closed))
        ));
        let (outcome, accepted) = {
            let mut budget = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
                model_for(&fx, &fx.generator, &mut budget),
                tools,
            )
            .with_interactive_control(&mut control);
            let mut reopened_submit = Box::pin(ingress.submit("after reopen".to_owned()));
            assert!(matches!(
                reopened_submit.as_mut().poll(&mut cx),
                Poll::Pending
            ));
            block_on(async { futures::join!(turn.run("reopened prompt"), reopened_submit) })
        };
        let outcome = outcome.expect("the reopened prompt does not interrupt the turn");
        let accepted = accepted.expect("the reopened prompt is durably admitted");
        assert_eq!(accepted.pending_steers, 1);
        assert_eq!(fx.generator.calls().len(), 4);
        assert_eq!(outcome.exit_reason, ExitReason::Completed);
        let events = fx.store.events();
        for text in [
            "before completion",
            "first follow-up",
            "after reopen",
            "reopened follow-up",
        ] {
            assert!(
                events.iter().any(|event| matches!(
                    &event.kind,
                    EventKind::UserPrompt { text: actual, .. } if actual == text
                ) || matches!(
                    &event.kind,
                    EventKind::AssistantMessage { text: actual } if actual == text
                )),
                "the admission barrier lost {text:?}"
            );
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
                .count(),
            2,
            "before and after admissions remain two safe-boundary turns"
        );
    }

    #[test]
    fn all_sibling_tool_results_close_before_the_queued_follow_up() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let cancel = CancelToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call-first".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/first.txt"}"#.to_owned(),
                },
                AiEvent::ToolCall {
                    call_id: "call-second".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/second.txt"}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "queued follow-up".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_rules(&["read(**)"], &[]);
        fx.with_cancelling_read_tool(Arc::clone(&calls), Arc::clone(&released));
        fx.fs.file("/ws/first.txt", "first");
        fx.fs.file("/ws/second.txt", "second");

        let mut turn_config = config();
        turn_config.parallel_tool_calls = true;
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                cancel,
            ),
            model_with(
                &fx.generator,
                &mut budget,
                metering(),
                Some(rates()),
                None,
                turn_config,
            ),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("run both reads");
        let drive = async {
            let accepted = ingress
                .submit("after both siblings".to_owned())
                .await
                .expect("sibling follow-up acceptance is durable before acknowledgement");
            assert_eq!(accepted.pending_steers, 1);
            let expedited = ingress
                .interrupt()
                .await
                .expect("sibling follow-up interruption is durable");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
            released.store(true, Ordering::SeqCst);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("sibling results were durably closed");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let events = fx.store.events();
        let completed: Vec<usize> = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                matches!(event.kind, EventKind::ToolCallCompleted { .. }).then_some(index)
            })
            .collect();
        assert_eq!(completed.len(), 2, "both sibling calls must close");
        assert_live_interrupt_order(&fx, |kind| {
            matches!(kind, EventKind::ToolCallCompleted { .. })
        });
        let first_completed = events
            .iter()
            .position(|event| {
                event.env.turn == outcome.turn
                    && matches!(event.kind, EventKind::TurnCompleted { .. })
            })
            .expect("the active sibling batch closes before queue delivery");
        assert!(!events[..first_completed].iter().any(|event| {
            matches!(
                event.kind,
                EventKind::UserPrompt {
                    queued_prompt: Some(_),
                    ..
                }
            )
        }));
        let state = turn.into_state().expect("unique turn journal state");
        drop(budget);
        let mut recovery_budget = fx.gate.borrow_mut();
        let recovery_tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut recovery = TurnLoop::open(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model_for(&fx, &fx.generator, &mut recovery_budget),
            recovery_tools,
        );
        let recovered = block_on(recovery.run_queued_turn())
            .expect("the sibling follow-up remains replayable")
            .expect("the sibling follow-up runs on a fresh turn");
        assert_ne!(recovered.turn, outcome.turn);
        let events = fx.store.events();
        let correlated = events
            .iter()
            .enumerate()
            .find_map(|(index, event)| {
                matches!(
                    event.kind,
                    EventKind::UserPrompt {
                        queued_prompt: Some(_),
                        ..
                    }
                )
                .then_some(index)
            })
            .expect("the follow-up is correlated");
        assert!(first_completed < correlated);
        let calls = fx.generator.calls();
        let blocks: Vec<&ContentBlock> = calls[1]
            .messages
            .messages()
            .iter()
            .flat_map(|message| message.content.iter())
            .collect();
        assert!(blocks.iter().any(|block| matches!(
            block,
            ContentBlock::Text { text } if text == "after both siblings"
        )));
        assert!(
            completed.into_iter().all(|index| index < correlated),
            "queue delivery must not split the sibling call/result batch"
        );
    }

    #[test]
    fn busy_ingress_is_durable_before_ack_and_projects_pending_steer() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![]);
        let generator = Arc::new(StallingGenerator {
            events: Mutex::new(vec![AiEvent::TextDelta {
                text: "streaming".to_owned(),
            }]),
        });
        let mut budget = SharedBudgetGate::new(gate());
        let sink = CollectingSink::new();
        let fs = FakeFs::default();
        let process = NoProcess;
        let evidence = NoEvidence;
        let cancel = CancelToken::new();
        let mut turn = TurnLoop::open(
            session(SessionState::default(), &fx.store, &fx.clock, &sink, cancel),
            model(&generator, &mut budget),
            Tools::new(
                &[],
                ToolEffectPorts::new(
                    &fs,
                    &process,
                    &evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            ),
        )
        .with_interactive_control(&mut control);

        let active = turn.run("initial prompt");
        let drive = async {
            let accepted = ingress
                .submit("durably accepted while the model is busy".to_owned())
                .await
                .expect("acknowledgement follows PromptQueued durability");
            assert_eq!(accepted.pending_steers, 1);
            assert_eq!(accepted.queued, 0);
            assert!(!accepted.interrupt_pending);
            let expedited = ingress
                .interrupt()
                .await
                .expect("expedited intent is durable before interruption");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("interruption settles the active turn");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
    }

    #[test]
    fn a_ready_provider_stream_cannot_starve_a_durable_control_intent() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![]);
        let generator = Arc::new(ContinuouslyReadyGenerator);
        let mut budget = SharedBudgetGate::new(gate());
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);

        let mut submit = Box::pin(ingress.submit("ready-stream steer".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(submit.as_mut().poll(&mut cx), Poll::Pending));
        let active = turn.run("initial prompt");
        let (outcome, accepted) = block_on(async { futures::join!(active, submit) });
        let outcome = outcome.expect("ready-stream control remains durably settled");
        let accepted = accepted.expect("a ready provider must still acknowledge control");
        assert_eq!(accepted.pending_steers, 1);
        let events = fx.store.events();
        let queued = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::PromptQueued { .. }))
            .expect("the ready-stream steer was persisted");
        let delta = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::AssistantDelta { .. }))
            .expect("the ready-stream delta is persisted");
        let completed = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
            .expect("the ready-stream turn closes");
        assert!(queued < delta, "the ack precedes the ready stream");
        assert!(queued < completed, "the ack precedes turn completion");
        assert_eq!(outcome.turn, events[completed].env.turn);
    }

    #[test]
    fn concurrent_ingress_submits_share_global_capacity() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let first_ingress = ingress.clone();
        let second_ingress = ingress.clone();
        let mut first = Box::pin(first_ingress.submit("first concurrent steer".to_owned()));
        let mut second = Box::pin(second_ingress.submit("second concurrent steer".to_owned()));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Pending));
        assert!(
            matches!(second.as_mut().poll(&mut cx), Poll::Pending),
            "the second clone must remain pending while the sole slot is occupied"
        );

        let fx = Fixture::new(vec![]);
        let generator = Arc::new(StallingGenerator {
            events: Mutex::new(vec![AiEvent::TextDelta {
                text: "coordinator consumes one intent at a time".to_owned(),
            }]),
        });
        let mut budget = SharedBudgetGate::new(gate());
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("initial prompt");
        let drive = async {
            let first_status = first.await.expect("the first submit is acknowledged");
            assert_eq!(first_status.pending_steers, 1);
            let second_status = second.await.expect("the second submit follows the first");
            assert_eq!(second_status.pending_steers, 2);
            ingress
                .interrupt()
                .await
                .expect("the shared coordinator accepts the final interrupt");
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        assert_eq!(
            outcome
                .expect("the shared coordinator remains live")
                .exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
    }

    #[test]
    fn interrupt_does_not_append_a_receding_marker_after_an_adversarial_ack() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let first = QueuedPromptId::new();
        let second = QueuedPromptId::new();
        let seeded_turn = TurnId::new();
        let mut seq = Seq::default();
        let mut seeded = |kind| DomainEvent {
            env: Envelope {
                seq: seq.next(),
                id: EventId::new(),
                turn: seeded_turn,
                item: None,
                parent: None,
                ts: Timestamp::from_millis(1_787_000_000_000),
                producer: Producer::Main,
            },
            kind,
        };
        let durable = vec![
            seeded(EventKind::PromptQueued {
                id: first,
                text: "first remains pending".to_owned(),
                images: Vec::new(),
            }),
            seeded(EventKind::PromptQueued {
                id: second,
                text: "second was delivered".to_owned(),
                images: Vec::new(),
            }),
            seeded(EventKind::PromptDeliveryExpedited { through: second }),
            seeded(EventKind::UserPrompt {
                text: "second was delivered".to_owned(),
                images: Vec::new(),
                queued_prompt: Some(second),
            }),
        ];
        let fx = Fixture::new(vec![]);
        fx.store.events.lock().unwrap().extend(durable.clone());
        let mut state = SessionState::default();
        for event in &durable {
            state.apply(event);
        }
        assert_eq!(state.unacknowledged_prompts().len(), 1);
        assert_eq!(state.unacknowledged_prompts()[0].id(), first);
        assert_eq!(state.expedited_through(), Some(second));

        let generator = Arc::new(StallingGenerator {
            events: Mutex::new(vec![AiEvent::TextDelta {
                text: "busy after adversarial ack".to_owned(),
            }]),
        });
        let mut budget = SharedBudgetGate::new(gate());
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model(&generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("resume the live turn");
        let interrupt = async {
            ingress
                .interrupt()
                .await
                .expect("the receding interrupt is still acknowledged durably");
        };
        let (outcome, _) = block_on(async { futures::join!(active, interrupt) });
        assert_eq!(
            outcome
                .expect("the seeded durable state remains writable")
                .exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        let markers: Vec<_> = fx
            .store
            .events()
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::PromptDeliveryExpedited { through } => Some(through),
                _ => None,
            })
            .collect();
        assert_eq!(
            markers,
            vec![second],
            "interrupt must not append through first"
        );
    }

    #[test]
    fn store_failure_before_ack_returns_control_error_and_leaves_queue_unchanged() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![]);
        // The stalled provider leaves the ingress window open before the
        // coordinator appends PromptQueued. Fail that event by kind so this
        // test cannot accidentally target an unrelated lifecycle append.
        fx.store.fail_prompt_queued();
        let generator = Arc::new(StallingGenerator {
            events: Mutex::new(Vec::new()),
        });
        let mut budget = SharedBudgetGate::new(gate());
        let sink = CollectingSink::new();
        let fs = FakeFs::default();
        let process = NoProcess;
        let evidence = NoEvidence;
        let cancel = CancelToken::new();
        let mut turn = TurnLoop::open(
            session(SessionState::default(), &fx.store, &fx.clock, &sink, cancel),
            model(&generator, &mut budget),
            Tools::new(
                &[],
                ToolEffectPorts::new(
                    &fs,
                    &process,
                    &evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            ),
        )
        .with_interactive_control(&mut control);

        let active = turn.run("initial prompt");
        let submit = ingress.submit("must not be acknowledged".to_owned());
        let (outcome, result) = block_on(async { futures::join!(active, submit) });
        let abandoned = outcome.expect_err("a failed queue append abandons the turn");
        assert_eq!(abandoned.state.queue_status().pending_steers, 0);
        assert_eq!(abandoned.state.queue_status().queued, 0);
        let Err(ControlError::Store(message)) = result else {
            panic!("failed queue append must return ControlError::Store: {result:?}");
        };
        assert!(
            !message.is_empty(),
            "the terminal receives the store failure"
        );
        assert!(
            fx.store
                .events()
                .iter()
                .all(|event| !matches!(event.kind, EventKind::PromptQueued { .. }))
        );
        drop(turn);
    }

    #[test]
    fn pending_human_approval_interrupt_decides_false_and_never_executes() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "permission-interrupt".to_owned(),
                    name: "fake_write".to_owned(),
                    args: r#"{"path":"/ws/out.txt","contents":"must not land"}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "queued recovery".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_write_tool()
        .with_pending_human();
        fx.fs.file("/ws/out.txt", "original");
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);

        let active = turn.run("write it");
        let drive = async {
            let accepted = ingress
                .submit("permission follow-up".to_owned())
                .await
                .expect("the permission-phase prompt is durable before acknowledgement");
            assert_eq!(accepted.pending_steers, 1);
            let expedited = ingress
                .interrupt()
                .await
                .expect("the permission-phase interruption is durable");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("the store accepted the cancellation ledger");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        assert_eq!(
            fx.fs.read(&CanonPath::new("/ws/out.txt")).unwrap(),
            b"original"
        );
        assert!(fx.store.events().iter().any(|event| {
            matches!(
                event.kind,
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    ..
                }
            )
        }));
        let current_state = turn.state();
        let call = current_state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the pending call remains visible");
        assert_eq!(call.ok, Some(false));
        assert_live_interrupt_order(&fx, |kind| {
            matches!(
                kind,
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    ..
                } | EventKind::ToolCallCompleted { ok: false, .. }
            )
        });
        let state = turn.into_state().expect("unique turn journal state");
        drop(budget);
        let mut recovery_budget = fx.gate.borrow_mut();
        let recovery_tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut recovery = TurnLoop::open(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model_for(&fx, &fx.generator, &mut recovery_budget),
            recovery_tools,
        );
        let queued = block_on(recovery.run_queued_turn())
            .expect("queued recovery is durable")
            .expect("the accepted permission follow-up runs");
        assert_ne!(queued.turn, outcome.turn);
    }

    #[test]
    fn running_cancellable_process_records_actual_failed_cancellation_result() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let calls = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "exec-cancel".to_owned(),
                    name: "exec".to_owned(),
                    args: r#"{"argv":["long-running"]}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "queued process recovery".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_exec_tool()
        .with_rules(&["exec(*)"], &[]);
        fx.with_process(Arc::new(GatedCancellableProcess {
            calls: Arc::clone(&calls),
            released: Arc::clone(&released),
            release_seen: Arc::new(AtomicBool::new(false)),
        }));

        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("run it");
        let drive = async {
            let accepted = ingress
                .submit("process follow-up".to_owned())
                .await
                .expect("process-phase acceptance is durable before acknowledgement");
            assert_eq!(accepted.pending_steers, 1);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the steer was admitted while the process was running"
            );
            let expedited = ingress
                .interrupt()
                .await
                .expect("process-phase interruption is durable");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
            released.store(true, Ordering::SeqCst);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("the store accepted the process cancellation ledger");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let current_state = turn.state();
        let call = current_state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the process call remains visible");
        assert_eq!(call.ok, Some(false));
        assert!(tool_result_text(call).contains("process was cancelled"));
        assert_live_interrupt_order(&fx, |kind| {
            matches!(kind, EventKind::ToolCallCompleted { ok: false, .. })
        });
        let state = turn.into_state().expect("unique turn journal state");
        drop(budget);
        let mut recovery_budget = fx.gate.borrow_mut();
        let recovery_tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut recovery = TurnLoop::open(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model_for(&fx, &fx.generator, &mut recovery_budget),
            recovery_tools,
        );
        let recovered = block_on(recovery.run_queued_turn())
            .expect("the process follow-up remains replayable")
            .expect("the process follow-up runs on a fresh turn");
        assert_ne!(recovered.turn, outcome.turn);
    }

    #[test]
    fn non_cancellable_builtin_waits_for_release_then_records_actual_success() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let released = Arc::new(AtomicBool::new(false));
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "builtin-read".to_owned(),
                    name: "builtin_read".to_owned(),
                    args: r#"{"path":"/ws/note.txt"}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "queued builtin recovery".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_rules(&["read(**)"], &[]);
        let release_seen = fx.with_gated_read_tool(Arc::clone(&released));
        fx.fs.file("/ws/note.txt", "builtin succeeded");
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut budget),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("read it");
        let drive = async {
            let accepted = ingress
                .submit("builtin follow-up".to_owned())
                .await
                .expect("builtin-phase acceptance is durable before acknowledgement");
            assert_eq!(accepted.pending_steers, 1);
            let expedited = ingress
                .interrupt()
                .await
                .expect("builtin-phase interruption is durable");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
            assert!(
                !fx.store
                    .events()
                    .iter()
                    .any(|event| { matches!(event.kind, EventKind::ToolCallCompleted { .. }) }),
                "the active builtin remains incomplete until its owner releases it"
            );
            assert!(
                !release_seen.load(Ordering::SeqCst),
                "the non-cancellable effect remains active until its owner releases it"
            );
            released.store(true, Ordering::SeqCst);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("the released builtin completes");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        let current_state = turn.state();
        let call = current_state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the builtin call remains visible");
        assert_eq!(call.ok, Some(true));
        assert!(tool_result_text(call).contains("builtin succeeded"));
        assert_live_interrupt_order(&fx, |kind| {
            matches!(kind, EventKind::ToolCallCompleted { ok: true, .. })
        });
        let state = turn.into_state().expect("unique turn journal state");
        drop(budget);
        let mut recovery_budget = fx.gate.borrow_mut();
        let recovery_tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut recovery = TurnLoop::open(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model_for(&fx, &fx.generator, &mut recovery_budget),
            recovery_tools,
        );
        let recovered = block_on(recovery.run_queued_turn())
            .expect("the builtin follow-up remains replayable")
            .expect("the builtin follow-up runs on a fresh turn");
        assert_ne!(recovered.turn, outcome.turn);
    }

    #[test]
    fn three_siblings_keep_current_result_and_close_unstarted_calls_explicitly() {
        let (ingress, mut control) = ego_core::turn::interactive_control();
        let cancel = CancelToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicBool::new(false));
        let mut fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "sibling-1".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/one.txt"}"#.to_owned(),
                },
                AiEvent::ToolCall {
                    call_id: "sibling-2".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/two.txt"}"#.to_owned(),
                },
                AiEvent::ToolCall {
                    call_id: "sibling-3".to_owned(),
                    name: "cancel_after_read".to_owned(),
                    args: r#"{"path":"/ws/three.txt"}"#.to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "queued sibling recovery".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_rules(&["read(**)"], &[]);
        fx.with_cancelling_read_tool(Arc::clone(&calls), Arc::clone(&released));
        fx.fs.file("/ws/one.txt", "one");
        fx.fs.file("/ws/two.txt", "two");
        fx.fs.file("/ws/three.txt", "three");
        let mut turn_config = config();
        turn_config.parallel_tool_calls = true;
        let mut budget = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                cancel,
            ),
            model_with(
                &fx.generator,
                &mut budget,
                metering(),
                Some(rates()),
                None,
                turn_config,
            ),
            tools,
        )
        .with_interactive_control(&mut control);
        let active = turn.run("run siblings");
        let drive = async {
            let accepted = ingress
                .submit("sibling follow-up".to_owned())
                .await
                .expect("sibling-batch acceptance is durable before acknowledgement");
            assert_eq!(accepted.pending_steers, 1);
            let expedited = ingress
                .interrupt()
                .await
                .expect("sibling-batch interruption is durable");
            assert_eq!(expedited.pending_steers, 1);
            assert_eq!(expedited.queued, 0);
            assert!(expedited.interrupt_pending);
            released.store(true, Ordering::SeqCst);
        };
        let (outcome, ()) = block_on(async { futures::join!(active, drive) });
        let outcome = outcome.expect("the store accepted sibling closure");
        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let current_state = turn.state();
        let mut calls: Vec<_> = current_state
            .items
            .values()
            .filter(|item| item.kind == ItemKind::ToolCall)
            .collect();
        calls.sort_by_key(|item| item.call_id.clone());
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].ok, Some(true));
        assert_eq!(calls[1].ok, Some(false));
        assert_eq!(calls[2].ok, Some(false));
        assert!(tool_result_text(calls[1]).contains("interrupted"));
        assert!(tool_result_text(calls[2]).contains("interrupted"));
        assert_live_interrupt_order(&fx, |kind| {
            matches!(kind, EventKind::ToolCallCompleted { .. })
        });
        let state = turn.into_state().expect("unique turn journal state");
        drop(budget);
        let mut recovery_budget = fx.gate.borrow_mut();
        let recovery_tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut recovery = TurnLoop::open(
            session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
            model_for(&fx, &fx.generator, &mut recovery_budget),
            recovery_tools,
        );
        let recovered = block_on(recovery.run_queued_turn())
            .expect("the sibling follow-up remains replayable")
            .expect("the sibling follow-up runs on a fresh turn");
        assert_ne!(recovered.turn, outcome.turn);
    }

    /// A call nobody in this run can answer ends the run, and says so in the
    /// words of a question rather than a refusal.
    ///
    /// `CLI.md` §4.4 gives both endings exit 3, which is exactly why the text
    /// has to differ. "Denied by r1:…" tells an operator which line of their
    /// config to go and look at; "no rule covers write /ws/note.txt" tells
    /// them there is no line to look at and they have to write one. A loop
    /// that collapsed the two would leave the commonest case reading like a
    /// refusal somebody configured on purpose.
    #[test]
    fn a_call_nobody_can_answer_stops_the_run_and_reads_as_a_question() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_abc".to_string(),
                    name: "fake_write".to_string(),
                    args: r#"{"path":"/ws/note.txt","contents":"pwned"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_write_tool();
        fx.fs.file("/ws/note.txt", "the original");

        let (reason, ok, state) = run(&fx, SessionState::default(), "overwrite note.txt");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);

        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the tool call became an item");
        assert_eq!(call.ok, Some(false));
        // "does not approve ... on its own" and not "no rule covers": both are
        // true, and the second is the reason the run stopped. The rules did
        // not cover it, so it went to the mode at §6 step 5, and the mode
        // declined too. The last party to decline is the one whose reason an
        // operator can act on — it is what tells them `--yolo` and `--allow`
        // are both available, where "no rule covers" names only the second.
        let result = tool_result_text(call);
        assert!(
            result.contains("needs permission") && result.contains("does not approve"),
            "an unanswerable call must read as a question: {}",
            result
        );
        assert!(
            result.contains("/ws/note.txt"),
            "and must name what it is a question about: {}",
            result
        );

        assert_eq!(
            fx.generator.calls().len(),
            1,
            "the run stopped, so there is no second generation"
        );
        assert_eq!(
            fx.fs.read(&CanonPath::new("/ws/note.txt")).unwrap(),
            b"the original",
            "a call that was never authorised must not have run"
        );
    }

    /// A system prompt is a setting the whole turn runs under, so it leads
    /// *every* generation — not just the first — and it is never written to
    /// the log. Writing it there would make a resumed session rebuild it as a
    /// message someone sent, and then send it twice.
    #[test]
    fn a_system_prompt_leads_every_generation_and_is_logged_only_as_an_epoch_snapshot() {
        const HOUSE_STYLE: &str = "the house style: no semicolons";

        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_abc".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/note.txt"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();
        fx.fs.file("/ws/note.txt", "hello from the file");

        let mut config = config();
        config.system = Some(SystemPrompt::verbatim(HOUSE_STYLE));
        run_with(
            &fx,
            SessionState::default(),
            "what is in note.txt?",
            metering(),
            Some(rates()),
            None,
            config,
        );

        let calls = fx.generator.calls();
        assert_eq!(calls.len(), 2, "the tool call should force a second pass");
        for (nth, call) in calls.iter().enumerate() {
            let lead = &call.messages.messages()[0];
            assert_eq!(
                lead.role,
                Role::System,
                "generation {nth} led with a {lead:?}"
            );
            assert!(
                matches!(&lead.content[0], ContentBlock::Text { text } if text == HOUSE_STYLE),
                "generation {nth} did not carry the system prompt: {lead:?}"
            );
        }

        let snapshots: Vec<_> = fx
            .store
            .events()
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::GenerationStarted {
                    snapshot: Some(snapshot),
                    ..
                } => Some(snapshot),
                _ => {
                    assert!(
                        !format!("{:?}", event.kind).contains(HOUSE_STYLE),
                        "system text leaked outside the epoch snapshot"
                    );
                    None
                }
            })
            .collect();
        assert_eq!(snapshots.len(), 1, "one durable snapshot per epoch");
        assert_eq!(snapshots[0].system.as_ref().unwrap().text, HOUSE_STYLE);
    }

    /// No system prompt means no system message — not an empty one. A
    /// provider that receives `""` in the system role has been told something.
    #[test]
    fn without_a_system_prompt_a_request_carries_no_system_message() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        run(&fx, SessionState::default(), "hello");

        let roles: Vec<Role> = fx.generator.calls()[0]
            .messages
            .messages()
            .iter()
            .map(|message| message.role)
            .collect();
        assert_eq!(roles, vec![Role::User]);
    }

    #[test]
    fn the_model_is_told_which_tools_it_may_call() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]).with_read_tool();

        run(&fx, SessionState::default(), "hello");

        let names: Vec<String> = fx.generator.calls()[0]
            .tools
            .iter()
            .map(|tool: &ToolDescriptor| tool.name.clone())
            .collect();
        assert_eq!(names, vec!["read".to_string()]);
    }

    #[test]
    fn a_tool_nobody_registered_is_answered_not_raised() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "delete_everything".to_string(),
                    args: "{}".to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();

        let (reason, _, state) = run(&fx, SessionState::default(), "delete everything");

        assert_eq!(reason, ExitReason::Completed);
        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the call was logged even though nothing could run it");
        assert_eq!(call.ok, Some(false));
        let result = tool_result_text(call);
        assert!(
            result.contains("delete_everything"),
            "the refusal must name the tool: {}",
            result
        );
    }

    #[test]
    fn arguments_that_are_not_json_are_answered_not_raised() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    // What a tool call truncated mid-stream looks like.
                    args: r#"{"path":"/ws/no"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();

        let (_, _, state) = run(&fx, SessionState::default(), "read it");

        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the call was logged");
        assert_eq!(call.ok, Some(false));
        let result = tool_result_text(call);
        assert!(
            result.contains("not JSON"),
            "the model has to learn its arguments were malformed: {}",
            result
        );
    }

    #[test]
    fn a_read_outside_the_workspace_is_refused_by_policy() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/etc/passwd"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();
        fx.fs.file("/etc/passwd", "root:x:0:0");

        let (_, _, state) = run(&fx, SessionState::default(), "read /etc/passwd");

        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the call was logged");
        assert_eq!(call.ok, Some(false));
        let result = tool_result_text(call);
        assert!(
            result.contains("/etc/passwd") && result.contains("outside the workspace"),
            "a denial must name what it denied: {}",
            result
        );
        assert!(
            !result.contains("root:x"),
            "the file must not have been read at all: {}",
            result
        );
    }

    /// Bug regression_case: a read outside the workspace ended the whole turn with
    /// `PermissionRequired`, so the model never saw the refusal and an AI Chat
    /// turn died on a file its instructions referenced. The refusal is a
    /// failed tool result and the next generation answers.
    #[test]
    fn a_refused_read_outside_the_workspace_does_not_end_the_turn() {
        let fx = Fixture::new(reading("/etc/passwd")).with_read_tool();
        fx.fs.file("/etc/passwd", "root:x:0:0");

        let (reason, _, state) = run(&fx, SessionState::default(), "read /etc/passwd");

        assert_eq!(reason, ExitReason::Completed);
        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the call was logged");
        assert_eq!(call.ok, Some(false));
        assert!(tool_result_text(call).contains("outside the workspace"));
    }

    /// Everything the log recorded about permissions, in order.
    fn permission_events(fx: &Fixture) -> Vec<EventKind> {
        fx.store
            .events()
            .into_iter()
            .map(|ev| ev.kind)
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::PermissionRequested { .. }
                        | EventKind::PermissionAmended { .. }
                        | EventKind::PermissionDecided { .. }
                )
            })
            .collect()
    }

    fn assert_live_interrupt_order(fx: &Fixture, terminal: impl Fn(&EventKind) -> bool) {
        let events = fx.store.events();
        let queued = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::PromptQueued { .. }))
            .expect("live submit is durable");
        let expedited = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::PromptDeliveryExpedited { .. }))
            .expect("live interrupt is durable");
        let truthful = events
            .iter()
            .position(|event| terminal(&event.kind))
            .expect("the interrupted phase has a truthful terminal event");
        let completed = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::TurnCompleted { .. }))
            .expect("the interrupted turn closes durably");
        assert!(queued < expedited, "acceptance precedes expedition");
        assert!(expedited < truthful, "expedition precedes truthful closure");
        assert!(
            truthful < completed,
            "truthful closure precedes turn closure"
        );
    }

    fn reading(path: &str) -> Vec<Vec<AiEvent>> {
        vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: format!(r#"{{"path":"{path}"}}"#),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ]
    }

    /// §7: *every decision is appended, asked or not.*
    ///
    /// This is the case the audit event exists for. Nobody was asked, nothing
    /// stopped, the file was read — and the log still says which rule, in
    /// which layer, let it happen. Before §7 the only trace was a successful
    /// `ToolCallCompleted`, which records that a read happened and not that
    /// anybody authorised it.
    #[test]
    fn a_call_the_rules_allow_records_the_rule_that_allowed_it() {
        let fx = Fixture::new(reading("/ws/notes.md"))
            .with_read_tool()
            .with_rules(&["read(**)"], &[]);
        fx.fs.file("/ws/notes.md", "hello");

        let (reason, _, _) = run(&fx, SessionState::default(), "read the notes");
        assert_eq!(reason, ExitReason::Completed, "the read was allowed");

        let recorded = permission_events(&fx);
        let [EventKind::PermissionDecided { allowed, by, .. }] = recorded.as_slice() else {
            panic!("one decision, and no question: {recorded:?}");
        };
        assert_eq!(*allowed, Some(true));
        let Some(Authority::Rules { rules }) = by else {
            panic!("a rule settled it at step 3, not the mode at step 5: {by:?}");
        };
        let [rule] = rules.as_slice() else {
            panic!("one op, so one rule: {rules:?}");
        };
        assert_eq!(rule.from, Source::Project, "the layer an operator can edit");
        assert!(
            rule.also.is_empty(),
            "one layer wrote it, so there is no other copy to mention"
        );
    }

    /// The same event, from the other end of §6: no rule matched, so the mode
    /// answered at step 5. `by` says so, and it names the mode rather than
    /// inventing a rule ID for a decision no rule made.
    #[test]
    fn a_call_only_the_mode_allows_names_the_mode_and_not_a_rule() {
        let fx = Fixture::new(reading("/ws/notes.md")).with_read_tool();
        fx.fs.file("/ws/notes.md", "hello");

        run(&fx, SessionState::default(), "read the notes");

        let recorded = permission_events(&fx);
        let [EventKind::PermissionDecided { allowed, by, .. }] = recorded.as_slice() else {
            panic!("one decision, and no question: {recorded:?}");
        };
        assert_eq!(*allowed, Some(true));
        assert_eq!(
            by.as_ref(),
            Some(&Authority::Mode {
                mode: Mode::Default
            })
        );
    }

    /// A refusal at §6 step 0. The boundary is nobody's rule — no `--allow`
    /// widens it and no layer carries it — so the authority has no ID to
    /// record, and recording one would send an operator looking for a line of
    /// config that does not exist.
    #[test]
    fn a_refusal_at_the_boundary_records_an_authority_with_no_rule_to_revoke() {
        let fx = Fixture::new(reading("/etc/passwd")).with_read_tool();
        fx.fs.file("/etc/passwd", "root:x:0:0");

        let (reason, _, _) = run(&fx, SessionState::default(), "read /etc/passwd");
        assert_eq!(reason, ExitReason::Completed);

        let recorded = permission_events(&fx);
        let [EventKind::PermissionDecided { allowed, by, .. }] = recorded.as_slice() else {
            panic!("one decision, and no question: {recorded:?}");
        };
        assert_eq!(*allowed, Some(false));
        assert_eq!(by.as_ref(), Some(&Authority::Boundary));
    }

    fn tool_calls_by_id(
        state: &SessionState,
    ) -> std::collections::HashMap<String, ego_core::state::ItemState> {
        state
            .items
            .values()
            .filter(|item| item.kind == ItemKind::ToolCall)
            .map(|item| (item.call_id.clone().expect("a call id"), item.clone()))
            .collect()
    }

    fn call_event(call_id: &str, name: &str, args: &str) -> AiEvent {
        AiEvent::ToolCall {
            call_id: call_id.to_string(),
            name: name.to_string(),
            args: args.to_string(),
        }
    }

    /// Critic regression_case. Plausible bug: the refused read short-circuits the
    /// batch, so the sibling that follows it is closed unstarted or never
    /// answered.
    #[test]
    fn a_refused_read_does_not_stop_the_sibling_read_after_it() {
        let fx = Fixture::new(vec![
            vec![
                call_event("outside", "read", r#"{"path":"/etc/passwd"}"#),
                call_event("inside", "read", r#"{"path":"/ws/notes.md"}"#),
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();
        fx.fs.file("/etc/passwd", "root:x:0:0");
        fx.fs.file("/ws/notes.md", "hello");

        let (reason, _, state) = run(&fx, SessionState::default(), "read both");

        assert_eq!(reason, ExitReason::Completed);
        let calls = tool_calls_by_id(&state);
        assert_eq!(calls["outside"].ok, Some(false));
        assert_eq!(calls["inside"].ok, Some(true));
        assert_eq!(tool_result_text(&calls["inside"]), "hello");
    }

    /// Critic regression_case. Plausible bug: the refused read's `continue` swallows
    /// the next call's own denial, or lets the write run.
    #[test]
    fn a_write_that_needs_an_answer_after_a_refused_read_still_ends_the_turn() {
        let mut fx = Fixture::new(vec![
            vec![
                call_event("outside", "read", r#"{"path":"/etc/passwd"}"#),
                call_event(
                    "write",
                    "fake_write",
                    r#"{"path":"/ws/out.txt","contents":"x"}"#,
                ),
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ]);
        fx.registry = vec![
            std::sync::Arc::new(Typed(FakeReadTool)),
            std::sync::Arc::new(Typed(FakeWriteTool)),
        ];
        fx.fs.file("/etc/passwd", "root:x:0:0");

        let (reason, ok, state) = run(&fx, SessionState::default(), "read then write");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);
        assert!(fx.fs.read(&CanonPath::new("/ws/out.txt")).is_err());
        let calls = tool_calls_by_id(&state);
        assert!(tool_result_text(&calls["outside"]).contains("outside the workspace"));
    }

    /// Critic regression_case. Plausible bug: the exception widens from "read the
    /// boundary refused" to every `OutsideWorkspace`, so a write outside the
    /// workspace is handed back to the model instead of ending the run.
    #[test]
    fn a_write_outside_the_workspace_still_ends_the_turn() {
        let fx = Fixture::new(writing("/etc/ego-owned", "x")).with_write_tool();

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);
        assert!(fx.fs.read(&CanonPath::new("/etc/ego-owned")).is_err());
    }

    /// Critic regression_case. Plausible bug: the exception widens to every refused
    /// read, so an operator's deny rule no longer ends the run.
    #[test]
    fn a_deny_rule_on_a_read_inside_the_workspace_still_ends_the_turn() {
        let fx = Fixture::new(reading("/ws/secret.txt"))
            .with_read_tool()
            .with_rules(&[], &["read(secret.txt)"]);
        fx.fs.file("/ws/secret.txt", "hunter2");

        let (reason, ok, state) = run(&fx, SessionState::default(), "read the secret");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);
        let calls = tool_calls_by_id(&state);
        assert!(!tool_result_text(&calls["call_1"]).contains("hunter2"));
    }

    /// Critic regression_case. Plausible bug: the exception is keyed on something a
    /// spawn also satisfies, so a command nobody can approve is returned to
    /// the model instead of ending the run.
    #[test]
    fn a_spawn_nobody_can_approve_still_ends_the_turn() {
        let fx = Fixture::new(vec![
            vec![
                call_event("exec", "exec", r#"{"argv":["cat","/etc/passwd"]}"#),
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_exec_tool();

        let (reason, ok, _) = run(&fx, SessionState::default(), "cat it");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);
    }

    /// Critic regression_case. Plausible bug: state left behind by the first refusal
    /// (a call left started, a double `ToolCallCompleted`, a decision recorded
    /// twice) breaks the next generation's refusal.
    #[test]
    fn repeated_refused_reads_each_complete_exactly_once() {
        let fx = Fixture::new(vec![
            vec![
                call_event("a", "read", r#"{"path":"/etc/passwd"}"#),
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                call_event("b", "read", r#"{"path":"/etc/shadow"}"#),
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                call_event("c", "read", r#"{"path":"/root/.ssh/id_rsa"}"#),
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();
        for path in ["/etc/passwd", "/etc/shadow", "/root/.ssh/id_rsa"] {
            fx.fs.file(path, "secret");
        }

        let (reason, _, state) = run(&fx, SessionState::default(), "read them");

        assert_eq!(reason, ExitReason::Completed);
        let calls = tool_calls_by_id(&state);
        for (id, path) in [
            ("a", "/etc/passwd"),
            ("b", "/etc/shadow"),
            ("c", "/root/.ssh/id_rsa"),
        ] {
            assert_eq!(calls[id].ok, Some(false));
            let text = tool_result_text(&calls[id]);
            assert!(text.contains(path), "{text}");
            assert!(!text.contains("secret"), "{text}");
        }
        let completed = fx
            .store
            .events()
            .into_iter()
            .filter(|ev| matches!(ev.kind, EventKind::ToolCallCompleted { .. }))
            .count();
        assert_eq!(completed, 3, "one completion per call");
        let decided = permission_events(&fx)
            .into_iter()
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::PermissionDecided {
                        allowed: Some(false),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(decided, 3, "one boundary decision per refused call");
    }

    /// The mode raised a canonical question and the headless run had no final
    /// seat. The request is still durable even though no decision follows.
    /// Silent rule/mode decisions above remain decisions without requests.
    #[test]
    fn a_question_nobody_can_answer_is_recorded_as_a_question_and_not_a_decision() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "fake_write".to_string(),
                    args: r#"{"path":"/ws/out.txt","contents":"x"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_write_tool();

        let (reason, _, _) = run(&fx, SessionState::default(), "write it");
        assert_eq!(reason, ExitReason::PermissionRequired);

        let recorded = permission_events(&fx);
        let [EventKind::PermissionRequested { tool, escalation }] = recorded.as_slice() else {
            panic!("one question, and no decision: {recorded:?}");
        };
        assert_eq!(tool, "fake_write");
        assert_eq!(
            escalation.to_string(),
            "default mode does not approve write /ws/out.txt on its own",
            "the escalation names the step that asked and the op it asked about"
        );
    }

    fn writing(path: &str, contents: &str) -> Vec<Vec<AiEvent>> {
        vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "fake_write".to_string(),
                    args: format!(r#"{{"path":"{path}","contents":"{contents}"}}"#),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ]
    }

    #[test]
    fn a_human_allow_mints_for_and_executes_the_exact_prepared_call() {
        let fx = Fixture::new(writing("/ws/out.txt", "allowed"))
            .with_write_tool()
            .with_human([PermissionDecision::Allow]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        assert_eq!(
            fx.fs.read(&CanonPath::new("/ws/out.txt")).unwrap(),
            b"allowed"
        );
        let recorded = permission_events(&fx);
        assert!(matches!(
            recorded.as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionDecided {
                    allowed: Some(true),
                    by: Some(Authority::Party),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn a_permission_request_is_durable_before_the_human_is_called() {
        let fx = Fixture::new(writing("/ws/out.txt", "allowed"))
            .with_write_tool()
            .with_request_observing_human();

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        assert!(matches!(
            permission_events(&fx).as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionDecided {
                    allowed: Some(true),
                    by: Some(Authority::Party),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn a_human_deny_is_a_settled_refusal_and_executes_nothing() {
        let fx = Fixture::new(writing("/ws/out.txt", "denied"))
            .with_write_tool()
            .with_human([PermissionDecision::Deny]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);
        assert!(fx.fs.read(&CanonPath::new("/ws/out.txt")).is_err());
        let recorded = permission_events(&fx);
        assert!(matches!(
            recorded.as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    by: Some(Authority::Party),
                    ..
                }
            ]
        ));
    }

    /// Catches a refusal that carries the human's words but ends the run
    /// anyway, so the model never reads what to do instead.
    #[test]
    fn a_human_refusal_with_words_answers_the_call_and_the_run_continues() {
        let fx = Fixture::new(writing("/ws/out.txt", "declined"))
            .with_write_tool()
            .with_human([PermissionDecision::DenyWithInstruction {
                instruction: "write to /ws/other.txt instead".into(),
            }]);

        let (reason, _, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::Completed);
        assert!(fx.fs.read(&CanonPath::new("/ws/out.txt")).is_err());
        let events = fx.store.events();
        let answer = events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::ToolCallCompleted { ok: false, result } => Some(format!("{result:?}")),
                _ => None,
            })
            .expect("the declined call is answered as a failed tool result");
        assert!(
            answer.contains("declined it and said: write to /ws/other.txt instead"),
            "{answer}"
        );
        assert!(matches!(
            permission_events(&fx).as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    by: Some(Authority::Party),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn an_amendment_restarts_at_prepare_and_cannot_cross_the_workspace_boundary() {
        let fx = Fixture::new(writing("/ws/out.txt", "original"))
            .with_write_tool()
            .with_human([PermissionDecision::Amend {
                params: json!({"path": "/etc/ego-owned", "contents": "escaped"}),
            }]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);
        assert!(fx.fs.read(&CanonPath::new("/ws/out.txt")).is_err());
        assert!(fx.fs.read(&CanonPath::new("/etc/ego-owned")).is_err());
        let recorded = permission_events(&fx);
        assert!(matches!(
            recorded.as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionAmended { .. },
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    by: Some(Authority::Boundary),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn an_amendment_restarts_at_classification_and_cannot_cross_a_deny_rule() {
        let fx = Fixture::new(writing("/ws/out.txt", "original"))
            .with_write_tool()
            .with_human_and_rules(
                [PermissionDecision::Amend {
                    params: json!({"path": "/ws/blocked.txt", "contents": "blocked"}),
                }],
                &["write(blocked.txt)"],
            );

        let (reason, ok, _) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::PermissionRequired);
        assert!(!ok);
        assert!(fx.fs.read(&CanonPath::new("/ws/out.txt")).is_err());
        assert!(fx.fs.read(&CanonPath::new("/ws/blocked.txt")).is_err());
        let recorded = permission_events(&fx);
        assert!(matches!(
            recorded.as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionAmended { .. },
                EventKind::PermissionDecided {
                    allowed: Some(false),
                    by: Some(Authority::Rules { .. }),
                    ..
                }
            ]
        ));
    }

    #[test]
    fn malformed_amended_params_are_tool_feedback_and_execute_nothing() {
        let fx = Fixture::new(writing("/ws/out.txt", "original"))
            .with_write_tool()
            .with_human([PermissionDecision::Amend {
                params: json!({"path": "/ws/out.txt"}),
            }]);

        let (reason, _, state) = run(&fx, SessionState::default(), "write it");

        assert_eq!(reason, ExitReason::Completed);
        assert!(fx.fs.read(&CanonPath::new("/ws/out.txt")).is_err());
        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the amended call still completes its tool item");
        assert_eq!(call.ok, Some(false));
        let result = tool_result_text(call);
        assert!(result.contains("contents"), "{}", result);
        assert!(matches!(
            permission_events(&fx).as_slice(),
            [
                EventKind::PermissionRequested { .. },
                EventKind::PermissionAmended { .. }
            ]
        ));
    }

    /// The permission event gets an item of its own, and the tool call keeps
    /// its own. One id per item is what lets a consumer say which lifecycle
    /// it is watching; the parent link is what still ties the two together.
    #[test]
    fn a_permission_event_is_its_own_item_and_names_the_call_as_its_cause() {
        let fx = Fixture::new(reading("/ws/notes.md")).with_read_tool();
        fx.fs.file("/ws/notes.md", "hello");

        run(&fx, SessionState::default(), "read the notes");

        let log = fx.store.events();
        let started = log
            .iter()
            .find(|ev| matches!(ev.kind, EventKind::ToolCallStarted { .. }))
            .expect("the call was logged");
        let decided = log
            .iter()
            .find(|ev| matches!(ev.kind, EventKind::PermissionDecided { .. }))
            .expect("the decision was logged");

        assert_ne!(
            decided.env.item, started.env.item,
            "a permission is not the tool item wearing a second kind"
        );
        assert_eq!(
            decided.env.parent,
            Some(started.env.id),
            "the decision is caused by the call it authorised"
        );
    }

    #[test]
    fn a_generation_that_calls_nothing_ends_the_turn_even_if_it_says_tool_use() {
        // A provider that keeps reporting `ToolUse` with no calls would spin
        // forever if the loop believed the stop reason over the evidence.
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::ToolUse)]]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "hello");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        assert_eq!(fx.generator.calls().len(), 1);
    }

    #[test]
    fn a_cancelled_turn_still_settles_its_attempt() {
        let fx = Fixture::new(vec![]);
        let token = CancelToken::new();
        let generator = Arc::new(SignallingGenerator {
            events: Mutex::new(vec![
                AiEvent::TextDelta {
                    text: "I have started".to_string(),
                },
                AiEvent::TextDelta {
                    text: " and will not finish".to_string(),
                },
                AiEvent::Stop(StopReason::Stop),
            ]),
            after: 1,
            signal: cancels(&token),
        });

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                token.clone(),
            ),
            model(&generator, &mut gate),
            tools,
        );
        assert!(!token.is_cancelled(), "nothing has asked to stop yet");

        let outcome = block_on(turn.run("start something long")).expect("the store accepted");

        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        assert!(!outcome.ok);
        let kinds = fx.store.kinds();
        assert!(
            kinds.contains(&"attempt_settled".to_string()),
            "a cancelled turn must still settle: {kinds:?}"
        );
        let settled = fx
            .store
            .events()
            .into_iter()
            .find_map(|ev| match ev.kind {
                EventKind::AttemptSettled { outcome, .. } => Some(outcome),
                _ => None,
            })
            .expect("the settlement was logged");
        assert_eq!(
            settled.stop(),
            StopReason::Aborted,
            "a stream nobody finished draining aborted, whatever it was about to say"
        );
        drop(turn);
        assert_eq!(
            outstanding(&gate),
            Some(usd("0")),
            "the reservation was released, not leaked"
        );
    }

    #[test]
    fn the_first_reason_a_turn_was_asked_to_stop_is_the_one_it_reports() {
        // A `SIGINT` and a deadline can both land, in either order, on a run
        // that is already shutting down — the second is often *caused* by the
        // first, since a cancelled turn still has to unwind. Reporting
        // whichever stored last would exit 124 for a run the user stopped,
        // and a scheduler that retries on timeout would then retry it.
        let token = CancelToken::new();
        assert_eq!(token.cause(), None, "nothing has asked to stop yet");

        token.cancel(CancelCause::User);
        token.cancel(CancelCause::Timeout);
        token.cancel(CancelCause::Host);

        assert!(token.is_cancelled());
        assert_eq!(
            token.cause(),
            Some(CancelCause::User),
            "the user asked first, so the run stopped because the user asked"
        );

        // And the same property from the other end: a deadline that arrives
        // first is not overwritten by the signal the shutdown then raises.
        let token = CancelToken::new();
        token.cancel(CancelCause::Timeout);
        token.cancel(CancelCause::User);
        assert_eq!(token.cause(), Some(CancelCause::Timeout));
    }

    #[test]
    fn a_cancelled_turn_does_not_run_the_tools_it_was_about_to() {
        // The hard case, and the reason cancellation is checked on the
        // generation's result and not only at the top of the loop: here the
        // cancel lands on the stream's *last* event, so the attempt settles
        // cleanly as `ToolUse` with a call already logged. A loop that only
        // looked at the stop reason would run that call — a side effect the
        // user asked not to happen — and only notice the cancellation on the
        // next iteration, once it was too late to matter.
        let fx = Fixture::new(vec![]).with_read_tool();
        fx.fs.file("/ws/note.txt", "hello from the file");
        let token = CancelToken::new();
        let generator = Arc::new(SignallingGenerator {
            events: Mutex::new(vec![
                AiEvent::ToolCall {
                    call_id: "call_abc".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/note.txt"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ]),
            after: 2,
            signal: cancels(&token),
        });

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                token,
            ),
            model(&generator, &mut gate),
            tools,
        );

        let outcome = block_on(turn.run("read note.txt")).expect("the store accepted");

        assert_eq!(
            outcome.exit_reason,
            ExitReason::Cancelled(CancelCause::User)
        );
        let kinds = fx.store.kinds();
        assert!(
            kinds.contains(&"tool_call_completed".to_string()),
            "a cancelled turn must close the queued call explicitly: {kinds:?}"
        );
        assert!(
            kinds.contains(&"attempt_settled".to_string()),
            "and it must still settle its attempt: {kinds:?}"
        );
        let state = turn.into_state().expect("unique turn journal state");
        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the call the model asked for was still logged");
        assert_eq!(
            call.ok,
            Some(false),
            "the call was not run and was explicitly answered as interrupted"
        );
        assert!(
            tool_result_text(call).contains("interrupted"),
            "the failed sibling result must say it was interrupted before execution"
        );
        assert_eq!(
            fx.store
                .events()
                .iter()
                .filter(|event| matches!(event.kind, EventKind::ToolCallCompleted { .. }))
                .count(),
            1,
            "the queued call receives exactly one explicit completion"
        );
    }

    #[test]
    fn a_paused_turn_does_not_run_the_tools_it_was_about_to() {
        // The twin of `a_cancelled_turn_does_not_run_the_tools_it_was_about
        // _to`, and the reason the batch is gated at its head rather than
        // only between generations: the pause lands on the stream's last
        // event, so the attempt settles cleanly as `ToolUse` with the call
        // already logged. A loop that asked about the hold only at the top
        // would run every call in this batch first — exactly the effects the
        // host asked not to happen — and notice on the next iteration, once
        // there was nothing left to hold back.
        let fx = Fixture::new(vec![]).with_read_tool();
        fx.fs.file("/ws/note.txt", "hello from the file");
        let pause = PauseToken::new();
        let request = request_id(7);
        let generator = Arc::new(SignallingGenerator {
            events: Mutex::new(vec![
                AiEvent::ToolCall {
                    call_id: "call_abc".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/note.txt"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ]),
            after: 2,
            signal: pauses(&pause, request),
        });

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&generator, &mut gate),
            tools,
        )
        .with_pause_token(pause.clone());

        let outcome = block_on(turn.run("read note.txt")).expect("the store accepted");

        assert_eq!(outcome.exit_reason, ExitReason::Paused);
        assert!(!outcome.ok);
        let state = turn.into_state().expect("unique turn journal state");
        let call = state
            .items
            .values()
            .find(|item| item.kind == ItemKind::ToolCall)
            .expect("the call the model asked for was still logged");
        assert_eq!(
            call.ok,
            Some(false),
            "the held call was not run and was explicitly answered"
        );
        assert!(
            tool_result_text(call).contains("interrupted"),
            "the unstarted call must say it never ran"
        );
        assert!(
            !tool_result_text(call).contains("hello from the file"),
            "and no byte of the file reached the conversation"
        );
    }

    #[test]
    fn a_paused_turn_records_the_boundary_and_the_request_that_asked_for_it() {
        // The durable half. The token stops a turn that is running; only the
        // log can tell a host that reconnects that the hold is in place, and
        // only the request id can tell it the hold is *its own* — which is
        // what makes a repeated `_ego/pause` idempotent rather than a second
        // pause.
        let fx = Fixture::new(vec![]);
        let pause = PauseToken::new();
        let request = request_id(7);
        let generator = Arc::new(SignallingGenerator {
            events: Mutex::new(vec![
                AiEvent::TextDelta {
                    text: "answered before the hold".to_string(),
                },
                AiEvent::Stop(StopReason::Stop),
            ]),
            after: 1,
            signal: pauses(&pause, request),
        });

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&generator, &mut gate),
            tools,
        )
        .with_pause_token(pause);

        let outcome = block_on(turn.run("say something")).expect("the store accepted");

        assert_eq!(outcome.exit_reason, ExitReason::Paused);
        let kinds = fx.store.kinds();
        let boundary = kinds
            .iter()
            .position(|kind| kind == "session_paused")
            .expect("the turn recorded the boundary it stopped at");
        let asked = kinds
            .iter()
            .position(|kind| kind == "session_pause_requested")
            .expect("and the request that asked for it");
        let completed = kinds
            .iter()
            .position(|kind| kind == "turn_completed")
            .expect("the turn still terminates");
        assert!(
            asked < boundary,
            "the request is recorded before the boundary answering it: {kinds:?}"
        );
        assert!(
            completed < asked,
            "and the hold is a session fact recorded after the turn's own \
             terminal, not instead of it: {kinds:?}"
        );
        let named: Vec<_> = fx
            .store
            .events()
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::SessionPauseRequested { request }
                | EventKind::SessionPaused { request } => Some(request),
                _ => None,
            })
            .collect();
        assert_eq!(named, vec![request, request], "both name the same request");
        let state = turn.into_state().expect("unique turn journal state");
        assert_eq!(state.pause, PauseState::Paused { request });
        assert!(!state.pause.admits_effects());
    }

    #[test]
    fn a_session_paused_before_this_run_does_not_generate_at_all() {
        // The hold outlives the turn that established it and the process that
        // ran it. A run asked for from any surface — including one with no
        // pause channel of its own, which is every CLI run — reads the same
        // durable state and refuses before spending a provider request.
        let request = request_id(7);
        let mut seq = Seq::ZERO;
        let mut state = SessionState::default();
        let turn_id = TurnId::new();
        for kind in [
            EventKind::SessionPauseRequested { request },
            EventKind::SessionPaused { request },
        ] {
            state.apply(&DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn: turn_id,
                    item: None,
                    parent: None,
                    ts: Timestamp::from_millis(1_787_000_000_000),
                    producer: Producer::Main,
                },
                kind,
            });
        }
        assert_eq!(state.pause, PauseState::Paused { request });

        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        let (reason, ok, _) = run(&fx, state, "carry on anyway");

        assert_eq!(reason, ExitReason::Paused);
        assert!(!ok);
        assert!(
            fx.generator.calls().is_empty(),
            "a paused session spends nothing: {:?}",
            fx.generator.calls()
        );
    }

    #[test]
    fn a_refused_budget_stops_the_turn_without_starting_a_generation() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        // A gate too small to admit the upper bound of any request.
        let mut gate = SharedBudgetGate::new(BudgetGate::new(usd("0.0000001")));
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        );

        let outcome = block_on(turn.run("something expensive")).expect("the store accepted");

        assert_eq!(outcome.exit_reason, ExitReason::Budget);
        assert!(!outcome.ok);
        let kinds = fx.store.kinds();
        assert!(
            !kinds.contains(&"generation_started".to_string()),
            "nothing may reach the provider once the gate refuses: {kinds:?}"
        );
        assert_eq!(kinds, vec!["user_prompt", "error", "turn_completed"]);
        assert!(fx.generator.calls().is_empty());
    }

    #[test]
    fn a_provider_error_is_logged_and_the_attempt_still_settles() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Error {
                message: "the provider fell over".to_string(),
                kind: ErrorKind::Unclassified,
            },
            AiEvent::Stop(StopReason::Error),
        ]]);

        let (reason, ok, state) = run(&fx, SessionState::default(), "hello");

        assert_eq!(reason, ExitReason::Provider);
        assert!(!ok);
        assert_eq!(state.errors, vec!["the provider fell over".to_string()]);
        assert!(fx.store.kinds().contains(&"attempt_settled".to_string()));
    }

    /// Two calls in one generation claiming the same provider id.
    ///
    /// See the tool-use-history ADR §6. Nothing rejected this before: the
    /// openai decoder's `settle_identity` compares an incoming id only with
    /// the id held at the *same* index, and the loop mints a fresh `ItemId`
    /// per call while `SessionState` keys on `ItemId`, so two items carried
    /// one `call_id` without complaint. It surfaced two generations later as
    /// a **panic** — `Conversation::new` returning `DuplicateCallId` into an
    /// `expect` whose stated reason was about unanswered calls — by which
    /// point both tools had already run.
    ///
    /// So the assertion that matters is not the exit code, it is
    /// **`executions == 0`**. A check that merely stopped the panic would
    /// leave the double effect, which is survivable only while every tool is
    /// read-only.
    /// The loop's half of `POTENTIALS.md` P22, and the only place it can be
    /// proved: a hand-built log can assert what a *rebuild* does with a
    /// parent edge, but only a run can assert that the loop writes one. The
    /// history is rebuilt from the state the run produced, exactly as the
    /// next generation would rebuild it.
    ///
    /// Without the edge this is five turns: use, answer, use, answer — a
    /// conversation in which the model saw `/ws/a`'s contents before asking
    /// for `/ws/b`, which is not what it did.
    #[test]
    fn two_calls_from_one_generation_rebuild_as_the_turn_the_model_took() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/a"}"#.to_string(),
                },
                AiEvent::ToolCall {
                    call_id: "call_2".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/b"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();

        let (reason, ok, state) = run(&fx, SessionState::default(), "read both");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
        let (conversation, repairs) =
            conversation_from(&state).expect("a history the loop just wrote rebuilds");
        assert!(repairs.is_empty(), "{repairs:?}");
        let shape: Vec<(Role, usize)> = conversation
            .messages()
            .iter()
            .map(|message| (message.role, message.content.len()))
            .collect();
        assert_eq!(
            shape,
            [(Role::User, 1), (Role::Assistant, 2), (Role::User, 2)],
            "one assistant turn made both calls, and one user turn answers both"
        );
    }

    #[test]
    fn duplicate_provider_call_ids_end_the_turn_before_any_tool_runs() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/a"}"#.to_string(),
                },
                AiEvent::ToolCall {
                    // The same id, a different call. Neither may be dispatched:
                    // once two calls share an id, a result cannot be attributed
                    // back to the call that earned it.
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/b"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            // Present and must go unused: reaching it would mean the loop
            // carried on past corruption.
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();

        let (reason, ok, state) = run(&fx, SessionState::default(), "read both");

        assert_eq!(
            reason,
            ExitReason::Provider,
            "a malformed response is the provider's fault, not ours: CLI.md §4.3 exit 4"
        );
        assert!(!ok);
        assert_eq!(
            fx.generator.calls().len(),
            1,
            "the turn must not generate again on a corrupt response"
        );
        let executions = state
            .items
            .values()
            .filter(|item| item.kind == ItemKind::ToolCall && item.ok.is_some())
            .count();
        assert_eq!(
            executions, 0,
            "neither call may run: the panic this replaces fired only after both had"
        );
        assert!(
            state.errors.iter().any(|error| error.contains("call_1")),
            "the duplicated id has to be named, or nobody can diagnose it: {:?}",
            state.errors
        );
        assert!(
            fx.store.kinds().contains(&"attempt_settled".to_string()),
            "the attempt is already paid for and still has to settle: {:?}",
            fx.store.kinds()
        );
    }

    /// The case that decides *where* the guard lives. A decoder sees one
    /// response and can never catch this; only a check seeded from the
    /// reduction can. Emptying that seed leaves the test above passing, which
    /// is how this one came to be written.
    #[test]
    fn a_call_id_reused_by_a_later_generation_is_refused_too() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/a"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::ToolCall {
                    // Answered in the generation before this one, so a result
                    // naming it now would answer that call as readily as this
                    // one.
                    call_id: "call_1".to_string(),
                    name: "read".to_string(),
                    args: r#"{"path":"/ws/b"}"#.to_string(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![AiEvent::Stop(StopReason::Stop)],
        ])
        .with_read_tool();

        let (reason, ok, state) = run(&fx, SessionState::default(), "read a, then b");

        assert_eq!(reason, ExitReason::Provider);
        assert!(!ok);
        assert_eq!(
            fx.generator.calls().len(),
            2,
            "the third generation is never reached"
        );
        let executions = state
            .items
            .values()
            .filter(|item| item.kind == ItemKind::ToolCall && item.ok.is_some())
            .count();
        assert_eq!(
            executions, 1,
            "the first generation's call had a valid id and stands; only the reuse is refused"
        );
        assert!(
            state.errors.iter().any(|error| error.contains("call_1")),
            "{:?}",
            state.errors
        );
    }

    /// The one classification that changes where the run lands. `CLI.md:215`
    /// tells a script to retry on exit 4; a request that will be wrong again
    /// is precisely what it must not retry, so an `Invalid` is exit 2.
    #[test]
    fn a_provider_error_the_request_caused_exits_as_usage_and_not_as_provider() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Error {
                message: "context length exceeded".to_string(),
                kind: ErrorKind::Invalid,
            },
            AiEvent::Stop(StopReason::Error),
        ]]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "hello");

        assert_eq!(reason, ExitReason::Usage);
        assert!(!ok);
    }

    /// The other three classifications land on exit 4, and this pins that as
    /// a decision rather than an accident — including `Unclassified`, which
    /// gets there by failing closed rather than by being read from anything.
    #[test]
    fn every_other_classification_still_exits_as_provider() {
        for kind in [
            ErrorKind::Transient,
            ErrorKind::Refused,
            ErrorKind::Unclassified,
        ] {
            let fx = Fixture::new(vec![vec![
                AiEvent::Error {
                    message: "upstream said no".to_string(),
                    kind,
                },
                AiEvent::Stop(StopReason::Error),
            ]]);

            let (reason, _, _) = run(&fx, SessionState::default(), "hello");
            assert_eq!(reason, ExitReason::Provider, "for {kind:?}");
        }
    }

    /// The last classification decides, because the terminal frame follows
    /// the error that caused it. An earlier error the stream recovered from
    /// does not describe how it ended.
    #[test]
    fn the_last_error_classification_is_the_one_that_decides_the_exit() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Error {
                message: "a blip".to_string(),
                kind: ErrorKind::Transient,
            },
            AiEvent::Error {
                message: "context length exceeded".to_string(),
                kind: ErrorKind::Invalid,
            },
            AiEvent::Stop(StopReason::Error),
        ]]);

        let (reason, _, state) = run(&fx, SessionState::default(), "hello");

        assert_eq!(reason, ExitReason::Usage);
        assert_eq!(
            state.errors.len(),
            2,
            "both are still journalled; only the exit is decided by the last"
        );
    }

    /// A classification is never consulted for a generation that ended some
    /// other way — otherwise a recovered-from error would rewrite the exit of
    /// a turn that finished cleanly.
    #[test]
    fn an_error_the_stream_recovered_from_does_not_change_a_clean_exit() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Error {
                message: "a blip".to_string(),
                kind: ErrorKind::Invalid,
            },
            AiEvent::TextDelta {
                text: "recovered".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);

        let (reason, ok, _) = run(&fx, SessionState::default(), "hello");

        assert_eq!(reason, ExitReason::Completed);
        assert!(ok);
    }

    // --- reasoning state closes its block ----------------------------------

    /// The state is the block's close, so the next delta opens a new item.
    /// Anthropic documents a `400` when blocks are not replayed exactly, and
    /// two blocks merged into one is not an exact replay.
    #[test]
    fn reasoning_state_closes_its_block_so_the_next_delta_opens_another() {
        let fx = Fixture::new(vec![vec![
            AiEvent::ReasoningDelta {
                text: "first".to_string(),
            },
            AiEvent::ReasoningState(ReasoningState {
                blob: "sealed-one".to_string(),
                dialect: "anthropic-messages".to_string(),
            }),
            AiEvent::ReasoningDelta {
                text: "second".to_string(),
            },
            AiEvent::ReasoningState(ReasoningState {
                blob: "sealed-two".to_string(),
                dialect: "anthropic-messages".to_string(),
            }),
            AiEvent::Stop(StopReason::Stop),
        ]]);

        run(&fx, SessionState::default(), "hello");

        let items: Vec<_> = fx
            .store
            .events()
            .iter()
            .filter(|ev| {
                matches!(
                    ev.kind,
                    EventKind::ReasoningDelta { .. } | EventKind::ReasoningState { .. }
                )
            })
            .map(|ev| ev.env.item.expect("reasoning events carry an item"))
            .collect();

        assert_eq!(items.len(), 4);
        assert_eq!(items[0], items[1], "the state seals the block it closes");
        assert_eq!(items[2], items[3]);
        assert_ne!(
            items[0], items[2],
            "the second block must be a new item, not a continuation of the sealed one"
        );
    }

    /// A block whose `thinking` field is empty is the case Anthropic's `400`
    /// names explicitly, so state arriving with no delta before it still
    /// needs an item to hang on.
    #[test]
    fn reasoning_state_with_no_preceding_delta_still_gets_an_item() {
        let fx = Fixture::new(vec![vec![
            AiEvent::ReasoningState(ReasoningState {
                blob: "sealed-and-silent".to_string(),
                dialect: "anthropic-messages".to_string(),
            }),
            AiEvent::Stop(StopReason::Stop),
        ]]);

        run(&fx, SessionState::default(), "hello");

        let events = fx.store.events();
        let logged = events
            .iter()
            .find(|ev| matches!(ev.kind, EventKind::ReasoningState { .. }))
            .expect("the state is journalled even with no text before it");
        assert!(logged.env.item.is_some());
    }

    #[test]
    fn a_turn_whose_log_cannot_be_written_is_abandoned_with_the_state_intact() {
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "hi".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);
        // The third append is `assistant_delta`, in the middle of a live
        // generation — the case where an early return would drop an
        // unsettled attempt.
        fx.store.fail_at(3);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        );

        let abandoned = block_on(turn.run("hello")).expect_err("the store refused an append");

        assert_eq!(
            Some(abandoned.state.last_seq),
            counting_from_one(fx.store.events().len()).last().copied(),
            "the in-memory state must describe exactly what reached the log"
        );
        drop(turn);
        assert_eq!(
            outstanding(&gate),
            Some(usd("0")),
            "an abandoned turn still settles the attempt it had in flight"
        );
    }

    /// The decisive property of the preflight, and the reason it sits where it
    /// sits: a reservation taken for a request the model will reject has
    /// authorised spending on something that was never going to be sent. So
    /// the gate must be untouched — `outstanding` and `committed` both zero —
    /// and the generator must never have been called.
    #[test]
    fn a_request_the_model_will_not_take_never_reaches_the_budget_gate() {
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "never sent".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let refusing = model_with_reasoning(
            &fx.generator,
            &mut gate,
            ReasoningEffortSupport::Only(BTreeSet::from([
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ])),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            refusing,
            tools,
        );

        let outcome = block_on(turn.run("hi")).expect("the store accepted every append");

        assert_eq!(outcome.exit_reason, ExitReason::Usage);
        assert!(!outcome.ok);
        drop(turn);
        assert_eq!(
            outstanding(&gate),
            Some(usd("0")),
            "the gate reserved anyway"
        );
        assert_eq!(committed(&gate), Some(usd("0")), "the gate settled anyway");
        assert!(
            fx.generator.calls().is_empty(),
            "the request was sent to the provider anyway"
        );
    }

    /// A usage error is exit 2, and the log says which levels would have
    /// worked — the summary line in `RunTermination` deliberately does not.
    #[test]
    fn a_refused_level_is_named_in_the_log_with_the_levels_that_would_have_worked() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let refusing = model_with_reasoning(
            &fx.generator,
            &mut gate,
            ReasoningEffortSupport::Only(BTreeSet::from([ThinkingLevel::High])),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            refusing,
            tools,
        );

        let outcome = block_on(turn.run("hi")).expect("the store accepted every append");

        assert_eq!(
            RunTermination::after_turn(outcome.exit_reason, outcome.denied)
                .expect("a usage error ends the run")
                .exit_code(),
            2
        );
        drop(turn);
        let message = fx
            .store
            .events()
            .iter()
            .find_map(|ev| match &ev.kind {
                EventKind::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("the refusal is durable, or nobody can diagnose the run from the log");
        assert!(message.contains("Off"), "{message}");
        assert!(message.contains("High"), "{message}");
    }

    /// A model that states nothing is refused for a different reason, and the
    /// message says so: re-resolve the model, do not pick another level.
    #[test]
    fn a_model_that_stated_nothing_is_refused_before_it_is_sent_anything() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let silent =
            model_with_reasoning(&fx.generator, &mut gate, ReasoningEffortSupport::Unknown);
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            silent,
            tools,
        );

        let outcome = block_on(turn.run("hi")).expect("the store accepted every append");

        assert_eq!(outcome.exit_reason, ExitReason::Usage);
        drop(turn);
        assert!(fx.generator.calls().is_empty());
    }

    #[test]
    fn the_public_stream_is_what_the_caller_gets_back() {
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "hello".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        );

        block_on(turn.run("hi")).expect("the store accepted");

        let published = fx.sink.events();
        assert!(
            !published.is_empty(),
            "a turn that wrote a log published something"
        );
        // `GenerationStarted` and `AttemptSettled` have no public spelling:
        // the attempt lifecycle is internal accounting, not a host's business.
        // `TurnStarted` and `MeteringSelected` go the other way — both are
        // published and neither is appended, so the sink carries two more
        // events than the log does public ones.
        assert_eq!(
            published.len(),
            fx.store.events().len() - 2 + 2,
            "exactly the two attempt-lifecycle events stay internal, and \
             turn.started and metering.selected are published without being \
             logged"
        );
    }

    #[test]
    fn the_first_request_carries_the_prompt_and_nothing_else() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);

        run(&fx, SessionState::default(), "the only thing said so far");

        let first = &fx.generator.calls()[0];
        let messages = first.messages.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(
            messages[0].content,
            vec![ContentBlock::Text {
                text: "the only thing said so far".to_string()
            }]
        );
    }

    // --- the sink is live, and it is fed only what is durable ---

    #[test]
    fn an_event_reaches_the_sink_before_the_turn_that_produced_it_ends() {
        // `CLI.md` §4.2: "one event per line, **live**". The turn below never
        // ends, so anything the sink holds got there while it was running.
        let fx = Fixture::new(Vec::new());
        let generator = Arc::new(StallingGenerator {
            events: Mutex::new(vec![AiEvent::TextDelta {
                text: "thinking out loud".to_string(),
            }]),
        });
        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model(&generator, &mut gate),
            tools,
        );

        let mut running = Box::pin(turn.run("say something"));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(
            running.as_mut().poll(&mut cx).is_pending(),
            "the generator never finishes, so neither does the turn"
        );

        let seen = fx.sink.events();
        assert!(
            seen.iter().any(|event| matches!(
                event,
                PublicEvent::ItemDelta { text, .. } if text == "thinking out loud"
            )),
            "the delta was published while the turn was still running, got {seen:?}"
        );
    }

    #[test]
    fn a_turn_announces_itself_before_it_says_anything_else() {
        // `turn.started` has no `EventKind`, so before the sink existed
        // nothing in the workspace could emit it and §4.2's envelope was
        // missing a level.
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "hello".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);

        let (_, _, _) = run(&fx, SessionState::default(), "hi");

        let seen = fx.sink.events();
        match seen.first() {
            Some(PublicEvent::TurnStarted { turn }) => assert!(
                seen.iter().skip(1).any(|event| matches!(
                    event,
                    PublicEvent::TurnCompleted { turn: ended, .. } if ended == turn
                )),
                "the turn that announced itself is the one that finished"
            ),
            other => panic!("a turn announces itself first, got {other:?}"),
        }
    }

    #[test]
    fn an_event_the_store_refused_never_reaches_the_sink() {
        // The ordering is the guarantee: a host told about an event no reader
        // will find is being told about a turn nobody can resume into.
        let fx = Fixture::new(vec![vec![
            AiEvent::TextDelta {
                text: "never written".to_string(),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);
        fx.store.fail_at(1);

        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        );
        block_on(turn.run("hello")).expect_err("the store refused the first append");

        // `TurnStarted` is published and never appended — it has no log entry
        // to be refused — so it is the only thing a sink may hold here.
        assert!(
            fx.sink
                .events()
                .iter()
                .all(|event| matches!(event, PublicEvent::TurnStarted { .. })),
            "nothing the store refused was published: {:?}",
            fx.sink.events()
        );
    }

    // --- how this generation is metered, said out loud ---
    //
    // `docs/decisions/2026-08-21-how-a-model-is-metered.md` §6. Three of the
    // four arrangements make a monetary `--budget` inert, and an operator who
    // set one has to be told that before the run rather than inferring it
    // from a limit that never fired.

    fn metering_events(fx: &Fixture) -> Vec<PublicEvent> {
        fx.sink
            .events()
            .into_iter()
            .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
            .collect()
    }

    fn subscription() -> Metering {
        Metering::Subscription {
            revision: MeteringRevision("acme/pro-plan/2026-08".to_string()),
        }
    }

    fn provider_reported() -> Metering {
        Metering::Currency {
            settlement: SettlementSource::ProviderReported {
                revision: ProviderCostRevision {
                    provider: "xai".to_string(),
                    field: "usage.cost_in_usd_ticks".to_string(),
                    schema_revision: "2026-08-21".to_string(),
                },
            },
        }
    }

    #[test]
    fn every_generation_says_how_it_is_metered_before_the_gate_is_asked() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        run(&fx, SessionState::default(), "hi");

        assert_eq!(
            metering_events(&fx),
            vec![PublicEvent::MeteringSelected {
                turn: match fx.sink.events().first() {
                    Some(PublicEvent::TurnStarted { turn }) => *turn,
                    other => panic!("a turn starts before it is metered: {other:?}"),
                },
                metering: ego_core::public::MeteringKind::Currency,
                ceiling: ego_core::public::CeilingEffect::Binds {
                    limit: usd("1000"),
                    clamped_from: None,
                },
                rates: RateSource::Operator,
            }]
        );

        // Before the gate is asked, and the falsifiable form of that is a
        // gate that refuses: an operator whose `--budget` denies the very
        // first generation still has to be told which metering was about to
        // be charged, or the denial is unreadable.
        let denied = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        *denied.gate.borrow_mut() = SharedBudgetGate::new(BudgetGate::new(usd("0.01")));
        let (exit, ok, _) = run(&denied, SessionState::default(), "hi");
        assert_eq!(exit, ExitReason::Budget);
        assert!(!ok, "a cent cannot admit this generation");
        assert_eq!(
            metering_events(&denied).len(),
            1,
            "a refused generation was still metered: {:?}",
            denied.sink.events()
        );
        assert!(
            denied.generator.calls().is_empty(),
            "and the provider was never reached, so this is genuinely pre-gate"
        );
    }

    /// The token ceiling, at the boundary §9 puts it on: between two
    /// generations of the *same* turn, not between turns.
    ///
    /// The tool call is what makes the distinction observable. A turn that
    /// calls a tool generates twice; a ceiling checked only between turns
    /// would let the second generation run and stop the run afterwards,
    /// having spent the tokens it was there to stop. Here the first
    /// generation's usage crosses the ceiling and the second never happens.
    #[test]
    fn a_token_ceiling_stops_the_next_generation_of_the_turn_it_is_in() {
        let script = || {
            vec![
                vec![
                    AiEvent::ToolCall {
                        call_id: "call-1".to_string(),
                        name: "read".to_string(),
                        args: r#"{"path":"/ws/note.txt"}"#.to_string(),
                    },
                    AiEvent::Usage(tokens(900, 0, 0, 200)),
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![AiEvent::Stop(StopReason::Stop)],
            ]
        };

        // 1_100 billed tokens against a 1_000 ceiling: reached after the
        // first generation, so the second is refused.
        let capped = Fixture::new(script()).with_read_tool();
        capped.fs.file("/ws/note.txt", "hello from the file");
        let (exit, ok, _) = run_with(
            &capped,
            SessionState::default(),
            "what is in note.txt?",
            metering(),
            Some(rates()),
            Some(1_000),
            config(),
        );

        assert_eq!(exit, ExitReason::Budget);
        assert!(!ok);
        assert_eq!(
            capped.generator.calls().len(),
            1,
            "the second generation ran despite the ceiling"
        );

        // The same script with no ceiling generates twice, so the assertion
        // above is about the ceiling and not about the script.
        let uncapped = Fixture::new(script()).with_read_tool();
        uncapped.fs.file("/ws/note.txt", "hello from the file");
        let (exit, ok, _) = run(&uncapped, SessionState::default(), "what is in note.txt?");
        assert_eq!(exit, ExitReason::Completed);
        assert!(ok);
        assert_eq!(uncapped.generator.calls().len(), 2);
    }

    /// It binds a subscription row exactly as it binds a metered one. That is
    /// §9's second property: the token ceiling needs no rates, no fetch and
    /// no network, so it is the one instrument that works on the two
    /// arrangements a monetary ceiling cannot touch.
    #[test]
    fn a_token_ceiling_binds_a_row_no_money_ceiling_could() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Usage(tokens(900, 0, 0, 200)),
            AiEvent::Stop(StopReason::Stop),
        ]]);
        *fx.gate.borrow_mut() = SharedBudgetGate::new(BudgetGate::unlimited(
            CurrencyCode::new("USD").expect("USD is valid"),
        ));

        // The first turn spends 1_100 tokens against a 1_000 ceiling, and
        // completes: the ceiling is checked before a generation, not after.
        let (exit, ok, state) = run_with(
            &fx,
            SessionState::default(),
            "hi",
            subscription(),
            None,
            Some(1_000),
            config(),
        );
        assert_eq!(exit, ExitReason::Completed);
        assert!(ok);

        // The next one is refused before it reaches the provider, on a row
        // that reserves zero and could never be stopped by `--budget 5$`.
        let (exit, ok, _) = run_with(
            &fx,
            state,
            "and again",
            subscription(),
            None,
            Some(1_000),
            config(),
        );
        assert_eq!(exit, ExitReason::Budget);
        assert!(!ok);
        assert_eq!(
            fx.generator.calls().len(),
            1,
            "a flat-fee row was generated past its token ceiling"
        );
        assert_eq!(
            RunTermination::after_turn(exit, None),
            Some(RunTermination::BudgetExceeded),
            "and it is exit 5, the same code the money ceiling ends on"
        );
    }

    /// The ordinary case, end to end: nobody configured a ceiling, so the
    /// run says so and runs anyway.
    ///
    /// This is §1's whole claim reduced to something falsifiable. The turn
    /// has no rates at all — the state that used to be a refusal — and it
    /// completes, publishing `ceiling: none` and `rates: none` rather than a
    /// limit that is not there or a provenance for figures nobody has.
    #[test]
    fn a_run_with_no_ceiling_and_no_rates_completes_and_says_so() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Usage(tokens(1_000, 0, 0, 200)),
            AiEvent::Stop(StopReason::Stop),
        ]]);
        *fx.gate.borrow_mut() = SharedBudgetGate::new(BudgetGate::unlimited(
            CurrencyCode::new("USD").expect("USD is valid"),
        ));

        let (exit, ok, _) = run_with(
            &fx,
            SessionState::default(),
            "hi",
            metering(),
            None,
            None,
            config(),
        );

        assert_eq!(exit, ExitReason::Completed);
        assert!(ok, "a run nobody asked to bound is not refused for a price");
        assert_eq!(
            metering_events(&fx),
            vec![PublicEvent::MeteringSelected {
                turn: match fx.sink.events().first() {
                    Some(PublicEvent::TurnStarted { turn }) => *turn,
                    other => panic!("a turn starts before it is metered: {other:?}"),
                },
                metering: ego_core::public::MeteringKind::Currency,
                ceiling: ego_core::public::CeilingEffect::None,
                rates: RateSource::None,
            }],
            "no ceiling and no rates are each announced as the absences they are"
        );
        assert_eq!(
            fx.generator.calls().len(),
            1,
            "and the provider was actually reached"
        );
    }

    /// The three arrangements a monetary budget cannot bind, each said in its
    /// own words. Without this the variants would be constructible and
    /// invisible — an operator running a subscription model under `--budget
    /// $5` would see a limit that silently never applies. §10 says that
    /// outright: the ceiling on a flat-fee row is announced `inapplicable`,
    /// naming the limit that will never fire and the metering that defeats
    /// it.
    #[test]
    fn each_metering_is_announced_as_the_thing_it_actually_is() {
        use ego_core::public::{CeilingEffect, MeteringKind};

        for (metering, expected_kind, expected_ceiling) in [
            (
                subscription(),
                MeteringKind::Subscription,
                CeilingEffect::Inapplicable {
                    limit: usd("1000"),
                    metering: MeteringKind::Subscription,
                },
            ),
            (
                Metering::Unmetered {
                    revision: MeteringRevision("ego/builtin/local/v1".to_string()),
                },
                MeteringKind::Unmetered,
                CeilingEffect::Inapplicable {
                    limit: usd("1000"),
                    metering: MeteringKind::Unmetered,
                },
            ),
            (
                provider_reported(),
                MeteringKind::Currency,
                CeilingEffect::Binds {
                    limit: usd("1000"),
                    clamped_from: None,
                },
            ),
        ] {
            let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
            run_metered(&fx, SessionState::default(), "hi", metering.clone());

            let announced = metering_events(&fx);
            let [
                PublicEvent::MeteringSelected {
                    metering: kind,
                    ceiling,
                    rates,
                    ..
                },
            ] = announced.as_slice()
            else {
                panic!("exactly one metering announcement per generation: {announced:?}");
            };
            assert_eq!(*kind, expected_kind, "for {metering:?}");
            assert_eq!(*ceiling, expected_ceiling, "for {metering:?}");
            assert_eq!(
                *rates,
                RateSource::Operator,
                "every fixture run resolves its rates from the operator's file, \
                 whatever the arrangement: for {metering:?}"
            );
        }
    }

    /// A subscription reserves nothing, so a monetary limit smaller than any
    /// currency bound still admits the generation. This is the gate actually
    /// behaving differently, not just a label on a log line.
    #[test]
    fn a_subscription_generation_is_admitted_by_a_budget_no_currency_row_could_pass() {
        let script = || {
            vec![vec![
                AiEvent::Usage(tokens(1_000, 0, 0, 200)),
                AiEvent::Stop(StopReason::Stop),
            ]]
        };

        // A cent could not cover the $0.76536 worst case of the currency row.
        let currency = Fixture::new(script());
        *currency.gate.borrow_mut() = SharedBudgetGate::new(BudgetGate::new(usd("0.01")));
        let (exit, ok, _) = run(&currency, SessionState::default(), "hi");
        assert_eq!(exit, ExitReason::Budget);
        assert!(!ok, "a cent cannot admit a currency-metered generation");

        let flat = Fixture::new(script());
        *flat.gate.borrow_mut() = SharedBudgetGate::new(BudgetGate::new(usd("0.01")));
        let (exit, ok, _) = run_metered(&flat, SessionState::default(), "hi", subscription());
        assert_eq!(exit, ExitReason::Completed);
        assert!(ok, "a flat fee is not spent per request");
        assert!(
            committed(&flat.gate.borrow())
                .expect("a limited gate keeps totals")
                .amount
                .is_zero()
        );
    }

    /// And the zero it settles at is `Known`, attributed to the policy.
    /// `Unknown` would poison the session's cumulative total for a figure
    /// nobody doubts, and a `PricingRevision` here would be a price list that
    /// does not exist.
    #[test]
    fn a_subscription_settles_at_a_known_zero_attributed_to_its_policy() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Usage(tokens(1_000, 0, 0, 200)),
            AiEvent::Stop(StopReason::Stop),
        ]]);
        run_metered(&fx, SessionState::default(), "hi", subscription());

        let booked: Vec<_> = fx
            .store
            .events()
            .into_iter()
            .filter_map(|ev| match ev.kind {
                EventKind::UsageUpdated { cost_delta, .. } => Some(cost_delta),
                _ => None,
            })
            .collect();
        assert_eq!(
            booked,
            vec![CostDelta::Known {
                amount: usd("0"),
                attribution: CostAttribution::MeteringPolicy {
                    revision: MeteringRevision("acme/pro-plan/2026-08".to_string()),
                },
                fx: None,
            }]
        );
    }

    /// Under provider-reported settlement the log books the provider's own
    /// figure, and the usage frame no longer decides the money.
    #[test]
    fn a_stated_cost_settles_the_generation_and_the_table_does_not() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Usage(tokens(1_000, 0, 0, 200)),
            AiEvent::ProviderCost {
                cost: usd("0.0012345678"),
            },
            AiEvent::Stop(StopReason::Stop),
        ]]);
        run_metered(&fx, SessionState::default(), "hi", provider_reported());

        let booked: Vec<_> = fx
            .store
            .events()
            .into_iter()
            .filter_map(|ev| match ev.kind {
                EventKind::UsageUpdated {
                    usage, cost_delta, ..
                } => Some((usage, cost_delta)),
                _ => None,
            })
            .collect();
        let [(usage, cost_delta)] = booked.as_slice() else {
            panic!("one usage update, holding both halves: {booked:?}");
        };
        assert_eq!(
            *usage,
            tokens(1_000, 0, 0, 200),
            "the tokens are still recorded; it is the money that changed hands"
        );
        assert_eq!(
            *cost_delta,
            CostDelta::Known {
                amount: usd("0.0012345678"),
                attribution: CostAttribution::ProviderReported(ProviderCostRevision {
                    provider: "xai".to_string(),
                    field: "usage.cost_in_usd_ticks".to_string(),
                    schema_revision: "2026-08-21".to_string(),
                }),
                fx: None,
            }
        );
        // 1000 x 0.000003 + 200 x 0.000015 = 0.006, which is what the table
        // would have said. It is three times the stated figure, so a silent
        // fallback to the quote could not hide here.
        assert_ne!(
            *cost_delta,
            quote().price(&tokens(1_000, 0, 0, 200)),
            "a stated cost is not the table's answer"
        );
        assert_eq!(committed(&fx.gate.borrow()), Some(usd("0.0012345678")));
    }

    /// The cost may arrive before the usage or after it — SSE gives no
    /// guarantee — and one `UsageUpdated` has to come out either way. Booking
    /// per event would produce two, double-counting the tokens.
    #[test]
    fn a_stated_cost_and_a_usage_frame_are_booked_once_in_either_order() {
        let cost = AiEvent::ProviderCost {
            cost: usd("0.0012345678"),
        };
        let usage = AiEvent::Usage(tokens(1_000, 0, 0, 200));
        let mut booked = Vec::new();
        for script in [
            vec![usage.clone(), cost.clone(), AiEvent::Stop(StopReason::Stop)],
            vec![cost.clone(), usage.clone(), AiEvent::Stop(StopReason::Stop)],
        ] {
            let fx = Fixture::new(vec![script]);
            run_metered(&fx, SessionState::default(), "hi", provider_reported());
            booked.push(
                fx.store
                    .events()
                    .into_iter()
                    .filter_map(|ev| match ev.kind {
                        EventKind::UsageUpdated {
                            usage, cost_delta, ..
                        } => Some((usage, cost_delta)),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(booked[0].len(), 1, "one booking, not one per event");
        assert_eq!(booked[0], booked[1], "the wire order is not the record");
    }

    /// A provider that declared it states a cost and then did not: the money
    /// is unknown, named as *that* silence rather than the usage one, and the
    /// gate charges the full reserved bound.
    #[test]
    fn a_missing_stated_cost_settles_at_the_bound_and_not_at_zero() {
        let fx = Fixture::new(vec![vec![
            AiEvent::Usage(tokens(1_000, 0, 0, 200)),
            AiEvent::Stop(StopReason::Stop),
        ]]);
        run_metered(&fx, SessionState::default(), "hi", provider_reported());

        let booked: Vec<_> = fx
            .store
            .events()
            .into_iter()
            .filter_map(|ev| match ev.kind {
                EventKind::UsageUpdated { cost_delta, .. } => Some(cost_delta),
                _ => None,
            })
            .collect();
        assert_eq!(
            booked,
            vec![CostDelta::Unknown(UnknownCost::ProviderReportedCostMissing)],
            "the tokens arrived, so `ProviderReportedNoUsage` would name the wrong silence"
        );
        assert_eq!(
            committed(&fx.gate.borrow()),
            Some(usd("0.76536")),
            "an unknown cost charges the bound, never zero"
        );
    }

    mod generation_boundary {
        use super::*;
        use ego_core::generation_model_source::{
            GenerationModelError, GenerationModelErrorClass, GenerationModelRequest,
            GenerationModelSource, GenerationProfile, GenerationRole,
        };
        use ego_core::generator::attempt::{AttemptRef, DeliveryCertainty};
        use ego_core::generator::capabilities::CapabilityGap;

        fn native_workspace() -> CanonPath {
            if cfg!(windows) {
                CanonPath::new(r"C:\ws")
            } else {
                CanonPath::new("/ws")
            }
        }

        enum ResolutionStep {
            Ready(Box<Result<GenerationProfile, GenerationModelError>>),
            Pending,
        }

        pub(super) struct ScriptedGenerationSource {
            steps: Mutex<VecDeque<ResolutionStep>>,
            resolved: Mutex<Vec<GenerationRole>>,
            ready: Option<Mutex<Option<futures::channel::oneshot::Sender<()>>>>,
            trace: Option<GenerationTrace>,
        }

        #[derive(Clone, Default)]
        pub(super) struct GenerationTrace {
            pub(super) requests: Arc<Mutex<Vec<GenRequest>>>,
            steps: Arc<Mutex<Vec<String>>>,
            pub(super) timeline: Arc<Mutex<Vec<String>>>,
        }

        struct RecordingGenerator {
            inner: FakeGenerator,
            trace: GenerationTrace,
            expressibility: Result<(), String>,
        }

        impl Generator for RecordingGenerator {
            fn ensure_expressible(
                &self,
                _request: &GenRequest,
            ) -> Result<(), RequestExpressibilityError> {
                match &self.expressibility {
                    Ok(()) => Ok(()),
                    Err(message) => Err(RequestExpressibilityError::new(message.clone())),
                }
            }

            fn generate(&self, request: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
                self.trace.requests.lock().unwrap().push(request.clone());
                self.trace
                    .steps
                    .lock()
                    .unwrap()
                    .push(format!("generate:{}", request.model));
                self.trace.timeline.lock().unwrap().push("provider".into());
                self.inner.generate(request, attempt)
            }
        }

        impl ScriptedGenerationSource {
            pub(super) fn new(
                profiles: impl IntoIterator<Item = Result<GenerationProfile, GenerationModelError>>,
            ) -> Self {
                Self {
                    steps: Mutex::new(
                        profiles
                            .into_iter()
                            .map(|profile| ResolutionStep::Ready(Box::new(profile)))
                            .collect(),
                    ),
                    resolved: Mutex::new(Vec::new()),
                    ready: None,
                    trace: None,
                }
            }

            fn pending_with_signal_at(
                mut self,
                index: usize,
            ) -> (Self, futures::channel::oneshot::Receiver<()>) {
                let (sender, receiver) = futures::channel::oneshot::channel();
                let mut steps = self.steps.lock().unwrap();
                assert!(index <= steps.len());
                steps.insert(index, ResolutionStep::Pending);
                drop(steps);
                self.ready = Some(Mutex::new(Some(sender)));
                (self, receiver)
            }

            fn pending_with_signal(self) -> (Self, futures::channel::oneshot::Receiver<()>) {
                self.pending_with_signal_at(0)
            }

            pub(super) fn with_trace(mut self, trace: GenerationTrace) -> Self {
                self.trace = Some(trace);
                self
            }

            fn roles(&self) -> Vec<GenerationRole> {
                self.resolved.lock().unwrap().clone()
            }
        }

        fn profile_with_settings(
            slug: &str,
            script: Vec<AiEvent>,
            trace: &GenerationTrace,
            output_ceiling: Option<u64>,
            context_ceiling: Option<u64>,
            thinking: ThinkingLevel,
            parallel_tool_calls: bool,
        ) -> GenerationProfile {
            profile_with_expressibility(
                slug,
                script,
                trace,
                ProfileSettings {
                    output_ceiling,
                    context_ceiling,
                    thinking,
                    parallel_tool_calls,
                },
                Ok(()),
            )
        }

        struct ProfileSettings {
            output_ceiling: Option<u64>,
            context_ceiling: Option<u64>,
            thinking: ThinkingLevel,
            parallel_tool_calls: bool,
        }

        fn profile_with_expressibility(
            slug: &str,
            script: Vec<AiEvent>,
            trace: &GenerationTrace,
            settings: ProfileSettings,
            expressibility: Result<(), String>,
        ) -> GenerationProfile {
            let mut capabilities = caps();
            capabilities.max_context_tokens = settings.context_ceiling;
            capabilities.max_output_tokens = settings.output_ceiling;
            capabilities.supports_parallel_tool_calls = settings.parallel_tool_calls;
            let profile = GenerationProfile {
                identity: ego_core::config::ModelRef(slug.to_string()),
                request_model: slug.split_once('/').unwrap().1.to_string(),
                thinking: settings.thinking,
                generator: Arc::new(RecordingGenerator {
                    inner: FakeGenerator::new(vec![script]),
                    trace: trace.clone(),
                    expressibility,
                }),
                capabilities,
                metering: metering(),
                rates: Some(rates()),
            };
            assert_eq!(
                profile.output_ceiling(),
                settings.output_ceiling.or(settings.context_ceiling)
            );
            profile
        }

        fn refusing_profile(
            slug: &str,
            judge: bool,
            script: Vec<AiEvent>,
            trace: &GenerationTrace,
            output_ceiling: Option<u64>,
        ) -> GenerationProfile {
            profile_with_expressibility(
                slug,
                script,
                trace,
                ProfileSettings {
                    output_ceiling,
                    context_ceiling: Some(8_192),
                    thinking: if judge {
                        ThinkingLevel::Off
                    } else {
                        ThinkingLevel::High
                    },
                    parallel_tool_calls: false,
                },
                Err("the selected wire cannot encode this request".into()),
            )
        }

        pub(super) fn profile(
            slug: &str,
            judge: bool,
            script: Vec<AiEvent>,
            trace: &GenerationTrace,
            output_ceiling: Option<u64>,
        ) -> GenerationProfile {
            profile_with_settings(
                slug,
                script,
                trace,
                output_ceiling,
                Some(8_192),
                if judge || output_ceiling == Some(1_024) {
                    ThinkingLevel::Off
                } else {
                    ThinkingLevel::High
                },
                output_ceiling == Some(2_048),
            )
        }

        fn judge_context() -> JudgeContext {
            JudgeContext {
                confinement: RunConfinement::new(Confinement::Workspace, false, native_workspace())
                    .unwrap(),
                exec_caps: ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
            }
        }

        impl GenerationModelSource for ScriptedGenerationSource {
            fn resolve<'a>(
                &'a self,
                request: GenerationModelRequest,
            ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
                let role = request.role();
                self.resolved.lock().unwrap().push(role);
                if let Some(trace) = &self.trace {
                    trace
                        .steps
                        .lock()
                        .unwrap()
                        .push(format!("resolve:{role:?}"));
                    trace.timeline.lock().unwrap().push("model_resolve".into());
                }
                let step = self
                    .steps
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("the scripted source has one step per boundary");
                if matches!(step, ResolutionStep::Pending) {
                    if let Some(ready) = &self.ready
                        && let Some(sender) = ready.lock().unwrap().take()
                    {
                        let _ = sender.send(());
                    }
                    return Box::pin(futures::future::pending());
                }
                let ResolutionStep::Ready(result) = step else {
                    unreachable!("pending steps return above");
                };
                Box::pin(futures::future::ready(*result))
            }
        }

        fn profile_with_carryover(
            slug: &str,
            script: Vec<AiEvent>,
            trace: &GenerationTrace,
            carryover: ReasoningCarryover,
            dialect: Option<&str>,
        ) -> GenerationProfile {
            let mut profile = profile(slug, false, script, trace, Some(1_024));
            profile.capabilities.reasoning_carryover = carryover;
            profile.capabilities.reasoning_state_dialect = dialect.map(str::to_owned);
            profile
        }

        fn run_scripted(
            fx: &Fixture,
            state: SessionState,
            prompt: &str,
            source: Arc<ScriptedGenerationSource>,
            config: TurnConfig,
        ) -> (ExitReason, bool, SessionState) {
            let gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(state, &fx.store, &fx.clock, &fx.sink, CancelToken::new()),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source,
                    gate: gate.clone(),
                    config,
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            );
            let outcome = block_on(turn.run(prompt)).expect("the store accepted every append");
            (
                outcome.exit_reason,
                outcome.ok,
                turn.into_state().expect("unique turn journal state"),
            )
        }

        #[test]
        fn main_read_tool_main_resolves_fresh_profiles_with_current_caps() {
            let fx = Fixture::new(vec![
                vec![
                    AiEvent::TextDelta {
                        text: "before".into(),
                    },
                    AiEvent::ToolCall {
                        call_id: "call-1".into(),
                        name: "read".into(),
                        args: r#"{"path":"/ws/notes.md"}"#.into(),
                    },
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![
                    AiEvent::TextDelta {
                        text: "after".into(),
                    },
                    AiEvent::Stop(StopReason::Stop),
                ],
            ])
            .with_read_tool();
            fx.fs.file("/ws/notes.md", "seeded notes");
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile(
                        "openai/first",
                        false,
                        vec![
                            AiEvent::TextDelta {
                                text: "before".into(),
                            },
                            AiEvent::ToolCall {
                                call_id: "call-1".into(),
                                name: "read".into(),
                                args: r#"{"path":"/ws/notes.md"}"#.into(),
                            },
                            AiEvent::Stop(StopReason::ToolUse),
                        ],
                        &trace,
                        Some(2_048),
                    )),
                    Ok(profile_with_settings(
                        "openai/second",
                        vec![
                            AiEvent::TextDelta {
                                text: "after".into(),
                            },
                            AiEvent::Stop(StopReason::Stop),
                        ],
                        &trace,
                        None,
                        Some(4_096),
                        ThinkingLevel::Off,
                        false,
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source: source.clone(),
                    gate: gate.clone(),
                    config: TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            );
            block_on(turn.run("read this")).expect("the scripted turn succeeds");
            let names = fx.store.kinds();
            assert_eq!(
                names
                    .iter()
                    .filter(|name| *name == "generation_started")
                    .count(),
                2
            );
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                2
            );
            assert_eq!(
                source.roles(),
                vec![GenerationRole::Main, GenerationRole::Main]
            );
            let requests = trace.requests.lock().unwrap();
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request.model.as_str())
                    .collect::<Vec<_>>(),
                ["first", "second"]
            );
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request.max_output_tokens)
                    .collect::<Vec<_>>(),
                [Some(2_048), Some(4_096)]
            );
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request.thinking)
                    .collect::<Vec<_>>(),
                [ThinkingLevel::High, ThinkingLevel::Off]
            );
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request.parallel_tool_calls)
                    .collect::<Vec<_>>(),
                [true, false]
            );
            assert_eq!(
                trace.steps.lock().unwrap().as_slice(),
                [
                    "resolve:Main",
                    "generate:first",
                    "resolve:Main",
                    "generate:second"
                ]
            );
        }

        #[derive(Default)]
        struct AnalysisCache(
            Mutex<std::collections::HashMap<[u8; 32], ego_core::permit::analysis::AnalysisOpinion>>,
        );

        struct AnalysisLease<'a>(&'a AnalysisCache, [u8; 32]);
        impl ego_core::permit::analysis::AnalysisLease for AnalysisLease<'_> {
            fn lookup(
                &self,
            ) -> Result<Option<ego_core::permit::analysis::AnalysisOpinion>, String> {
                Ok(self.0.0.lock().unwrap().get(&self.1).copied())
            }
            fn insert(
                &self,
                opinion: ego_core::permit::analysis::AnalysisOpinion,
            ) -> Result<(), String> {
                self.0.0.lock().unwrap().insert(self.1, opinion);
                Ok(())
            }
        }
        impl ego_core::permit::analysis::PermissionAnalysisCache for AnalysisCache {
            fn acquire(
                &self,
                key: [u8; 32],
            ) -> BoxFuture<
                '_,
                Result<Box<dyn ego_core::permit::analysis::AnalysisLease + '_>, String>,
            > {
                Box::pin(async move {
                    Ok(Box::new(AnalysisLease(self, key))
                        as Box<dyn ego_core::permit::analysis::AnalysisLease>)
                })
            }
        }

        #[derive(Default)]
        struct ObservedProcess {
            calls: AtomicUsize,
            observations: AtomicUsize,
            movement: AtomicUsize,
            incomplete: bool,
        }
        impl ProcessExecutor for ObservedProcess {
            fn observe(&self, spec: &SpawnSpec) -> Result<SpawnSpec, ProcessError> {
                use ego_core::permit::analysis::{
                    ExecutableFileIdentity, ExecutionIdentity, IdentityFailure,
                };
                self.observations.fetch_add(1, Ordering::SeqCst);
                let path = CanonPath::new("/bin/image");
                let identity = ExecutionIdentity {
                    target: path.clone(),
                    files: vec![ExecutableFileIdentity {
                        named: path.clone(),
                        resolved: path,
                        content: Some([self.movement.load(Ordering::SeqCst) as u8; 32]),
                        platform: b"test-platform".to_vec(),
                    }],
                    failures: if self.incomplete {
                        vec![IdentityFailure::Unavailable]
                    } else {
                        Vec::new()
                    },
                    dynamic_code: true,
                };
                Ok(spec
                    .clone()
                    .with_execution_identity(EnvPolicy::exact(Vec::new()).unwrap(), identity))
            }
            fn revalidate(&self, _spec: &SpawnSpec) -> Result<(), ProcessError> {
                if self
                    .movement
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
                {
                    Err(ProcessError::Invariant("observed movement".into()))
                } else {
                    Ok(())
                }
            }
            fn execute<'a>(
                &'a self,
                _spec: &'a SpawnSpec,
            ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(ToolResult::builtin("executed")) })
            }
        }

        struct AnalysisSource {
            main: AtomicUsize,
            verdict: String,
            trace: GenerationTrace,
        }
        impl GenerationModelSource for AnalysisSource {
            fn resolve<'a>(
                &'a self,
                request: GenerationModelRequest,
            ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
                Box::pin(async move {
                    let judge = request.role() == GenerationRole::Judge;
                    let events = if judge {
                        vec![
                            AiEvent::TextDelta {
                                text: self.verdict.clone(),
                            },
                            AiEvent::Stop(StopReason::Stop),
                        ]
                    } else if self.main.fetch_add(1, Ordering::SeqCst) == 0 {
                        vec![
                            AiEvent::ToolCall {
                                call_id: "process".into(),
                                name: "exec".into(),
                                args: r#"{"argv":["image","arg"]}"#.into(),
                            },
                            AiEvent::Stop(StopReason::ToolUse),
                        ]
                    } else {
                        vec![AiEvent::Stop(StopReason::Stop)]
                    };
                    Ok(profile(
                        if judge { "openai/judge" } else { "openai/main" },
                        judge,
                        events,
                        &self.trace,
                        Some(1024),
                    ))
                })
            }
        }

        fn analysis_session(
            cache: Arc<AnalysisCache>,
            process: Arc<ObservedProcess>,
            trace: GenerationTrace,
            verdict: &str,
            mode: Mode,
            deny: bool,
        ) -> Vec<DomainEvent> {
            let mut fx = Fixture::new(Vec::new()).with_exec_tool();
            fx.set_mode(mode);
            fx.process = process;
            if deny {
                fx = fx.with_rules(&[], &["exec(image *)"]);
            }
            let source = Arc::new(AnalysisSource {
                main: AtomicUsize::new(0),
                verdict: verdict.into(),
                trace,
            });
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let runtime = GenerationRuntime {
                policy_source: fx.policy_source.clone(),
                source,
                gate: fx.gate.borrow().clone(),
                config: TurnConfig {
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    system: None,
                    identity: None,
                    token_ceiling: None,
                    retry: 0,
                    fallbacks: Vec::new(),
                },
                admission: Arc::new(EvidenceAdmissionService::new()),
                invocation: RunInvocationId::new(),
            };
            let observation = ego_core::permit::ExecObservation::new(
                ego_core::permit::BackendIdentity::new("test", "1"),
                ExecCaps::READ_SCOPED,
                "11".repeat(32).try_into().unwrap(),
            );
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                runtime,
                tools,
            )
            .with_judge(judge_context())
            .with_execution_observation(observation, None)
            .with_permission_analysis_cache(cache, Trust::Trusted);
            block_on(turn.run("run the process")).unwrap();
            drop(turn);
            fx.store.events()
        }

        #[test]
        fn analysis_two_sessions_generate_once_and_mint_fresh_call_audits() {
            let cache = Arc::new(AnalysisCache::default());
            let process = Arc::new(ObservedProcess::default());
            let trace = GenerationTrace::default();
            let verdict = r#"{"verdict":"allow","reason":"bounded by confinement"}"#;
            let first = analysis_session(
                cache.clone(),
                process.clone(),
                trace.clone(),
                verdict,
                Mode::Auto,
                false,
            );
            let second = analysis_session(
                cache,
                process.clone(),
                trace.clone(),
                verdict,
                Mode::Auto,
                false,
            );
            assert_eq!(
                trace
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|request| request.model == "judge")
                    .count(),
                1
            );
            assert_eq!(process.calls.load(Ordering::SeqCst), 2);
            assert!(first.iter().any(|event| matches!(
                event.kind,
                EventKind::PermissionDecided {
                    by: Some(Authority::Judge { .. }),
                    allowed: Some(true),
                    ..
                }
            )));
            assert!(second.iter().any(|event| matches!(
                event.kind,
                EventKind::PermissionDecided {
                    by: Some(Authority::CachedJudge { .. }),
                    allowed: Some(true),
                    ..
                }
            )));
            assert_ne!(first[0].env.turn, second[0].env.turn);
        }

        #[test]
        fn analysis_invalid_output_and_incomplete_allow_are_not_reused_but_escalation_is() {
            for (verdict, incomplete, expected_generations, expected_executions) in [
                ("{", false, 2, 0),
                (r#"{"verdict":"allow","reason":"ok"}"#, true, 2, 2),
                (r#"{"verdict":"escalate","reason":"unknown"}"#, false, 1, 0),
            ] {
                let cache = Arc::new(AnalysisCache::default());
                let process = Arc::new(ObservedProcess {
                    incomplete,
                    ..Default::default()
                });
                let trace = GenerationTrace::default();
                for _ in 0..2 {
                    analysis_session(
                        cache.clone(),
                        process.clone(),
                        trace.clone(),
                        verdict,
                        Mode::Auto,
                        false,
                    );
                }
                assert_eq!(
                    trace
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|request| request.model == "judge")
                        .count(),
                    expected_generations
                );
                assert_eq!(process.calls.load(Ordering::SeqCst), expected_executions);
            }
        }

        #[test]
        fn analysis_refreshes_once_and_repeated_movement_executes_nothing() {
            for (movement, executions) in [(1, 1), (2, 0)] {
                let cache = Arc::new(AnalysisCache::default());
                let process = Arc::new(ObservedProcess {
                    movement: AtomicUsize::new(movement),
                    ..Default::default()
                });
                let trace = GenerationTrace::default();
                analysis_session(
                    cache,
                    process.clone(),
                    trace,
                    r#"{"verdict":"allow","reason":"ok"}"#,
                    Mode::Auto,
                    false,
                );
                assert_eq!(process.calls.load(Ordering::SeqCst), executions);
                assert_eq!(process.observations.load(Ordering::SeqCst), 2);
            }
        }

        #[test]
        fn analysis_plan_and_yolo_skip_optional_generation() {
            for (mode, executions) in [(Mode::Plan, 0), (Mode::Yolo, 1)] {
                let process = Arc::new(ObservedProcess::default());
                let trace = GenerationTrace::default();
                analysis_session(
                    Arc::new(AnalysisCache::default()),
                    process.clone(),
                    trace.clone(),
                    r#"{"verdict":"allow","reason":"ok"}"#,
                    mode,
                    false,
                );
                assert_eq!(process.calls.load(Ordering::SeqCst), executions);
                assert_eq!(
                    trace
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|request| request.model == "judge")
                        .count(),
                    0
                );
            }
        }

        #[test]
        fn analysis_cached_allow_cannot_pass_a_new_deny_or_plan_mode() {
            let cache = Arc::new(AnalysisCache::default());
            let process = Arc::new(ObservedProcess::default());
            let trace = GenerationTrace::default();
            let verdict = r#"{"verdict":"allow","reason":"ok"}"#;
            analysis_session(
                cache.clone(),
                process.clone(),
                trace.clone(),
                verdict,
                Mode::Auto,
                false,
            );
            analysis_session(
                cache.clone(),
                process.clone(),
                trace.clone(),
                verdict,
                Mode::Auto,
                true,
            );
            analysis_session(
                cache,
                process.clone(),
                trace.clone(),
                verdict,
                Mode::Plan,
                false,
            );
            assert_eq!(process.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                trace
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|request| request.model == "judge")
                    .count(),
                1
            );
        }

        #[test]
        fn auto_process_judge_allow_resolves_fresh_judge_and_executes_the_prepared_process() {
            let mut fx = Fixture::new(vec![]).with_exec_tool();
            fx.set_mode(Mode::Auto);
            let process = Arc::new(ObservedProcess::default());
            fx.process = process.clone();
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile(
                        "openai/main",
                        false,
                        vec![
                            AiEvent::ToolCall {
                                call_id: "call-write".into(),
                                name: "exec".into(),
                                args: r#"{"argv":["image","arg"]}"#.into(),
                            },
                            AiEvent::Stop(StopReason::ToolUse),
                        ],
                        &trace,
                        Some(2_048),
                    )),
                    Ok(profile(
                        "openai/judge",
                        true,
                        vec![
                            AiEvent::TextDelta {
                                text: r#"{"verdict":"allow","reason":"bounded"}"#.into(),
                            },
                            AiEvent::Stop(StopReason::Stop),
                        ],
                        &trace,
                        None,
                    )),
                    Ok(profile(
                        "openai/main",
                        false,
                        vec![AiEvent::Stop(StopReason::Stop)],
                        &trace,
                        Some(1_024),
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let confinement =
                RunConfinement::new(Confinement::Workspace, false, native_workspace()).unwrap();
            let context = JudgeContext {
                confinement,
                exec_caps: ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
            };
            let gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source: source.clone(),
                    gate: gate.clone(),
                    config: TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            )
            .with_judge(context);
            block_on(turn.run("write this")).expect("auto judge allow completes the call");
            assert_eq!(
                source.roles(),
                vec![
                    GenerationRole::Main,
                    GenerationRole::Judge,
                    GenerationRole::Main
                ]
            );
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                3
            );
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                3
            );
            let requests = trace.requests.lock().unwrap();
            assert_eq!(requests[1].model, "judge");
            assert_eq!(requests[1].thinking, ThinkingLevel::Off);
            assert!(requests[1].tools.is_empty());
            assert!(!requests[1].parallel_tool_calls);
            assert_eq!(requests[1].max_output_tokens, Some(8_192));
            assert_eq!(
                trace.steps.lock().unwrap().as_slice(),
                [
                    "resolve:Main",
                    "generate:main",
                    "resolve:Judge",
                    "generate:judge",
                    "resolve:Main",
                    "generate:main",
                ]
            );
            assert_eq!(process.calls.load(Ordering::SeqCst), 1);
            let events = fx.store.events();
            let judge_attempt = events
                .iter()
                .find_map(|event| match &event.kind {
                    EventKind::GenerationStarted {
                        attempt,
                        role: GenerationRole::Judge,
                        snapshot: None,
                    } => Some(*attempt),
                    _ => None,
                })
                .expect("Judge generation is durable");
            assert!(events.iter().any(|event| matches!(
                &event.kind,
                EventKind::PermissionDecided {
                    allowed: Some(true),
                    by: Some(Authority::Judge { model, attempt }),
                    ..
                } if model == &ego_core::config::ModelRef("openai/judge".into()) && attempt == &judge_attempt
            )));
            assert!(fx.store.kinds().iter().any(|kind| kind.contains("tool")));
        }

        #[test]
        fn judge_context_without_unresolved_call_does_not_resolve_or_reserve() {
            let fx = Fixture::new(vec![vec![
                AiEvent::TextDelta {
                    text: "done".into(),
                },
                AiEvent::Stop(StopReason::Stop),
            ]]);
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([Ok(profile(
                    "openai/main",
                    false,
                    vec![
                        AiEvent::TextDelta {
                            text: "done".into(),
                        },
                        AiEvent::Stop(StopReason::Stop),
                    ],
                    &trace,
                    Some(2_048),
                ))])
                .with_trace(trace.clone()),
            );
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let gate = fx.gate.borrow_mut();
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source: source.clone(),
                    gate: gate.clone(),
                    config: TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            )
            .with_judge(judge_context());
            block_on(turn.run("hello")).expect("the scripted turn succeeds");
            drop(turn);
            assert_eq!(source.roles(), vec![GenerationRole::Main]);
            assert!(!fx.store.kinds().iter().any(|kind| kind == "judge_resolved"));
            assert!(!fx.store.events().iter().any(|event| matches!(
                event.kind,
                EventKind::PermissionDecided {
                    by: Some(Authority::Judge { .. }),
                    ..
                }
            )));
        }

        #[test]
        fn main_resolution_classes_refuse_without_start_or_reservation() {
            for (class, expected) in [
                (GenerationModelErrorClass::Usage, ExitReason::Usage),
                (GenerationModelErrorClass::Provider, ExitReason::Provider),
                (GenerationModelErrorClass::Internal, ExitReason::Internal),
            ] {
                let fx = Fixture::new(vec![]);
                let source = Arc::new(ScriptedGenerationSource::new([Err(
                    GenerationModelError::new(class, "resolution failed"),
                )]));
                let tools = Tools::new(
                    &fx.registry,
                    ToolEffectPorts::new(
                        &fx.fs,
                        fx.process.as_ref(),
                        &fx.evidence,
                        Arc::new(NoResolver),
                        Arc::new(NoHop),
                        Arc::new(NoDuplex),
                    ),
                    CanonPath::new("/ws"),
                );
                let gate = fx.gate.borrow_mut();
                let mut turn = TurnLoop::open(
                    session(
                        SessionState::default(),
                        &fx.store,
                        &fx.clock,
                        &fx.sink,
                        CancelToken::new(),
                    ),
                    GenerationRuntime {
                        policy_source: fx.policy_source.clone(),
                        source: source.clone(),
                        gate: gate.clone(),
                        config: TurnConfig {
                            cache: ego_core::generator::capabilities::CacheRetention::Short,
                            system: None,
                            identity: None,
                            token_ceiling: None,
                            retry: 0,
                            fallbacks: Vec::new(),
                        },
                        admission: Arc::new(EvidenceAdmissionService::new()),
                        invocation: RunInvocationId::new(),
                    },
                    tools,
                );
                let outcome = block_on(turn.run("hello")).expect("refusal is durable");
                drop(turn);
                assert_eq!(outcome.exit_reason, expected);
                assert!(
                    !fx.store
                        .kinds()
                        .iter()
                        .any(|kind| kind == "generation_started")
                );
                assert!(fx.store.kinds().iter().any(|kind| kind == "error"));
                assert_eq!(outstanding(&gate), Some(usd("0")));
                assert_eq!(committed(&gate), Some(usd("0")));
            }
        }

        #[test]
        fn main_expressibility_refusal_is_durable_before_metering_or_attempt() {
            let fx = Fixture::new(vec![]);
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([Ok(refusing_profile(
                    "openai/main",
                    false,
                    Vec::new(),
                    &trace,
                    Some(2_048),
                ))])
                .with_trace(trace.clone()),
            );
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let gate = fx.gate.borrow_mut();
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source: source.clone(),
                    gate: gate.clone(),
                    config: TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            );
            let outcome = block_on(turn.run("hello")).expect("refusal is durable");
            assert_eq!(outcome.exit_reason, ExitReason::Usage);
            drop(turn);
            assert_eq!(source.roles(), vec![GenerationRole::Main]);
            assert!(trace.requests.lock().unwrap().is_empty());
            assert!(
                !fx.store
                    .kinds()
                    .iter()
                    .any(|kind| kind == "generation_started")
            );
            assert!(
                !fx.sink
                    .events()
                    .iter()
                    .any(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
            );
            assert!(fx.store.kinds().iter().any(|kind| kind == "error"));
            assert_eq!(outstanding(&gate), Some(usd("0")));
            assert_eq!(committed(&gate), Some(usd("0")));
        }

        #[test]
        fn judge_expressibility_refusal_escalates_after_only_main_attempt() {
            let mut fx = Fixture::new(vec![]).with_exec_tool();
            fx.set_mode(Mode::Auto);
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile(
                        "openai/main",
                        false,
                        vec![
                            AiEvent::ToolCall {
                                call_id: "write".into(),
                                name: "exec".into(),
                                args: r#"{"argv":["image","arg"]}"#.into(),
                            },
                            AiEvent::Stop(StopReason::ToolUse),
                        ],
                        &trace,
                        Some(2_048),
                    )),
                    Ok(refusing_profile(
                        "openai/judge",
                        true,
                        Vec::new(),
                        &trace,
                        None,
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source: source.clone(),
                    gate: gate.clone(),
                    config: TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            )
            .with_judge(judge_context());
            let outcome = block_on(turn.run("write")).expect("judge refusal is durable");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            drop(turn);
            assert_eq!(
                source.roles(),
                vec![GenerationRole::Main, GenerationRole::Judge]
            );
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                1
            );
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                1
            );
            assert_eq!(
                trace
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|request| request.model.as_str())
                    .collect::<Vec<_>>(),
                ["main"]
            );
            assert!(fx.store.kinds().iter().any(|kind| kind == "error"));
            assert!(!fx.store.events().iter().any(|event| matches!(
                event.kind,
                EventKind::PermissionDecided {
                    by: Some(Authority::Judge { .. }),
                    ..
                }
            )));
            assert_eq!(outstanding(&gate), Some(usd("0")));
            assert_eq!(committed(&gate), Some(usd("0.06144")));
        }

        #[test]
        fn controlled_cancellation_before_resolution_creates_no_attempt() {
            let fx = Fixture::new(vec![]);
            let trace = GenerationTrace::default();
            let (source, ready) = ScriptedGenerationSource::new([Ok(profile(
                "openai/main",
                false,
                Vec::new(),
                &trace,
                Some(2_048),
            ))])
            .pending_with_signal();
            let source = Arc::new(source);
            let (ingress, mut control) = ego_core::turn::interactive_control();
            let cancel = CancelToken::new();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let gate = fx.gate.borrow_mut();
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    cancel,
                ),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source: source.clone(),
                    gate: gate.clone(),
                    config: TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            )
            .with_interactive_control(&mut control);
            let active = turn.run("hello");
            let drive = async move {
                ready.await.expect("resolution reached the controlled wait");
                ingress.interrupt().await.expect("interrupt is accepted");
            };
            let _ = block_on(async { futures::join!(active, drive) });
            drop(turn);
            assert!(
                !fx.store
                    .kinds()
                    .iter()
                    .any(|kind| kind == "generation_started")
            );
            assert_eq!(outstanding(&gate), Some(usd("0")));
            assert_eq!(committed(&gate), Some(usd("0")));
        }

        #[test]
        fn unresolved_process_without_judge_context_is_headless_and_does_not_resolve_judge() {
            let mut fx = Fixture::new(vec![]).with_exec_tool();
            fx.set_mode(Mode::Auto);
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([Ok(profile(
                    "openai/main",
                    false,
                    vec![
                        AiEvent::ToolCall {
                            call_id: "write".into(),
                            name: "exec".into(),
                            args: r#"{"argv":["image","arg"]}"#.into(),
                        },
                        AiEvent::Stop(StopReason::ToolUse),
                    ],
                    &trace,
                    Some(2_048),
                ))])
                .with_trace(trace),
            );
            let gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source: source.clone(),
                    gate: gate.clone(),
                    config: TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            );
            let outcome = block_on(turn.run("write")).expect("headless denial is durable");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            assert_eq!(source.roles(), vec![GenerationRole::Main]);
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                1
            );
        }

        #[test]
        fn explicitly_denied_write_skips_judge_even_with_context() {
            let fx = Fixture::new(vec![])
                .with_write_tool()
                .with_rules(&[], &["write(**)"]);
            let trace = GenerationTrace::default();
            let source = Arc::new(ScriptedGenerationSource::new([Ok(profile(
                "openai/main",
                false,
                vec![
                    AiEvent::ToolCall {
                        call_id: "write".into(),
                        name: "fake_write".into(),
                        args: r#"{"path":"/ws/out","contents":"ok"}"#.into(),
                    },
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                &trace,
                Some(2_048),
            ))]));
            let gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source: source.clone(),
                    gate: gate.clone(),
                    config: TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            )
            .with_judge(judge_context());
            let outcome = block_on(turn.run("write")).expect("denial is durable");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            assert_eq!(source.roles(), vec![GenerationRole::Main]);
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                1
            );
        }

        #[test]
        fn judge_resolution_errors_are_headless_without_judge_attempt() {
            for class in [
                GenerationModelErrorClass::Usage,
                GenerationModelErrorClass::Provider,
                GenerationModelErrorClass::Internal,
            ] {
                let mut fx = Fixture::new(vec![]).with_exec_tool();
                fx.set_mode(Mode::Auto);
                let trace = GenerationTrace::default();
                let source = Arc::new(
                    ScriptedGenerationSource::new([
                        Ok(profile(
                            "openai/main",
                            false,
                            vec![
                                AiEvent::ToolCall {
                                    call_id: "write".into(),
                                    name: "exec".into(),
                                    args: r#"{"argv":["image","arg"]}"#.into(),
                                },
                                AiEvent::Stop(StopReason::ToolUse),
                            ],
                            &trace,
                            Some(2_048),
                        )),
                        Err(GenerationModelError::new(class, "judge unavailable")),
                    ])
                    .with_trace(trace.clone()),
                );
                let gate = fx.gate.borrow_mut();
                let tools = Tools::new(
                    &fx.registry,
                    ToolEffectPorts::new(
                        &fx.fs,
                        fx.process.as_ref(),
                        &fx.evidence,
                        Arc::new(NoResolver),
                        Arc::new(NoHop),
                        Arc::new(NoDuplex),
                    ),
                    CanonPath::new("/ws"),
                );
                let mut turn = TurnLoop::open(
                    session(
                        SessionState::default(),
                        &fx.store,
                        &fx.clock,
                        &fx.sink,
                        CancelToken::new(),
                    ),
                    GenerationRuntime {
                        policy_source: fx.policy_source.clone(),
                        source: source.clone(),
                        gate: gate.clone(),
                        config: TurnConfig {
                            cache: ego_core::generator::capabilities::CacheRetention::Short,
                            system: None,
                            identity: None,
                            token_ceiling: None,
                            retry: 0,
                            fallbacks: Vec::new(),
                        },
                        admission: Arc::new(EvidenceAdmissionService::new()),
                        invocation: RunInvocationId::new(),
                    },
                    tools,
                )
                .with_judge(judge_context());
                let outcome = block_on(turn.run("write")).expect("judge error is durable");
                assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
                assert_eq!(
                    source.roles(),
                    vec![GenerationRole::Main, GenerationRole::Judge]
                );
                assert_eq!(
                    fx.store
                        .kinds()
                        .iter()
                        .filter(|kind| *kind == "generation_started")
                        .count(),
                    1
                );
                assert_eq!(
                    fx.sink
                        .events()
                        .iter()
                        .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                        .count(),
                    1
                );
                assert!(fx.store.kinds().iter().any(|kind| kind == "error"));
                assert_eq!(
                    trace.steps.lock().unwrap().as_slice(),
                    ["resolve:Main", "generate:main", "resolve:Judge",]
                );
                assert!(!fx.store.events().iter().any(|event| matches!(
                    event.kind,
                    EventKind::PermissionDecided {
                        by: Some(Authority::Judge { .. }),
                        ..
                    }
                )));
                drop(turn);
                assert_eq!(outstanding(&gate), Some(usd("0")));
                assert_eq!(committed(&gate), Some(usd("0.06144")));
            }
        }

        #[test]
        fn judge_pending_cancellation_has_no_judge_attempt_or_reservation() {
            let mut fx = Fixture::new(vec![]).with_exec_tool();
            fx.set_mode(Mode::Auto);
            let cache = Arc::new(AnalysisCache::default());
            let trace = GenerationTrace::default();
            let (source, ready) = ScriptedGenerationSource::new([
                Ok(profile(
                    "openai/main",
                    false,
                    vec![
                        AiEvent::ToolCall {
                            call_id: "write".into(),
                            name: "exec".into(),
                            args: r#"{"argv":["image","arg"]}"#.into(),
                        },
                        AiEvent::Stop(StopReason::ToolUse),
                    ],
                    &trace,
                    Some(2_048),
                )),
                Ok(profile("openai/judge", true, vec![], &trace, None)),
            ])
            .pending_with_signal_at(1);
            let source = Arc::new(source);
            let (ingress, mut control) = ego_core::turn::interactive_control();
            let cancel = CancelToken::new();
            let gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    cancel,
                ),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source: source.clone(),
                    gate: gate.clone(),
                    config: TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            )
            .with_judge(judge_context())
            .with_interactive_control(&mut control)
            .with_permission_analysis_cache(cache.clone(), Trust::Trusted);
            let active = turn.run("write");
            let drive = async move {
                ready.await.expect("Judge resolution reached pending");
                ingress.interrupt().await.expect("interrupt accepted");
            };
            let (outcome, ()) = block_on(async { futures::join!(active, drive) });
            assert_eq!(
                outcome.expect("cancellation is durable").exit_reason,
                ExitReason::Cancelled(CancelCause::User)
            );
            drop(turn);
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                1
            );
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                1
            );
            assert!(!fx.store.kinds().iter().any(|kind| kind == "error"));
            assert!(!fx.store.events().iter().any(|event| matches!(
                event.kind,
                EventKind::PermissionDecided {
                    by: Some(Authority::Judge { .. }),
                    ..
                }
            )));
            assert_eq!(outstanding(&gate), Some(usd("0")));
            assert_eq!(committed(&gate), Some(usd("0.06144")));
            assert!(cache.0.lock().unwrap().is_empty());
        }

        #[test]
        fn judge_capability_gap_is_headless_without_judge_attempt() {
            let mut fx = Fixture::new(vec![]).with_exec_tool();
            fx.set_mode(Mode::Auto);
            let trace = GenerationTrace::default();
            let mut judge = profile_with_settings(
                "openai/judge",
                vec![],
                &trace,
                Some(8_192),
                Some(8_192),
                ThinkingLevel::Off,
                false,
            );
            judge.capabilities.reasoning_efforts =
                ReasoningEffortSupport::Only(BTreeSet::from([ThinkingLevel::High]));
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile(
                        "openai/main",
                        false,
                        vec![
                            AiEvent::ToolCall {
                                call_id: "write".into(),
                                name: "exec".into(),
                                args: r#"{"argv":["image","arg"]}"#.into(),
                            },
                            AiEvent::Stop(StopReason::ToolUse),
                        ],
                        &trace,
                        Some(2_048),
                    )),
                    Ok(judge),
                ])
                .with_trace(trace.clone()),
            );
            let gate = fx.gate.borrow_mut();
            let tools = Tools::new(
                &fx.registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                ),
                CanonPath::new("/ws"),
            );
            let mut turn = TurnLoop::open(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    CancelToken::new(),
                ),
                GenerationRuntime {
                    policy_source: fx.policy_source.clone(),
                    source: source.clone(),
                    gate: gate.clone(),
                    config: TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                    admission: Arc::new(EvidenceAdmissionService::new()),
                    invocation: RunInvocationId::new(),
                },
                tools,
            )
            .with_judge(judge_context());
            let outcome = block_on(turn.run("write")).expect("capability gap is durable");
            assert_eq!(outcome.exit_reason, ExitReason::PermissionRequired);
            assert_eq!(
                source.roles(),
                vec![GenerationRole::Main, GenerationRole::Judge]
            );
            assert_eq!(
                fx.store
                    .kinds()
                    .iter()
                    .filter(|kind| *kind == "generation_started")
                    .count(),
                1
            );
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                1
            );
            assert!(fx.store.kinds().iter().any(|kind| kind == "error"));
            assert_eq!(
                trace.steps.lock().unwrap().as_slice(),
                ["resolve:Main", "generate:main", "resolve:Judge",]
            );
            assert!(!fx.store.events().iter().any(|event| matches!(
                event.kind,
                EventKind::PermissionDecided {
                    by: Some(Authority::Judge { .. }),
                    ..
                }
            )));
            drop(turn);
            assert_eq!(outstanding(&gate), Some(usd("0")));
            assert_eq!(committed(&gate), Some(usd("0.06144")));
        }

        /// The occupancy report names the window of the model that produced
        /// it, not the one the turn started out intending to use.
        ///
        /// A caller resolves a profile once before the loop — `ego-cli`'s
        /// `generation_preflight` does, and hands its `ModelCapabilities` to
        /// the sink for the lifetime of the turn. The loop then walks its
        /// fallback candidates and resolves a fresh profile per candidate, so
        /// the model that answers can have a different ceiling. A consumer
        /// dividing this occupancy by the caller's window would render a
        /// session as comfortably inside a limit it is over, which is worse
        /// than showing nothing.
        #[test]
        fn the_occupancy_report_carries_the_window_of_the_model_that_produced_it() {
            const PRIMARY_WINDOW: u64 = 200_000;
            const FALLBACK_WINDOW: u64 = 32_768;
            let primary = "ollama/primary-model";
            let fallback_slug = "openai/fallback-model";
            let trace = GenerationTrace::default();
            let windowed = |slug: &str, script: Vec<AiEvent>, window: u64| {
                profile_with_settings(
                    slug,
                    script,
                    &trace,
                    Some(1_024),
                    Some(window),
                    ThinkingLevel::Off,
                    false,
                )
            };
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(windowed(
                        primary,
                        vec![
                            AiEvent::Error {
                                message: "temporary refusal before write".to_owned(),
                                kind: ErrorKind::Transient,
                            },
                            AiEvent::DeliveryObserved(DeliveryCertainty::DefinitelyUnsent),
                            AiEvent::Stop(StopReason::Error),
                        ],
                        PRIMARY_WINDOW,
                    )),
                    Ok(windowed(
                        fallback_slug,
                        vec![
                            AiEvent::TextDelta {
                                text: "fallback answer".to_owned(),
                            },
                            AiEvent::Usage(tokens(2, 0, 0, 3)),
                            AiEvent::Stop(StopReason::Stop),
                        ],
                        FALLBACK_WINDOW,
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let fx = Fixture::new(vec![]);
            let (_, ok, state) = run_scripted(
                &fx,
                SessionState::default(),
                "which model answered",
                source,
                TurnConfig {
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    system: None,
                    identity: None,
                    token_ceiling: None,
                    retry: 0,
                    fallbacks: vec![ModelRef(fallback_slug.to_owned())],
                },
            );
            assert!(ok, "the fallback attempt completed");

            let windows: Vec<_> = fx
                .store
                .events()
                .iter()
                .filter_map(|event| match event.kind {
                    EventKind::ContextWindowUpdated { window, .. } => Some(window),
                    _ => None,
                })
                .collect();
            assert_eq!(
                windows,
                vec![Some(FALLBACK_WINDOW)],
                "the occupancy was recorded against the wrong model's ceiling"
            );
            assert_eq!(state.context.window, Some(FALLBACK_WINDOW));
        }

        /// regression case: one logical generation owns an ordered, delivery-safe
        /// retry/fallback plan. Every wire journey is fresh, but all four
        /// attempts remain attributable to the same generation.
        /// Attack on the runtime identity: rendered once from the primary, a
        /// fallback's request would tell the model it is a model it is not.
        #[test]
        fn a_fallback_generation_names_the_fallback_model_in_its_identity() {
            let unsent = vec![
                AiEvent::Error {
                    message: "refused before write".to_owned(),
                    kind: ErrorKind::Transient,
                },
                AiEvent::DeliveryObserved(DeliveryCertainty::DefinitelyUnsent),
                AiEvent::Stop(StopReason::Error),
            ];
            let answered = vec![
                AiEvent::TextDelta {
                    text: "fallback answer".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ];
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile_with_carryover(
                        "ollama/primary-model",
                        unsent,
                        &trace,
                        ReasoningCarryover::Portable,
                        None,
                    )),
                    Ok(profile_with_carryover(
                        "openai/fallback-model",
                        answered,
                        &trace,
                        ReasoningCarryover::Portable,
                        None,
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let fx = Fixture::new(vec![]);
            let (_, ok, _) = run_scripted(
                &fx,
                SessionState::default(),
                "which model?",
                source,
                TurnConfig {
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    system: None,
                    identity: Some(ego_core::turn::AgentIdentity {
                        version: "0.0.0".to_owned(),
                        sandbox: Confinement::Workspace,
                        offline: false,
                        host: None,
                    }),
                    token_ceiling: None,
                    retry: 0,
                    fallbacks: vec![ModelRef("openai/fallback-model".to_owned())],
                },
            );
            assert!(ok, "the fallback answered");

            let identities: Vec<String> = trace
                .requests
                .lock()
                .unwrap()
                .iter()
                .map(|request| {
                    request
                        .messages
                        .messages()
                        .iter()
                        .filter(|message| message.role == Role::User)
                        .flat_map(|message| &message.content)
                        .find_map(|block| match block {
                            ContentBlock::Text { text }
                                if text.starts_with("Current context data.") =>
                            {
                                Some(text.clone())
                            }
                            _ => None,
                        })
                        .expect("live identity data")
                })
                .collect();
            assert_eq!(identities.len(), 2);
            assert!(
                identities[0].contains("- Model: ollama/primary-model ("),
                "{}",
                identities[0]
            );
            assert!(
                identities[1].contains("- Model: openai/fallback-model ("),
                "{}",
                identities[1]
            );
        }

        #[test]
        fn retry_and_fallback_are_ordered_per_candidate_and_delivery_safe() {
            let primary = "ollama/primary-model";
            let fallback_slug = "openai/fallback-model";
            let transient_unsent = || {
                vec![
                    AiEvent::Error {
                        message: "temporary refusal before write".to_owned(),
                        kind: ErrorKind::Transient,
                    },
                    AiEvent::DeliveryObserved(DeliveryCertainty::DefinitelyUnsent),
                    AiEvent::Stop(StopReason::Error),
                ]
            };
            let success = || {
                vec![
                    AiEvent::TextDelta {
                        text: "fallback answer".to_owned(),
                    },
                    AiEvent::Usage(tokens(2, 0, 0, 3)),
                    AiEvent::Stop(StopReason::Stop),
                ]
            };
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile_with_carryover(
                        primary,
                        transient_unsent(),
                        &trace,
                        ReasoningCarryover::Portable,
                        None,
                    )),
                    Ok(profile_with_carryover(
                        primary,
                        transient_unsent(),
                        &trace,
                        ReasoningCarryover::Portable,
                        None,
                    )),
                    Ok(profile_with_carryover(
                        fallback_slug,
                        transient_unsent(),
                        &trace,
                        ReasoningCarryover::Portable,
                        None,
                    )),
                    Ok(profile_with_carryover(
                        fallback_slug,
                        success(),
                        &trace,
                        ReasoningCarryover::Portable,
                        None,
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let fx = Fixture::new(vec![]);
            let fallback = ModelRef(fallback_slug.to_owned());
            let (_, ok, state) = run_scripted(
                &fx,
                SessionState::default(),
                "retry safely",
                source.clone(),
                TurnConfig {
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    system: None,
                    identity: None,
                    token_ceiling: None,
                    retry: 1,
                    fallbacks: vec![fallback.clone()],
                },
            );
            assert!(ok, "the final fallback attempt completed");

            let events = fx.store.events();
            let started: Vec<_> = events
                .iter()
                .filter_map(|event| match event.kind {
                    EventKind::GenerationStarted { attempt, .. } => Some(attempt),
                    _ => None,
                })
                .collect();
            let settled: Vec<_> = events
                .iter()
                .filter_map(|event| match event.kind {
                    EventKind::AttemptSettled { attempt, .. } => Some(attempt),
                    _ => None,
                })
                .collect();
            assert_eq!(started.len(), 4);
            assert_eq!(settled.len(), 4);
            assert_eq!(
                started
                    .iter()
                    .map(|attempt| attempt.generation())
                    .collect::<BTreeSet<_>>()
                    .len(),
                1,
                "retries and fallback share one logical GenerationId"
            );
            assert_eq!(
                started
                    .iter()
                    .map(|attempt| attempt.attempt())
                    .collect::<BTreeSet<_>>()
                    .len(),
                4,
                "every wire journey has a fresh AttemptId"
            );
            assert_eq!(
                trace
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|request| request.model.as_str())
                    .collect::<Vec<_>>(),
                [
                    "primary-model",
                    "primary-model",
                    "fallback-model",
                    "fallback-model"
                ]
            );
            assert_eq!(
                trace.steps.lock().unwrap().as_slice(),
                [
                    "resolve:Main",
                    "generate:primary-model",
                    "resolve:Main",
                    "generate:primary-model",
                    "resolve:Main",
                    "generate:fallback-model",
                    "resolve:Main",
                    "generate:fallback-model",
                ]
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event.kind, EventKind::UsageUpdated { .. }))
                    .count(),
                4,
                "missing usage still contributes one accounting line per attempt"
            );
            assert!(state.live_attempts.is_empty());

            // A safely unsent permanent failure advances to the next
            // candidate without spending this candidate's retry allowance.
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile(
                        primary,
                        false,
                        vec![
                            AiEvent::Error {
                                message: "permanent refusal".to_owned(),
                                kind: ErrorKind::Refused,
                            },
                            AiEvent::DeliveryObserved(DeliveryCertainty::DefinitelyUnsent),
                            AiEvent::Stop(StopReason::Error),
                        ],
                        &trace,
                        Some(1_024),
                    )),
                    Ok(profile(
                        fallback_slug,
                        false,
                        success(),
                        &trace,
                        Some(1_024),
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let fx = Fixture::new(vec![]);
            let (_, ok, _) = run_scripted(
                &fx,
                SessionState::default(),
                "permanent then fallback",
                source,
                TurnConfig {
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    system: None,
                    identity: None,
                    token_ceiling: None,
                    retry: 1,
                    fallbacks: vec![fallback.clone()],
                },
            );
            assert!(ok);
            assert_eq!(
                trace
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|request| request.model.as_str())
                    .collect::<Vec<_>>(),
                ["primary-model", "fallback-model"]
            );

            // A possibly-sent transient failure is terminal for automatic
            // delivery, even though its error class would otherwise retry.
            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([Ok(profile(
                    primary,
                    false,
                    vec![
                        AiEvent::Error {
                            message: "connection lost after write".to_owned(),
                            kind: ErrorKind::Transient,
                        },
                        AiEvent::DeliveryObserved(DeliveryCertainty::PossiblySent),
                        AiEvent::Stop(StopReason::Error),
                    ],
                    &trace,
                    Some(1_024),
                ))])
                .with_trace(trace.clone()),
            );
            let fx = Fixture::new(vec![]);
            let (_, ok, _) = run_scripted(
                &fx,
                SessionState::default(),
                "possibly sent",
                source,
                TurnConfig {
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    system: None,
                    identity: None,
                    token_ceiling: None,
                    retry: 1,
                    fallbacks: vec![fallback.clone()],
                },
            );
            assert!(!ok);
            assert_eq!(trace.requests.lock().unwrap().len(), 1);
            assert_eq!(
                trace.steps.lock().unwrap().as_slice(),
                ["resolve:Main", "generate:primary-model"]
            );
        }

        /// regression case: fallback preflight may remove only opaque reasoning
        /// state, and must make both the removal and a skipped candidate
        /// visible without exposing the state blob.
        #[test]
        fn fallback_preflight_drops_reasoning_or_typed_skips_without_admission() {
            const OPAQUE: &str = "provider-private-reasoning-signature";
            let fx = Fixture::new(vec![vec![
                AiEvent::ReasoningDelta {
                    text: "private thought".to_owned(),
                },
                AiEvent::ReasoningState(ReasoningState {
                    blob: OPAQUE.to_owned(),
                    dialect: "primary-dialect".to_owned(),
                }),
                AiEvent::TextDelta {
                    text: "ordinary answer".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ]]);
            let (_, _, state) = run(&fx, SessionState::default(), "seed ordinary content");
            assert!(
                !state.items.is_empty(),
                "the fallback has a durable history"
            );

            let trace = GenerationTrace::default();
            let primary = "ollama/primary-model";
            let fallback = "anthropic/fallback-model";
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile_with_carryover(
                        primary,
                        vec![
                            AiEvent::Error {
                                message: "temporary refusal before write".to_owned(),
                                kind: ErrorKind::Transient,
                            },
                            AiEvent::DeliveryObserved(DeliveryCertainty::DefinitelyUnsent),
                            AiEvent::Stop(StopReason::Error),
                        ],
                        &trace,
                        ReasoningCarryover::Portable,
                        None,
                    )),
                    Ok(profile_with_carryover(
                        fallback,
                        vec![
                            AiEvent::TextDelta {
                                text: "fallback answer".to_owned(),
                            },
                            AiEvent::Stop(StopReason::Stop),
                        ],
                        &trace,
                        ReasoningCarryover::SameProviderOnly,
                        Some("fallback-dialect"),
                    )),
                ])
                .with_trace(trace.clone()),
            );
            let (_, ok, _) = run_scripted(
                &fx,
                state,
                "use the fallback",
                source,
                TurnConfig {
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    system: None,
                    identity: None,
                    token_ceiling: None,
                    retry: 0,
                    fallbacks: vec![ModelRef(fallback.to_owned())],
                },
            );
            assert!(ok);
            let requests = trace.requests.lock().unwrap();
            let fallback_request = requests
                .iter()
                .find(|request| request.model == "fallback-model")
                .expect("the reasoning-drop fallback reached its generator");
            let blocks: Vec<_> = fallback_request
                .messages
                .messages()
                .iter()
                .flat_map(|message| message.content.iter())
                .collect();
            assert!(blocks.iter().any(|block| matches!(
                block,
                ContentBlock::Text { text } if text == "seed ordinary content"
            )));
            assert!(blocks.iter().any(|block| matches!(
                block,
                ContentBlock::Text { text } if text == "ordinary answer"
            )));
            assert!(
                blocks
                    .iter()
                    .all(|block| !matches!(block, ContentBlock::Reasoning { .. })),
                "only opaque reasoning state was removed"
            );
            drop(requests);

            let decision = fx
                .store
                .events()
                .into_iter()
                .find_map(|event| match event.kind {
                    EventKind::FallbackDecision {
                        candidate,
                        decision:
                            ego_core::event::domain::FallbackDecision::ReasoningDropped {
                                dropped_blocks,
                                dialects,
                            },
                    } => Some((candidate, dropped_blocks, dialects)),
                    _ => None,
                })
                .expect("reasoning removal is durable typed evidence");
            assert_eq!(decision.0.0, fallback);
            assert_eq!(decision.1, 1);
            assert_eq!(decision.2, vec!["primary-dialect".to_owned()]);

            let public = fx
                .sink
                .events()
                .into_iter()
                .find(|event| matches!(event, PublicEvent::FallbackDecision { .. }))
                .expect("reasoning removal is visible to the public projection");
            let public_json = serde_json::to_value(public).expect("public event is serialisable");
            let public_text = public_json.to_string();
            assert!(public_text.contains(fallback));
            assert!(public_text.contains("primary-dialect"));
            assert!(public_text.contains("1"));
            assert!(!public_text.contains(OPAQUE));

            // A non-reasoning gap is skipped before the gate and generator.
            let trace = GenerationTrace::default();
            let mut incapable = profile_with_settings(
                "openai/too-small",
                vec![],
                &trace,
                Some(1_024),
                Some(8_192),
                ThinkingLevel::Off,
                false,
            );
            incapable.capabilities.max_tools = Some(0);
            let source = Arc::new(
                ScriptedGenerationSource::new([
                    Ok(profile(
                        primary,
                        false,
                        vec![
                            AiEvent::Error {
                                message: "temporary refusal before write".to_owned(),
                                kind: ErrorKind::Transient,
                            },
                            AiEvent::DeliveryObserved(DeliveryCertainty::DefinitelyUnsent),
                            AiEvent::Stop(StopReason::Error),
                        ],
                        &trace,
                        Some(1_024),
                    )),
                    Ok(incapable),
                ])
                .with_trace(trace.clone()),
            );
            let fx = Fixture::new(vec![]).with_read_tool();
            let (_, _, _) = run_scripted(
                &fx,
                SessionState::default(),
                "skip the candidate",
                source,
                TurnConfig {
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    system: None,
                    identity: None,
                    token_ceiling: None,
                    retry: 0,
                    fallbacks: vec![ModelRef("openai/too-small".to_owned())],
                },
            );
            assert_eq!(trace.requests.lock().unwrap().len(), 1);
            assert_eq!(
                fx.sink
                    .events()
                    .iter()
                    .filter(|event| matches!(event, PublicEvent::MeteringSelected { .. }))
                    .count(),
                1,
                "a skipped candidate never reaches reservation admission"
            );
            let skipped = fx
                .store
                .events()
                .into_iter()
                .find_map(|event| match event.kind {
                    EventKind::FallbackDecision {
                        candidate,
                        decision: ego_core::event::domain::FallbackDecision::Skipped { gaps },
                    } => Some((candidate, gaps)),
                    _ => None,
                })
                .expect("the incapable candidate is durably skipped");
            assert_eq!(skipped.0.0, "openai/too-small");
            assert!(
                skipped
                    .1
                    .iter()
                    .any(|gap| matches!(gap, CapabilityGap::TooManyTools { .. }))
            );
        }

        /// regression case: accounting names definitely-unsent missing usage as
        /// known zero, while possible delivery remains unknown. A replayed
        /// live attempt refuses before accepting a new prompt or resolving a
        /// model.
        #[test]
        fn missing_usage_is_attributed_and_live_replay_is_refused_before_prompt() {
            let run_case = |certainty| {
                let trace = GenerationTrace::default();
                let source = Arc::new(
                    ScriptedGenerationSource::new([Ok(profile(
                        "ollama/accounting-model",
                        false,
                        vec![
                            AiEvent::DeliveryObserved(certainty),
                            AiEvent::Stop(StopReason::Error),
                        ],
                        &trace,
                        Some(1_024),
                    ))])
                    .with_trace(trace.clone()),
                );
                let fx = Fixture::new(vec![]);
                let (_, _, _) = run_scripted(
                    &fx,
                    SessionState::default(),
                    "account missing usage",
                    source,
                    TurnConfig {
                        cache: ego_core::generator::capabilities::CacheRetention::Short,
                        system: None,
                        identity: None,
                        token_ceiling: None,
                        retry: 0,
                        fallbacks: Vec::new(),
                    },
                );
                let usage = fx
                    .store
                    .events()
                    .into_iter()
                    .find_map(|event| match event.kind {
                        EventKind::UsageUpdated {
                            usage, cost_delta, ..
                        } => Some((usage, cost_delta)),
                        _ => None,
                    })
                    .expect("every settled attempt contributes accounting");
                (fx, usage)
            };

            let (_, usage) = run_case(DeliveryCertainty::DefinitelyUnsent);
            assert_eq!(usage.0, Usage::IDENTITY);
            assert!(matches!(
                usage.1,
                CostDelta::Known {
                    amount,
                    attribution: CostAttribution::DefinitelyUnsent,
                    fx: None,
                } if amount == usd("0")
            ));

            let (_, usage) = run_case(DeliveryCertainty::PossiblySent);
            assert_eq!(usage.0, Usage::IDENTITY);
            assert!(matches!(usage.1, CostDelta::Unknown(_)));

            let attempt = AttemptRef::first(ego_core::event::id::GenerationId::new());
            let turn_id = TurnId::new();
            let mut seq = Seq::ZERO;
            let seeded = DomainEvent {
                env: Envelope {
                    seq: seq.next(),
                    id: EventId::new(),
                    turn: turn_id,
                    item: None,
                    parent: None,
                    ts: Timestamp::from_millis(1_787_000_000_000),
                    producer: Producer::Main,
                },
                kind: EventKind::GenerationStarted {
                    attempt,
                    role: ego_core::generation_model_source::GenerationRole::Main,
                    snapshot: None,
                },
            };
            let mut state = SessionState::default();
            state.apply(&seeded);
            assert_eq!(state.live_attempts.len(), 1);

            let trace = GenerationTrace::default();
            let source = Arc::new(
                ScriptedGenerationSource::new([Ok(profile(
                    "ollama/never-reached",
                    false,
                    vec![AiEvent::Stop(StopReason::Stop)],
                    &trace,
                    Some(1_024),
                ))])
                .with_trace(trace.clone()),
            );
            let fx = Fixture::new(vec![]);
            let _ = run_scripted(
                &fx,
                state,
                "must refuse live replay",
                source,
                TurnConfig {
                    cache: ego_core::generator::capabilities::CacheRetention::Short,
                    system: None,
                    identity: None,
                    token_ceiling: None,
                    retry: 0,
                    fallbacks: Vec::new(),
                },
            );
            assert!(trace.requests.lock().unwrap().is_empty());
            assert!(trace.steps.lock().unwrap().is_empty());
            assert!(
                fx.store
                    .events()
                    .iter()
                    .all(|event| !matches!(event.kind, EventKind::UserPrompt { .. }))
            );
        }
    }

    #[test]
    fn completed_model_skill_turn_removes_invocation_hooks_before_next_turn() {
        let location =
            ego_core::event::skill::SkillSourceLocation::parse("skills/explain/SKILL.md")
                .expect("skill location");
        let source_digest = ego_core::event::skill::SkillContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("skill digest");
        let descriptor = ego_core::event::skill::SkillDescriptorEvidence {
            id: ego_core::event::skill::SkillId::derive(
                ego_core::event::skill::SkillSourceAuthority::User,
                &location,
                "explain",
                &source_digest,
            ),
            name: "explain".to_owned(),
            description: "Explain a request".to_owned(),
            trigger: None,
            argument_hint: None,
            source: ego_core::event::skill::SkillSourceAuthority::User,
            location: location.clone(),
            source_digest,
            user_invocable: true,
            model_invocable: true,
        };
        let skill = ego_core::skill::SkillSpec::new(
            descriptor,
            b"Explain this request: $ARGUMENTS".to_vec(),
        )
        .expect("skill body")
        .with_hook_entries(vec![HookConfigEntry::new(
            HookLifecycle::UserPromptExpansion,
            None,
            HookHandlerConfig::Prompt {
                prompt: "continue this skill".to_owned(),
                model: None,
                timeout_seconds: None,
            },
        )]);
        let catalogue =
            Arc::new(ego_core::skill::SkillCatalogue::new(vec![skill]).expect("skill catalogue"));
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "skill-call".to_owned(),
                    name: "skill".to_owned(),
                    args: r#"{"kind":"invoke","name":"explain","arguments":"from model"}"#
                        .to_owned(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "skill complete".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "ordinary complete".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ]);
        let skill_evidence = SkillEvidence::new();
        let hook_invocations = RecordingHookInvoker::default();
        let base_hooks =
            HookCatalogue::admit_typed(&[], &[], Trust::Trusted).expect("empty hook catalogue");
        let session_id = SessionId::new();
        let journal = SharedJournal::new(Journal::new(
            session_id,
            &fx.store,
            &fx.clock,
            &fx.sink,
            Producer::Main,
            SessionState::default(),
        ));
        let hook_state = SessionHookState::new(
            HookLifecycleDispatcher::new(HookRunner::new(), base_hooks),
            journal.clone(),
            CancelToken::new(),
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        let runtime_observer = hook_state.clone();
        let hook_binding = Arc::new(RecordingHookBinding {
            invoker: hook_invocations.clone(),
            policies: Mutex::new(Vec::new()),
            mcp_bindings: Mutex::new(0),
            fail_at: None,
            timeline: Arc::new(Mutex::new(Vec::new())),
        });
        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::from_registry(
            &[RegisteredTool::skill(Arc::clone(&catalogue))],
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &skill_evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = CoreTurnLoop::from_shared_journal(
            session_id,
            journal,
            &NO_ADMISSION_EVIDENCE,
            CancelToken::new(),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        )
        .expect("synchronized budget gate")
        .with_skill_invocation_port(Arc::new(RecordingSkillInvocationPort::default()))
        .with_hook_binding(hook_state, hook_binding, None)
        .expect("hook runtime shares the turn journal");

        let skill_outcome = block_on(turn.run("model invokes explain"))
            .expect("model-invoked skill turn completes");
        assert_eq!(skill_outcome.exit_reason, ExitReason::Completed);
        let invocation = fx
            .store
            .events()
            .iter()
            .find_map(|event| match event.kind {
                EventKind::SkillInstructionAdmitted { invocation, .. } => Some(invocation),
                _ => None,
            })
            .expect("skill instruction admission is durable");
        assert_eq!(
            hook_invocations
                .lifecycles()
                .iter()
                .filter(|lifecycle| **lifecycle == HookLifecycle::UserPromptExpansion)
                .count(),
            1,
            "the invocation-scoped hook fires for the model skill turn"
        );
        assert!(
            !runtime_observer
                .catalogue_snapshot()
                .expect("hook catalogue snapshot")
                .specs()
                .iter()
                .any(|spec| spec.skill_invocation() == Some(invocation)),
            "completed skill turns remove invocation-scoped hooks"
        );

        let ordinary =
            block_on(turn.run("ordinary")).expect("the following ordinary turn completes");
        assert_eq!(ordinary.exit_reason, ExitReason::Completed);
        assert_eq!(
            hook_invocations
                .lifecycles()
                .iter()
                .filter(|lifecycle| **lifecycle == HookLifecycle::UserPromptExpansion)
                .count(),
            1,
            "the removed skill hook cannot fire on the next ordinary turn"
        );
    }

    #[test]
    fn typed_user_skill_turn_uses_admitted_instruction_not_prompt_or_tool_call() {
        let location =
            ego_core::event::skill::SkillSourceLocation::parse("skills/explain/SKILL.md")
                .expect("skill source location");
        let source_digest = ego_core::event::skill::SkillContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("skill source digest");
        let descriptor = ego_core::event::skill::SkillDescriptorEvidence {
            id: ego_core::event::skill::SkillId::derive(
                ego_core::event::skill::SkillSourceAuthority::User,
                &location,
                "explain",
                &source_digest,
            ),
            name: "explain".to_owned(),
            description: "Explain a request".to_owned(),
            trigger: None,
            argument_hint: None,
            source: ego_core::event::skill::SkillSourceAuthority::User,
            location,
            source_digest,
            user_invocable: true,
            model_invocable: false,
        };
        let catalogue = Arc::new(
            ego_core::skill::SkillCatalogue::new(vec![
                ego_core::skill::SkillSpec::new(
                    descriptor.clone(),
                    b"Explain this request: $ARGUMENTS".to_vec(),
                )
                .expect("admitted skill")
                .with_model(ModelRef("skill-model".to_owned()), "high"),
            ])
            .expect("admitted catalogue"),
        );
        let arguments = "preserve these raw arguments: {x: 1}";
        let operation = ego_core::skill::AdmittedSkillOperation::invoke(&descriptor, arguments);
        let fx = Fixture::new(vec![
            vec![
                AiEvent::TextDelta {
                    text: "done".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "ordinary done".to_owned(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ]);
        let skill_evidence = SkillEvidence::new();
        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::from_registry(
            &[RegisteredTool::skill(Arc::clone(&catalogue))],
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &skill_evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        )
        .with_skill_invocation_port(Arc::new(RecordingSkillInvocationPort::default()));

        let outcome =
            block_on(turn.run_admitted_skill(operation)).expect("the store accepted every append");
        let typed_turn = outcome.turn;
        assert_eq!(outcome.exit_reason, ExitReason::Completed);
        assert!(outcome.ok);

        let calls = fx.generator.calls();
        assert_eq!(calls.len(), 1, "a typed skill invokes one generation");
        let request = &calls[0];
        assert_eq!(request.model, "skill-model");
        assert_eq!(
            request
                .tools
                .iter()
                .filter(|tool| tool.name == "skill")
                .count(),
            1,
            "the mixed registry exposes the skill tool to the generation"
        );
        let instruction = request
            .messages
            .messages()
            .iter()
            .filter(|message| message.role == Role::System)
            .flat_map(|message| message.content.iter())
            .find_map(|block| match block {
                ContentBlock::Text { text } if text.starts_with("Admitted skill instruction (") => {
                    Some(text)
                }
                _ => None,
            })
            .expect("the request carries the admitted skill as a system instruction");
        assert!(
            instruction.ends_with("\nExplain this request: preserve these raw arguments: {x: 1}"),
            "the request contains the rendered skill body: {instruction}"
        );
        assert!(
            instruction.contains(arguments),
            "the exact raw arguments survive rendering"
        );

        let events = fx.store.events();
        let catalogue_at = events
            .iter()
            .position(|event| {
                event.env.turn == typed_turn
                    && matches!(&event.kind, EventKind::SkillCatalogueAdmitted { .. })
            })
            .expect("catalogue admission is durable");
        let instruction_at = events
            .iter()
            .position(|event| {
                event.env.turn == typed_turn
                    && matches!(&event.kind, EventKind::SkillInstructionAdmitted { .. })
            })
            .expect("instruction admission is durable");
        let generation_at = events
            .iter()
            .position(|event| {
                event.env.turn == typed_turn
                    && matches!(&event.kind, EventKind::GenerationStarted { .. })
            })
            .expect("generation start is durable");
        assert!(catalogue_at < instruction_at && instruction_at < generation_at);
        assert!(!events.iter().any(|event| {
            event.env.turn == typed_turn && matches!(&event.kind, EventKind::UserPrompt { .. })
        }));
        assert!(!events.iter().any(|event| matches!(
            &event.kind,
            EventKind::ToolCallStarted { .. } | EventKind::ToolCallCompleted { .. }
        ) && event.env.turn == typed_turn));

        let ordinary =
            block_on(turn.run("ordinary")).expect("the store accepted every ordinary append");
        assert_eq!(ordinary.exit_reason, ExitReason::Completed);
        let calls = fx.generator.calls();
        assert_eq!(
            calls.len(),
            2,
            "the ordinary turn invokes one more generation"
        );
        assert_eq!(
            calls[1].model, "test-model",
            "the skill model pin is cleared"
        );
        assert!(!calls[1].messages.messages().iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { text }
                        if text.starts_with("Admitted skill instruction (")
                )
            })
        }));

        let state = turn.into_state().expect("unique turn journal state");
        assert!(
            state.live_hooks().is_empty(),
            "completed typed skill turns leave no active skill hooks"
        );
        assert_eq!(state.skill_events().len(), 1);
    }

    #[test]
    fn stale_typed_user_skill_operation_is_refused_before_generation() {
        let location =
            ego_core::event::skill::SkillSourceLocation::parse("skills/explain/SKILL.md")
                .expect("skill source location");
        let old_digest = ego_core::event::skill::SkillContentDigest::parse(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("old skill source digest");
        let current_digest = ego_core::event::skill::SkillContentDigest::parse(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )
        .expect("current skill source digest");
        let old_descriptor = ego_core::event::skill::SkillDescriptorEvidence {
            id: ego_core::event::skill::SkillId::derive(
                ego_core::event::skill::SkillSourceAuthority::User,
                &location,
                "explain",
                &old_digest,
            ),
            name: "explain".to_owned(),
            description: "Explain a request".to_owned(),
            trigger: None,
            argument_hint: None,
            source: ego_core::event::skill::SkillSourceAuthority::User,
            location: location.clone(),
            source_digest: old_digest,
            user_invocable: true,
            model_invocable: false,
        };
        let current_descriptor = ego_core::event::skill::SkillDescriptorEvidence {
            id: ego_core::event::skill::SkillId::derive(
                ego_core::event::skill::SkillSourceAuthority::User,
                &location,
                "explain",
                &current_digest,
            ),
            name: "explain".to_owned(),
            description: "Explain a request".to_owned(),
            trigger: None,
            argument_hint: None,
            source: ego_core::event::skill::SkillSourceAuthority::User,
            location,
            source_digest: current_digest,
            user_invocable: true,
            model_invocable: false,
        };
        assert_ne!(old_descriptor.id, current_descriptor.id);
        let catalogue = Arc::new(
            ego_core::skill::SkillCatalogue::new(vec![
                ego_core::skill::SkillSpec::new(
                    current_descriptor,
                    b"Explain this request: $ARGUMENTS".to_vec(),
                )
                .expect("admitted current skill"),
            ])
            .expect("admitted current catalogue"),
        );
        let operation =
            ego_core::skill::AdmittedSkillOperation::invoke(&old_descriptor, "stale arguments");
        let fx = Fixture::new(vec![]);
        let skill_evidence = SkillEvidence::new();
        let mut gate = fx.gate.borrow_mut();
        let tools = Tools::from_registry(
            &[RegisteredTool::skill(Arc::clone(&catalogue))],
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &skill_evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            model_for(&fx, &fx.generator, &mut gate),
            tools,
        );

        let _ = block_on(turn.run_admitted_skill(operation));

        assert!(
            fx.generator.calls().is_empty(),
            "a stale operation never contacts the generator"
        );
        assert!(
            !fx.store
                .events()
                .iter()
                .any(|event| matches!(&event.kind, EventKind::SkillInstructionAdmitted { .. }))
        );
    }

    struct ScriptedPolicySource {
        captures: Mutex<VecDeque<Result<GenerationPolicyCapture, String>>>,
        timeline: Arc<Mutex<Vec<String>>>,
        active: Arc<PolicyEngine>,
    }

    impl GenerationPolicySource for ScriptedPolicySource {
        fn current(&self) -> Result<Arc<PolicyEngine>, String> {
            self.timeline.lock().unwrap().push("current".into());
            Ok(Arc::clone(&self.active))
        }

        fn capture(&self) -> Result<GenerationPolicyCapture, String> {
            let next = self
                .captures
                .lock()
                .unwrap()
                .pop_front()
                .expect("one policy capture per generation");
            self.timeline.lock().unwrap().push("capture".into());
            next
        }

        fn restore(
            &self,
            snapshot: Arc<GenerationPolicySnapshot>,
        ) -> Result<GenerationPolicyCapture, String> {
            GenerationPolicyCapture::restore(snapshot, None).map_err(|error| error.to_string())
        }
    }

    fn captured_policy(mode: Mode, rules: Ruleset) -> GenerationPolicyCapture {
        GenerationPolicyCapture::new(CanonPath::new("/ws"), mode, rules, None)
            .expect("valid generation policy capture")
    }

    struct RecordingHookBinding {
        invoker: RecordingHookInvoker,
        policies: Mutex<Vec<Arc<PolicyEngine>>>,
        mcp_bindings: Mutex<usize>,
        fail_at: Option<usize>,
        timeline: Arc<Mutex<Vec<String>>>,
    }

    impl HookInvocationBinding for RecordingHookBinding {
        fn bind<'a>(
            &self,
            policy: Arc<PolicyEngine>,
            _mcp: Option<&dyn McpRuntimeBinding>,
        ) -> Result<Arc<dyn HookInvocationPort + 'a>, String>
        where
            Self: 'a,
        {
            let mut policies = self.policies.lock().unwrap();
            policies.push(Arc::clone(&policy));
            if _mcp.is_some() {
                *self.mcp_bindings.lock().unwrap() += 1;
            }
            self.timeline.lock().unwrap().push("bind".into());
            if self.fail_at == Some(policies.len()) {
                return Err("hook policy binding failed".into());
            }
            Ok(Arc::new(self.invoker.clone()))
        }
    }

    struct TimelineStore<'a> {
        inner: &'a MemStore,
        timeline: Arc<Mutex<Vec<String>>>,
    }

    impl SessionStore for TimelineStore<'_> {
        fn append(&self, id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
            self.inner.append(id, event)?;
            match &event.kind {
                EventKind::PermissionDecided { .. } => self
                    .timeline
                    .lock()
                    .unwrap()
                    .push("permission_settled".into()),
                EventKind::ToolCallCompleted { .. } => {
                    self.timeline.lock().unwrap().push("tool_completed".into())
                }
                _ => {}
            }
            Ok(())
        }

        fn compare_and_append_run(
            &self,
            session: &SessionId,
            expected_revision: Option<RunRevision>,
            transaction: RunTransaction,
        ) -> Result<RunAppend, StoreError> {
            self.inner
                .compare_and_append_run(session, expected_revision, transaction)
        }

        fn read(
            &self,
            id: &SessionId,
        ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
            self.inner.read(id)
        }

        fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
            self.inner.list()
        }
    }

    #[test]
    fn policy_capture_is_once_per_generation_and_binds_each_tool_batch() {
        let fx = Fixture::new(vec![
            vec![
                AiEvent::ToolCall {
                    call_id: "first".into(),
                    name: "read".into(),
                    args: r#"{"path":"/ws/notes.md"}"#.into(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::ToolCall {
                    call_id: "second".into(),
                    name: "read".into(),
                    args: r#"{"path":"/ws/notes.md"}"#.into(),
                },
                AiEvent::Stop(StopReason::ToolUse),
            ],
            vec![
                AiEvent::TextDelta {
                    text: "done".into(),
                },
                AiEvent::Stop(StopReason::Stop),
            ],
        ])
        .with_read_tool();
        fx.fs.file("/ws/notes.md", "notes");
        let trace = generation_boundary::GenerationTrace::default();
        let timeline = Arc::clone(&trace.timeline);
        let policy_source = Arc::new(ScriptedPolicySource {
            captures: Mutex::new(VecDeque::from([
                Ok(captured_policy(
                    Mode::Default,
                    Ruleset::from_layer(
                        PermCfg {
                            judge: None,
                            allow: vec![Rule::load("read(/ws/notes.md)").unwrap()],
                            deny: vec![],
                        },
                        Source::Project,
                    ),
                )),
                Ok(captured_policy(Mode::Default, Ruleset::default())),
                Ok(captured_policy(Mode::Default, Ruleset::default())),
            ])),
            timeline: Arc::clone(&timeline),
            active: Arc::new(PolicyEngine::new(
                CanonPath::new("/ws"),
                Mode::Default,
                Ruleset::default(),
            )),
        });
        let source = Arc::new(
            generation_boundary::ScriptedGenerationSource::new([
                Ok(generation_boundary::profile(
                    "openai/one",
                    false,
                    vec![
                        AiEvent::ToolCall {
                            call_id: "first".into(),
                            name: "read".into(),
                            args: r#"{"path":"/ws/notes.md"}"#.into(),
                        },
                        AiEvent::Stop(StopReason::ToolUse),
                    ],
                    &trace,
                    Some(2048),
                )),
                Ok(generation_boundary::profile(
                    "openai/two",
                    false,
                    vec![
                        AiEvent::ToolCall {
                            call_id: "second".into(),
                            name: "read".into(),
                            args: r#"{"path":"/ws/missing.md"}"#.into(),
                        },
                        AiEvent::Stop(StopReason::ToolUse),
                    ],
                    &trace,
                    Some(2048),
                )),
                Ok(generation_boundary::profile(
                    "openai/three",
                    false,
                    vec![
                        AiEvent::TextDelta {
                            text: "done".into(),
                        },
                        AiEvent::Stop(StopReason::Stop),
                    ],
                    &trace,
                    Some(2048),
                )),
            ])
            .with_trace(trace.clone()),
        );
        let mut gate = fx.gate.borrow_mut();
        let mut runtime = model_for(&fx, &fx.generator, &mut gate);
        runtime.source = source;
        let timeline_store = TimelineStore {
            inner: &fx.store,
            timeline: Arc::clone(&timeline),
        };
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &timeline_store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            runtime,
            tools,
        )
        .with_policy_source(policy_source);
        block_on(turn.run("two generations")).expect("turn completes");
        assert_eq!(
            timeline.lock().unwrap().as_slice(),
            [
                "capture",
                "model_resolve",
                "provider",
                "permission_settled",
                "tool_completed",
                "capture",
                "model_resolve",
                "provider",
                "permission_settled",
                "tool_completed",
                "capture",
                "model_resolve",
                "provider",
            ]
        );
        let completed: Vec<bool> = fx
            .store
            .events()
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::ToolCallCompleted { ok, .. } => Some(ok),
                _ => None,
            })
            .collect();
        assert_eq!(completed, [true, false]);
    }

    #[test]
    fn policy_capture_failure_has_no_generation_provider_or_effect() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        let trace = generation_boundary::GenerationTrace::default();
        let source = Arc::new(generation_boundary::ScriptedGenerationSource::new([Ok(
            generation_boundary::profile("openai/never", false, vec![], &trace, Some(2048)),
        )]));
        let timeline = Arc::clone(&trace.timeline);
        let policy_source = Arc::new(ScriptedPolicySource {
            captures: Mutex::new(VecDeque::from([Err("poisoned policy".into())])),
            timeline: Arc::clone(&timeline),
            active: Arc::new(PolicyEngine::new(
                CanonPath::new("/ws"),
                Mode::Default,
                Ruleset::default(),
            )),
        });
        let mut gate = fx.gate.borrow_mut();
        let mut runtime = model_for(&fx, &fx.generator, &mut gate);
        runtime.source = source;
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            runtime,
            tools,
        )
        .with_policy_source(policy_source);
        let outcome = block_on(turn.run("capture fails")).expect("capture refusal is durable");
        assert_eq!(outcome.exit_reason, ExitReason::Internal);
        assert_eq!(timeline.lock().unwrap().as_slice(), ["capture"]);
        assert!(
            fx.store
                .kinds()
                .iter()
                .all(|kind| kind != "generation_started")
        );
        assert!(trace.requests.lock().unwrap().is_empty());
        assert!(
            fx.store
                .kinds()
                .iter()
                .all(|kind| kind != "tool_call_started")
        );
    }

    #[test]
    fn policy_capture_binds_current_prompt_hook_before_generation_capture() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        let trace = generation_boundary::GenerationTrace::default();
        let active = Arc::new(PolicyEngine::new(
            CanonPath::new("/ws"),
            Mode::Auto,
            Ruleset::default(),
        ));
        let captured = captured_policy(Mode::Default, Ruleset::default());
        let captured_policy = captured.policy();
        let policy_source = Arc::new(ScriptedPolicySource {
            captures: Mutex::new(VecDeque::from([Ok(captured)])),
            timeline: Arc::clone(&trace.timeline),
            active: Arc::clone(&active),
        });
        let invoker = Arc::new(RecordingHookInvoker {
            lifecycles: Arc::new(Mutex::new(Vec::new())),
            timeline: Some(Arc::clone(&trace.timeline)),
            block_prompt: false,
        });
        let binding = Arc::new(RecordingHookBinding {
            invoker: (*invoker).clone(),
            policies: Mutex::new(Vec::new()),
            mcp_bindings: Mutex::new(0),
            fail_at: None,
            timeline: Arc::clone(&trace.timeline),
        });
        let mut gate = fx.gate.borrow_mut();
        let mut runtime = model_for(&fx, &fx.generator, &mut gate);
        runtime.policy_source = policy_source.clone();
        runtime.source = Arc::new(
            generation_boundary::ScriptedGenerationSource::new([Ok(generation_boundary::profile(
                "openai/hook-binding",
                false,
                vec![AiEvent::Stop(StopReason::Stop)],
                &trace,
                Some(2048),
            ))])
            .with_trace(trace.clone()),
        );
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            runtime,
            tools,
        );
        let hook_catalogue = HookCatalogue::admit_typed(
            &[ConfigLayer::parse(
                Source::User,
                "[[hooks]]\nevent = \"UserPromptSubmit\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n",
            )
            .expect("one prompt hook")],
            &[],
            Trust::Trusted,
        )
        .expect("hook catalogue");
        let state = SessionHookState::new(
            HookLifecycleDispatcher::new(HookRunner::new(), hook_catalogue),
            turn.journal(),
            CancelToken::new(),
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        let mcp = Arc::new(RecordingMcpRuntime::default());
        turn = turn
            .with_hook_binding(state, binding.clone(), Some(mcp.clone()))
            .expect("hook binding attaches to turn");
        let outcome = block_on(turn.run("hook binding")).expect("turn completes");
        assert_eq!(outcome.exit_reason, ExitReason::Completed);
        let policies = binding.policies.lock().unwrap();
        assert_eq!(policies.len(), 2);
        assert!(Arc::ptr_eq(&policies[0], &active));
        assert!(Arc::ptr_eq(&policies[1], &captured_policy));
        assert_eq!(*binding.mcp_bindings.lock().unwrap(), 2);
        assert_eq!(
            mcp.floors.lock().unwrap().as_slice(),
            [active.resume_floor(), captured_policy.resume_floor()]
        );
        assert_eq!(invoker.lifecycles(), vec![HookLifecycle::UserPromptSubmit]);
        assert_eq!(
            trace.timeline.lock().unwrap().as_slice(),
            [
                "current",
                "bind",
                "prompt_hook",
                "capture",
                "bind",
                "model_resolve",
                "provider"
            ]
        );
    }

    #[test]
    fn mcp_runtime_binding_exposes_explicit_pending_policy_recovery_phases() {
        let snapshot = captured_policy(Mode::Default, Ruleset::default()).snapshot();
        let runtime = RecordingMcpRuntime {
            pending_policy: Mutex::new(Some(Arc::clone(&snapshot))),
            ..RecordingMcpRuntime::default()
        };
        let binding: &dyn McpRuntimeBinding = &runtime;

        let loaded = binding
            .load_pending_foreign_policy("continuation")
            .expect("pending policy loads");
        assert!(Arc::ptr_eq(&loaded, &snapshot));

        binding
            .bind_pending_foreign_continuation("continuation", Arc::clone(&loaded))
            .expect("pending continuation binds");
        let bound = runtime.bound_policies.lock().unwrap();
        assert_eq!(bound.len(), 1);
        assert!(Arc::ptr_eq(&bound[0], &snapshot));
    }

    #[test]
    fn construction_policy_binding_uses_supplied_policy_without_reading_current() {
        let fx = Fixture::new(vec![]);
        let trace = generation_boundary::GenerationTrace::default();
        let active = Arc::new(PolicyEngine::new(
            CanonPath::new("/ws"),
            Mode::Auto,
            Ruleset::default(),
        ));
        let supplied = captured_policy(Mode::Default, Ruleset::default()).policy();
        let policy_source = Arc::new(ScriptedPolicySource {
            captures: Mutex::new(VecDeque::new()),
            timeline: Arc::clone(&trace.timeline),
            active,
        });
        let binding = Arc::new(RecordingHookBinding {
            invoker: RecordingHookInvoker::default(),
            policies: Mutex::new(Vec::new()),
            mcp_bindings: Mutex::new(0),
            fail_at: None,
            timeline: Arc::clone(&trace.timeline),
        });
        let mut gate = fx.gate.borrow_mut();
        let mut runtime = model_for(&fx, &fx.generator, &mut gate);
        runtime.policy_source = policy_source;
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            runtime,
            tools,
        );
        let hook_catalogue =
            HookCatalogue::admit_typed(&[], &[], Trust::Trusted).expect("empty hook catalogue");
        let state = SessionHookState::new(
            HookLifecycleDispatcher::new(HookRunner::new(), hook_catalogue),
            turn.journal(),
            CancelToken::new(),
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        let mcp = Arc::new(RecordingMcpRuntime::default());

        let _turn = turn
            .with_hook_binding(state, binding.clone(), Some(mcp.clone()))
            .expect("hook binding attaches to turn")
            .with_hook_policy(Arc::clone(&supplied))
            .expect("supplied policy binds");

        let policies = binding.policies.lock().unwrap();
        assert_eq!(policies.len(), 1);
        assert!(Arc::ptr_eq(&policies[0], &supplied));
        assert_eq!(*binding.mcp_bindings.lock().unwrap(), 1);
        assert_eq!(
            mcp.floors.lock().unwrap().as_slice(),
            [supplied.resume_floor()]
        );
        assert_eq!(trace.timeline.lock().unwrap().as_slice(), ["bind"]);
    }

    #[test]
    fn blocking_prompt_hook_does_not_capture_generation_policy() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        let trace = generation_boundary::GenerationTrace::default();
        let active = Arc::new(PolicyEngine::new(
            CanonPath::new("/ws"),
            Mode::Auto,
            Ruleset::default(),
        ));
        let policy_source = Arc::new(ScriptedPolicySource {
            captures: Mutex::new(VecDeque::new()),
            timeline: Arc::clone(&trace.timeline),
            active,
        });
        let binding = Arc::new(RecordingHookBinding {
            invoker: RecordingHookInvoker {
                lifecycles: Arc::new(Mutex::new(Vec::new())),
                timeline: Some(Arc::clone(&trace.timeline)),
                block_prompt: true,
            },
            policies: Mutex::new(Vec::new()),
            mcp_bindings: Mutex::new(0),
            fail_at: None,
            timeline: Arc::clone(&trace.timeline),
        });
        let mut gate = fx.gate.borrow_mut();
        let mut runtime = model_for(&fx, &fx.generator, &mut gate);
        runtime.policy_source = policy_source;
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            runtime,
            tools,
        );
        let hook_catalogue = HookCatalogue::admit_typed(
            &[ConfigLayer::parse(
                Source::User,
                "[[hooks]]\nevent = \"UserPromptSubmit\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n",
            )
            .expect("one prompt hook")],
            &[],
            Trust::Trusted,
        )
        .expect("hook catalogue");
        let state = SessionHookState::new(
            HookLifecycleDispatcher::new(HookRunner::new(), hook_catalogue),
            turn.journal(),
            CancelToken::new(),
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        turn = turn
            .with_hook_binding(state, binding, None)
            .expect("hook binding attaches to turn");
        let _outcome = block_on(turn.run("blocked prompt")).expect("prompt block is durable");
        assert_eq!(
            trace.timeline.lock().unwrap().as_slice(),
            ["current", "bind", "prompt_hook"]
        );
        assert!(
            fx.store
                .kinds()
                .iter()
                .all(|kind| kind != "generation_started")
        );
        assert!(
            fx.store
                .kinds()
                .iter()
                .all(|kind| kind != "tool_call_started")
        );
    }

    #[test]
    fn policy_hook_binding_failure_refuses_before_model_resolution_generation_or_effect() {
        let fx = Fixture::new(vec![vec![AiEvent::Stop(StopReason::Stop)]]);
        let trace = generation_boundary::GenerationTrace::default();
        let active = Arc::new(PolicyEngine::new(
            CanonPath::new("/ws"),
            Mode::Auto,
            Ruleset::default(),
        ));
        let captured = captured_policy(Mode::Default, Ruleset::default());
        let policy_source = Arc::new(ScriptedPolicySource {
            captures: Mutex::new(VecDeque::from([Ok(captured)])),
            timeline: Arc::clone(&trace.timeline),
            active,
        });
        let binding = Arc::new(RecordingHookBinding {
            invoker: RecordingHookInvoker {
                lifecycles: Arc::new(Mutex::new(Vec::new())),
                timeline: Some(Arc::clone(&trace.timeline)),
                block_prompt: false,
            },
            policies: Mutex::new(Vec::new()),
            mcp_bindings: Mutex::new(0),
            fail_at: Some(2),
            timeline: Arc::clone(&trace.timeline),
        });
        let mut gate = fx.gate.borrow_mut();
        let mut runtime = model_for(&fx, &fx.generator, &mut gate);
        runtime.policy_source = policy_source;
        runtime.source = Arc::new(
            generation_boundary::ScriptedGenerationSource::new([Ok(generation_boundary::profile(
                "openai/hook-binding-failure",
                false,
                vec![AiEvent::Stop(StopReason::Stop)],
                &trace,
                Some(2048),
            ))])
            .with_trace(trace.clone()),
        );
        let tools = Tools::new(
            &fx.registry,
            ToolEffectPorts::new(
                &fx.fs,
                fx.process.as_ref(),
                &fx.evidence,
                Arc::new(NoResolver),
                Arc::new(NoHop),
                Arc::new(NoDuplex),
            ),
            CanonPath::new("/ws"),
        );
        let mut turn = TurnLoop::open(
            session(
                SessionState::default(),
                &fx.store,
                &fx.clock,
                &fx.sink,
                CancelToken::new(),
            ),
            runtime,
            tools,
        );
        let hook_catalogue = HookCatalogue::admit_typed(
            &[ConfigLayer::parse(
                Source::User,
                "[[hooks]]\nevent = \"UserPromptSubmit\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"continue\"\n",
            )
            .expect("one prompt hook")],
            &[],
            Trust::Trusted,
        )
        .expect("hook catalogue");
        let state = SessionHookState::new(
            HookLifecycleDispatcher::new(HookRunner::new(), hook_catalogue),
            turn.journal(),
            CancelToken::new(),
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        turn = turn
            .with_hook_binding(state, binding.clone(), None)
            .expect("hook binding attaches to turn");
        let outcome = block_on(turn.run("hook binding")).expect("binding refusal is durable");
        assert_eq!(outcome.exit_reason, ExitReason::Internal);
        assert_eq!(
            trace.timeline.lock().unwrap().as_slice(),
            ["current", "bind", "prompt_hook", "capture", "bind"]
        );
        assert!(trace.requests.lock().unwrap().is_empty());
        assert!(
            fx.store
                .kinds()
                .iter()
                .all(|kind| kind != "generation_started")
        );
        assert!(
            fx.store
                .kinds()
                .iter()
                .all(|kind| kind != "tool_call_started")
        );
    }
    mod task_tool {
        use super::*;

        const TASK_SECRET: &str = "regression_case-unique";

        fn call(id: &str, name: &str, args: serde_json::Value) -> AiEvent {
            AiEvent::ToolCall {
                call_id: id.into(),
                name: name.into(),
                args: args.to_string(),
            }
        }

        fn run_tasks<G: Generator + 'static>(
            fx: &Fixture,
            generator: &Arc<G>,
            cancel: CancelToken,
            ceiling: Option<u64>,
        ) -> (ExitReason, SessionState) {
            let mut registry: Vec<_> = fx
                .registry
                .iter()
                .cloned()
                .map(RegisteredTool::Effect)
                .collect();
            registry.push(RegisteredTool::Task);
            let tools = Tools::from_registry(
                &registry,
                ToolEffectPorts::new(
                    &fx.fs,
                    fx.process.as_ref(),
                    &fx.evidence,
                    Arc::new(NoResolver),
                    Arc::new(NoHop),
                    Arc::new(NoDuplex),
                )
                .with_cancel(cancel.clone())
                .with_secret_values(ego_core::secrets::SecretValues::new([TASK_SECRET
                    .as_bytes()
                    .to_vec()])),
                CanonPath::new("/ws"),
            );
            let mut settings = config();
            settings.parallel_tool_calls = true;
            let mut runtime = model_with(
                generator,
                &mut fx.gate.borrow_mut(),
                metering(),
                Some(rates()),
                ceiling,
                settings,
            );
            runtime.policy_source = fx.policy_source.clone();
            runtime.config.identity = Some(ego_core::turn::AgentIdentity {
                version: "test".into(),
                sandbox: Confinement::Workspace,
                offline: true,
                host: None,
            });
            let mut loop_ = CoreTurnLoop::new(
                session(
                    SessionState::default(),
                    &fx.store,
                    &fx.clock,
                    &fx.sink,
                    cancel,
                ),
                runtime,
                tools,
            )
            .unwrap();
            let outcome = block_on(loop_.run("parent")).unwrap();
            (outcome.exit_reason, loop_.into_state().unwrap())
        }

        #[test]
        fn task_child_cannot_escape_workspace_mode_or_recursively_delegate() {
            // Catches: a child widening the parent workspace/mode or retaining task.
            let fx = Fixture::new(vec![]).with_write_tool();
            fx.fs.file("/outside/stolen", "original");
            let generator = Arc::new(FakeGenerator::new([
                vec![
                    call("task1", "task", json!({"name":"review", "prompt":"child"})),
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![
                    call(
                        "escape",
                        "fake_write",
                        json!({"path":"/outside/stolen", "contents":"bad"}),
                    ),
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![AiEvent::Stop(StopReason::Stop)],
            ]));
            let (exit, state) = run_tasks(&fx, &generator, CancelToken::new(), None);
            assert_eq!(exit, ExitReason::Completed);
            assert!(generator.calls()[1].tools.iter().all(|t| t.name != "task"));
            let task = state
                .items
                .values()
                .find(|i| i.tool_name.as_deref() == Some("task"))
                .unwrap();
            assert_eq!(task.ok, Some(false));
            assert!(
                state
                    .items
                    .values()
                    .all(|i| i.tool_name.as_deref() != Some("fake_write")),
                "child transcript leaked into parent"
            );
        }

        struct NetworkAttempt;
        impl TypedTool for NetworkAttempt {
            type Params = ReadParams;
            fn name(&self) -> &str {
                "network_attempt"
            }
            fn description(&self) -> &str {
                "attempt a network effect"
            }
            fn prepare_typed(
                &self,
                params: ReadParams,
                _: &PrepareCtx<'_>,
            ) -> Result<PreparedCall, ToolError> {
                Ok(PreparedCall::new(
                    self.name(),
                    Op::ProtocolConnect {
                        target: ego_core::permit::NetworkTarget::parse(&params.path)
                            .map_err(|error| ToolError::InvalidParams(error.to_string()))?,
                    },
                ))
            }
            fn execute<'a, 'io>(
                &'a self,
                _: PreparedCall,
                _: &'a dyn GuardedIo<'io>,
            ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
            where
                'io: 'a,
            {
                panic!("offline parent's denied network effect reached execution")
            }
        }

        #[test]
        fn task_child_keeps_offline_posture_and_cannot_drop_parent_network_denial() {
            // Catches: offline child advertising online posture or losing the parent network ceiling.
            let mut fx = Fixture::new(vec![]).with_rules(&[], &["net(*)"]);
            fx.registry.push(Arc::new(Typed(NetworkAttempt)));
            let generator = Arc::new(FakeGenerator::new([
                vec![
                    call(
                        "task1",
                        "task",
                        json!({"name":"offline", "prompt":"try network"}),
                    ),
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![
                    call(
                        "net1",
                        "network_attempt",
                        json!({"path":"api.example.test:443"}),
                    ),
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![AiEvent::Stop(StopReason::Stop)],
            ]));
            let (_, state) = run_tasks(&fx, &generator, CancelToken::new(), None);
            let requests = generator.calls();
            for request in &requests[..2] {
                assert!(request.messages.messages().iter().flat_map(|message| &message.content).any(|block| {
                    matches!(block, ContentBlock::Text { text } if text.contains("sandbox: workspace; network: offline"))
                }), "parent and child must both receive the authoritative offline context");
            }
            assert_eq!(
                state
                    .items
                    .values()
                    .find(|item| item.tool_name.as_deref() == Some("task"))
                    .unwrap()
                    .ok,
                Some(false)
            );
        }

        #[derive(Debug, Default)]
        struct NamedHuman(Mutex<Vec<PermissionRequest>>);
        impl HumanApprover for NamedHuman {
            fn decide<'a>(
                &'a self,
                _: ItemId,
                request: &'a PermissionRequest,
            ) -> BoxFuture<'a, PermissionDecision> {
                self.0.lock().unwrap().push(request.clone());
                Box::pin(futures::future::ready(PermissionDecision::Deny))
            }
        }

        #[test]
        fn task_child_permission_reaches_parent_human_with_name_without_autoapproval() {
            // Catches: a dropped, unnamed or automatically approved child permission.
            let mut fx = Fixture::new(vec![]).with_write_tool();
            let human = Arc::new(NamedHuman::default());
            fx.policy_source = Arc::new(FixedPolicySource {
                mode: Mode::Default,
                permissions: Ruleset::default(),
                human: Some(human.clone()),
            });
            let generator = Arc::new(FakeGenerator::new([
                vec![
                    call("task1", "task", json!({"name":"review", "prompt":"child"})),
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![
                    call(
                        "write1",
                        "fake_write",
                        json!({"path":"/ws/note.txt", "contents":"bad"}),
                    ),
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![AiEvent::Stop(StopReason::Stop)],
            ]));
            let (_, state) = run_tasks(&fx, &generator, CancelToken::new(), None);
            let requests = human.0.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].child_name(), Some("review"));
            assert_eq!(requests[0].tool(), "fake_write");
            assert_eq!(
                state
                    .items
                    .values()
                    .find(|i| i.tool_name.as_deref() == Some("task"))
                    .unwrap()
                    .ok,
                Some(false)
            );
        }

        #[test]
        fn task_books_child_usage_once_and_returns_only_masked_bounded_final_text() {
            // Catches: unmetered/double-booked spend, child transcript leakage, and unmasked task results.
            let fx = Fixture::new(vec![]);
            let child_text = format!("{TASK_SECRET}{}", "é".repeat(40_000));
            let generator = Arc::new(FakeGenerator::new([
                vec![
                    call("task1", "task", json!({"name":"review", "prompt":"child"})),
                    AiEvent::Usage(tokens(1, 0, 0, 1)),
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![
                    AiEvent::TextDelta { text: child_text },
                    AiEvent::Usage(tokens(3, 0, 0, 2)),
                    AiEvent::Stop(StopReason::Stop),
                ],
                vec![
                    AiEvent::Usage(tokens(0, 0, 0, 0)),
                    AiEvent::Stop(StopReason::Stop),
                ],
            ]));
            let (_, state) = run_tasks(&fx, &generator, CancelToken::new(), None);
            assert_eq!(state.usage.uncached_input_tokens, 4);
            assert_eq!(state.usage.output_tokens, 3);
            assert_eq!(state.cost.known(), Some(&usd("0.000057")));
            let task = state
                .items
                .values()
                .find(|i| i.tool_name.as_deref() == Some("task"))
                .unwrap();
            let result = task.tool_result.as_ref().unwrap().model_text();
            assert!(!result.contains(TASK_SECRET));
            assert!(result.contains("[REDACTED]"));
            let parent_request = serde_json::to_string(generator.calls().last().unwrap()).unwrap();
            assert!(!parent_request.contains(TASK_SECRET));
            assert!(parent_request.contains("[REDACTED]"));
            assert!(result.len() <= 65_536);
            assert!(
                task.tool_result
                    .as_ref()
                    .unwrap()
                    .model_text()
                    .contains("truncated")
            );
            assert_eq!(
                state
                    .items
                    .values()
                    .filter(|i| i.kind == ItemKind::Assistant)
                    .count(),
                0
            );
            assert_eq!(
                fx.store
                    .events()
                    .iter()
                    .filter(|ev| matches!(
                        ev.kind,
                        EventKind::GenerationStarted {
                            role: GenerationRole::Subagent,
                            ..
                        }
                    ))
                    .count(),
                1
            );
            assert!(state.refusals.is_empty());
        }

        #[test]
        fn task_cumulative_token_ceiling_includes_parent_spend() {
            // Catches: a child resetting the cumulative token allowance.
            let fx = Fixture::new(vec![]);
            let generator = Arc::new(FakeGenerator::new([
                vec![
                    call("task1", "task", json!({"name":"review", "prompt":"child"})),
                    AiEvent::Usage(tokens(10, 0, 0, 0)),
                    AiEvent::Stop(StopReason::ToolUse),
                ],
                vec![AiEvent::Stop(StopReason::Stop)],
            ]));
            let (exit, state) = run_tasks(&fx, &generator, CancelToken::new(), Some(10));
            assert_eq!(exit, ExitReason::Budget);
            assert_eq!(
                generator.calls().len(),
                1,
                "child spent beyond the parent token ceiling"
            );
            assert_eq!(
                state
                    .items
                    .values()
                    .find(|i| i.tool_name.as_deref() == Some("task"))
                    .unwrap()
                    .ok,
                Some(false)
            );
        }
        struct ConcurrentCancelGenerator {
            calls: AtomicUsize,
            cancel: CancelToken,
        }
        impl Generator for ConcurrentCancelGenerator {
            fn ensure_expressible(
                &self,
                _: &GenRequest,
            ) -> Result<(), ego_core::generator::port::RequestExpressibilityError> {
                Ok(())
            }
            fn generate(&self, _: GenRequest, attempt: GenerationAttempt) -> AttemptStream {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    return AttemptStream::new(
                        attempt,
                        Box::pin(futures::stream::iter(vec![
                            call("one", "task", json!({"name":"one","prompt":"first"})),
                            call("two", "task", json!({"name":"two","prompt":"second"})),
                            AiEvent::Stop(StopReason::ToolUse),
                        ])),
                    );
                }
                if n == 2 {
                    self.cancel.cancel(CancelCause::User);
                }
                AttemptStream::new(attempt, Box::pin(futures::stream::pending()))
            }
        }

        #[test]
        fn task_batch_starts_both_children_and_cancellation_settles_every_attempt() {
            // Catches: sequential children deadlocking, orphan streams or unpaid cancellation.
            let fx = Fixture::new(vec![]);
            let cancel = CancelToken::new();
            let generator = Arc::new(ConcurrentCancelGenerator {
                calls: AtomicUsize::new(0),
                cancel: cancel.clone(),
            });
            let watchdog_cancel = cancel.clone();
            let (stop, watchdog_rx) = std::sync::mpsc::channel();
            let watchdog = std::thread::spawn(move || {
                if watchdog_rx
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .is_err()
                {
                    watchdog_cancel.cancel(CancelCause::User);
                }
            });
            let (exit, state) = run_tasks(&fx, &generator, cancel, None);
            let _ = stop.send(());
            watchdog.join().unwrap();
            assert_eq!(
                generator.calls.load(Ordering::SeqCst),
                3,
                "both children must reach generation before cancellation"
            );
            assert_eq!(exit, ExitReason::Cancelled(CancelCause::User));
            assert!(state.live_attempts.is_empty());
            let tasks: Vec<_> = state
                .items
                .values()
                .filter(|i| i.tool_name.as_deref() == Some("task"))
                .collect();
            assert_eq!(tasks.len(), 2);
            assert!(tasks.iter().all(|task| {
                task.ok == Some(false)
                    && task
                        .tool_result
                        .as_ref()
                        .unwrap()
                        .model_text()
                        .contains("Cancelled")
            }));
            assert!(state.refusals.is_empty());
        }
    }
}
