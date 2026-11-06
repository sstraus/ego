//! contracts for the borrow-scoped shared session hook runtime.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use ego_core::config::{ConfigLayer, Source, Trust};
use ego_core::event::domain::{DomainEvent, EventKind};
use ego_core::event::hook::{
    HookActivation, HookCatalogueEvidence, HookDecision, HookLifecycle, HookOccurrenceKey,
    HookOccurrenceOwner, HookOutcome,
};
use ego_core::event::id::{Producer, Seq, SessionId, TurnId};
use ego_core::event::skill::{SkillInvocationId, SkillSourceAuthority};
use ego_core::event::time::{Clock, Timestamp};
use ego_core::hook::{
    HookCatalogue, HookCausality, HookConfigEntry, HookEvent, HookFuture, HookHandlerConfig,
    HookInvocationPort, HookInvocationRequest, HookLifecycleDispatcher, HookRunner, HookSource,
    SessionHookRuntime, SessionHookState,
};
use ego_core::mcp::{ForeignQuestionPort, McpRuntimeBinding, ToolExecution};
use ego_core::run::{EventSinkFactory, RunExecutionBindings};
use ego_core::sink::{EventSink, NullSink};
use ego_core::state::SessionState;
use ego_core::store::session_store::{SessionEntry, SessionMeta, SessionStore, StoreError};
use ego_core::suspension::ResumeFloor;
use ego_core::turn::{
    AdmittedToolSource, CancelToken, CapturedToolCatalogue, Journal, SharedJournal, ToolEffectPorts,
};
use futures::future::BoxFuture;
use futures::stream::BoxStream;

#[derive(Default)]
struct Store {
    events: Mutex<Vec<DomainEvent>>,
}

impl Store {
    fn events(&self) -> Vec<DomainEvent> {
        self.events.lock().expect("journal events").clone()
    }
}

impl SessionStore for Store {
    fn append(&self, _id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
        self.events
            .lock()
            .expect("journal events")
            .push(event.clone());
        Ok(())
    }

    fn read(
        &self,
        _id: &SessionId,
    ) -> Result<BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError> {
        let events = self
            .events
            .lock()
            .expect("journal events")
            .iter()
            .cloned()
            .map(|event| Ok(SessionEntry::Event(event)))
            .collect::<Vec<_>>();
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn compare_and_append_run(
        &self,
        _session: &SessionId,
        _expected_revision: Option<ego_core::run::RunRevision>,
        _transaction: ego_core::run::RunTransaction,
    ) -> Result<ego_core::run::RunAppend, StoreError> {
        panic!("run transactions are outside this hook fixture")
    }

    fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        panic!("session listing is outside this hook fixture")
    }
}

#[derive(Clone, Copy, Default)]
struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(79_000)
    }
}

#[derive(Default)]
struct RecordingPort {
    calls: Mutex<Vec<HookInvocationRequest>>,
    blocked: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
}

impl RecordingPort {
    fn calls(&self) -> usize {
        self.calls.lock().expect("hook calls").len()
    }

    fn blocked() -> (Self, futures::channel::oneshot::Sender<()>) {
        let (sender, receiver) = futures::channel::oneshot::channel();
        (
            Self {
                calls: Mutex::new(Vec::new()),
                blocked: Mutex::new(Some(receiver)),
            },
            sender,
        )
    }
}

impl HookInvocationPort for RecordingPort {
    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        self.calls.lock().expect("hook calls").push(request);
        let blocked = self.blocked.lock().expect("hook block").take();
        if let Some(blocked) = blocked {
            return Box::pin(async move {
                let _ = blocked.await;
                Ok(HookOutcome::new(HookDecision::Continue, None))
            });
        }
        Box::pin(std::future::ready(Ok(HookOutcome::new(
            HookDecision::Continue,
            None,
        ))))
    }
}

fn active_catalogue() -> HookCatalogue {
    let layer = ConfigLayer::parse(
        Source::User,
        "[[hooks]]\nevent = \"SessionStart\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"fixture\"\n[[hooks]]\nevent = \"InstructionsLoaded\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"fixture\"\n[[hooks]]\nevent = \"UserPromptExpansion\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"fixture\"\n[[hooks]]\nevent = \"SessionEnd\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"fixture\"\n",
    )
    .expect("hook layer");
    HookCatalogue::admit_typed(&[layer], &[], Trust::Trusted).expect("active catalogue")
}

fn admitted_catalogue() -> HookCatalogueEvidence {
    active_catalogue().evidence()
}

fn admit_catalogue(runtime: &SessionHookRuntime<'_>, turn: TurnId) {
    runtime
        .hook_state()
        .journal()
        .append(
            turn,
            None,
            None,
            EventKind::HookCatalogueAdmitted {
                descriptors: admitted_catalogue(),
            },
        )
        .expect("catalogue admission");
}

fn runtime_fixture(
    blocked: bool,
) -> (
    SessionHookRuntime<'static>,
    Arc<RecordingPort>,
    &'static Store,
    SessionId,
    TurnId,
    Option<futures::channel::oneshot::Sender<()>>,
) {
    let session = SessionId::new();
    let turn = TurnId::new();
    let store: &'static Store = Box::leak(Box::new(Store::default()));
    let clock: &'static FixedClock = Box::leak(Box::new(FixedClock));
    let sink: &'static NullSink = Box::leak(Box::new(NullSink));
    let journal = Journal::new(
        session,
        store,
        clock,
        sink,
        Producer::Main,
        SessionState::default(),
    );
    let shared = SharedJournal::new(journal);
    let (port, release) = if blocked {
        let (port, release) = RecordingPort::blocked();
        (port, Some(release))
    } else {
        (RecordingPort::default(), None)
    };
    let invocation = Arc::new(port);
    let dispatcher = HookLifecycleDispatcher::new(HookRunner::new(), active_catalogue());
    let state = SessionHookState::new(
        dispatcher,
        shared,
        CancelToken::new(),
        Instant::now() + Duration::from_secs(60),
    );
    let runtime = state.bind(invocation.clone());
    (runtime, invocation, store, session, turn, release)
}

fn occurrence(
    lifecycle: HookLifecycle,
    session: SessionId,
    turn: TurnId,
    _n: u128,
) -> ego_core::hook::HookOccurrence {
    ego_core::hook::HookOccurrence::new(
        HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Session { session }),
        HookEvent::session(lifecycle),
        ego_core::hook::HookCausality::session(session, turn),
    )
    .expect("matching occurrence")
}

#[test]
fn normal_append_and_two_hook_occurrences_share_one_sequence_and_state() {
    let (runtime, _invocation, store, session, turn, release) = runtime_fixture(false);
    let normal = runtime
        .hook_state()
        .journal()
        .append(
            turn,
            None,
            None,
            EventKind::AssistantMessage {
                text: "normal".into(),
            },
        )
        .expect("normal append");
    admit_catalogue(&runtime, turn);
    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::SessionStart, session, turn, 1),
    ))
    .expect("first hook");
    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::SessionEnd, session, turn, 2),
    ))
    .expect("second hook");
    if let Some(release) = release {
        let _ = release.send(());
    }

    let events = store.events();
    assert!(events.len() >= 3);
    assert_eq!(events[0].env.id, normal);
    let mut expected = Seq::ZERO;
    for event in &events {
        assert_eq!(event.env.seq, expected.next());
    }
    assert_eq!(
        runtime.hook_state().state().last_seq,
        events.last().expect("events").env.seq
    );
    let state = runtime.hook_state().state();
    assert_eq!(state.hook_occurrences().len(), 2);
    assert_eq!(
        state
            .hook_occurrences()
            .iter()
            .map(|occurrence| occurrence.starts().len())
            .sum::<usize>(),
        2,
        "the reached occurrences record their durable starts"
    );
    assert_eq!(state.hook_settlements().len(), 2);
}

#[test]
fn admitted_catalogue_is_published_before_instruction_lifecycle_hooks_are_routed() {
    let (runtime, invocation, store, session, turn, release) = runtime_fixture(false);
    admit_catalogue(&runtime, turn);
    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::InstructionsLoaded, session, turn, 7),
    ))
    .expect("instructions-loaded hook");
    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::UserPromptExpansion, session, turn, 8),
    ))
    .expect("prompt-expansion hook");
    if let Some(release) = release {
        let _ = release.send(());
    }

    let events = store.events();
    let admitted = events
        .iter()
        .position(|event| matches!(event.kind, EventKind::HookCatalogueAdmitted { .. }))
        .expect("catalogue admission evidence");
    let reached = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match &event.kind {
            EventKind::HookLifecycleReached { occurrence, .. }
                if matches!(
                    occurrence.lifecycle(),
                    HookLifecycle::InstructionsLoaded | HookLifecycle::UserPromptExpansion
                ) =>
            {
                Some(index)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reached.len(), 2);
    assert!(reached.iter().all(|index| admitted < *index));
    assert_eq!(
        invocation.calls(),
        2,
        "both lifecycle events route through the catalogue"
    );
}

#[test]
fn skill_catalogue_is_activation_qualified_and_turn_completion_restores_persistent_only() {
    let (runtime, _invocation, store, session, turn, _release) = runtime_fixture(false);
    let invocation = SkillInvocationId::new();
    let skill = HookSource::skill_entries_with_authority(
        "skills/resumed/SKILL.md",
        invocation,
        SkillSourceAuthority::User,
        [HookConfigEntry::new(
            HookLifecycle::InstructionsLoaded,
            None,
            HookHandlerConfig::Prompt {
                prompt: "resumed hook".to_owned(),
                model: None,
                timeout_seconds: Some(7),
            },
        )],
    );
    runtime
        .hook_state()
        .replace_skill_sources(turn, &[skill])
        .expect("publish complete active skill catalogue");
    assert!(
        runtime
            .hook_state()
            .catalogue_evidence()
            .as_slice()
            .iter()
            .any(|entry| { entry.activation() == HookActivation::Skill { invocation } })
    );

    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::InstructionsLoaded, session, turn, 9),
    ))
    .expect("active skill hook");
    let reached = store
        .events()
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::HookLifecycleReached { plan, .. }
                if plan
                    .as_slice()
                    .iter()
                    .any(|entry| entry.activation() == HookActivation::Skill { invocation }) =>
            {
                Some(plan.clone())
            }
            _ => None,
        })
        .expect("activation-qualified skill plan was routed");
    assert!(
        reached
            .as_slice()
            .iter()
            .any(|entry| { entry.activation() == HookActivation::Skill { invocation } })
    );

    runtime
        .hook_state()
        .journal()
        .append(
            turn,
            None,
            None,
            EventKind::TurnCompleted {
                ok: true,
                exit_reason: ego_core::event::domain::ExitReason::Completed,
            },
        )
        .expect("turn completion");
    runtime
        .hook_state()
        .replace_skill_sources(turn, &[])
        .expect("restore persistent-only catalogue after turn completion");
    assert!(
        runtime
            .hook_state()
            .catalogue_evidence()
            .as_slice()
            .iter()
            .all(|entry| !matches!(entry.activation(), HookActivation::Skill { .. }))
    );
}

#[test]
fn recovered_completed_occurrence_does_not_invoke_and_state_keeps_all_facts() {
    let (runtime, invocation, store, session, turn, release) = runtime_fixture(false);
    admit_catalogue(&runtime, turn);
    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::SessionStart, session, turn, 3),
    ))
    .expect("first dispatch");
    let first_events = store.events().len();
    let first_calls = invocation.calls();
    assert_eq!(first_calls, 1, "first occurrence invokes once");
    assert!(release.is_none());
    futures::executor::block_on(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::SessionStart, session, turn, 3),
    ))
    .expect("recovered dispatch");
    assert_eq!(
        invocation.calls(),
        first_calls,
        "recovery never invokes again"
    );
    assert!(
        store
            .events()
            .iter()
            .any(|event| matches!(event.kind, EventKind::HookLifecycleReached { .. }))
    );
    assert_eq!(
        store.events().len(),
        first_events,
        "recovery reuses durable envelope"
    );
}

#[test]
fn blocked_invocation_does_not_hold_journal_lock_across_await() {
    let (runtime, _invocation, _store, session, turn, release) = runtime_fixture(true);
    admit_catalogue(&runtime, turn);
    let mut blocked = Box::pin(runtime.dispatch(
        Producer::Main,
        occurrence(HookLifecycle::SessionStart, session, turn, 4),
    ));
    let waker = futures::task::noop_waker_ref();
    let mut context = Context::from_waker(waker);
    assert!(matches!(blocked.as_mut().poll(&mut context), Poll::Pending));
    let append = runtime.hook_state().journal().append(
        turn,
        None,
        None,
        EventKind::AssistantMessage {
            text: "independent".into(),
        },
    );
    assert!(
        append.is_ok(),
        "journal append must not await a hook-owned lock"
    );
    let _ = release.expect("blocked invocation release").send(());
    drop(blocked);
}

#[test]
fn finalization_rejects_live_append_handles_then_succeeds_after_ordered_teardown() {
    let (runtime, _invocation, _store, _session, _turn, _release) = runtime_fixture(false);
    let journal = runtime.hook_state().journal().clone();
    assert!(
        journal.try_into_state().is_err(),
        "live runtime retains append authority"
    );

    let (runtime, _invocation, _store, _session, _turn, _release) = runtime_fixture(false);
    let journal = runtime.hook_state().journal().clone();
    drop(runtime);
    assert!(
        journal.try_into_state().is_ok(),
        "ordered teardown releases authority"
    );
}

#[test]
fn session_start_and_end_are_exact_once_and_reuse_durable_envelopes() {
    let (runtime, _invocation, store, session, turn, release) = runtime_fixture(false);
    admit_catalogue(&runtime, turn);
    let start = occurrence(HookLifecycle::SessionStart, session, turn, 5);
    let end = occurrence(HookLifecycle::SessionEnd, session, turn, 6);
    futures::executor::block_on(runtime.dispatch(Producer::Main, start.clone())).expect("start");
    futures::executor::block_on(runtime.dispatch(Producer::Main, start)).expect("replayed start");
    futures::executor::block_on(runtime.dispatch(Producer::Main, end.clone())).expect("end");
    futures::executor::block_on(runtime.dispatch(Producer::Main, end)).expect("replayed end");
    assert!(release.is_none());
    let events = store.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::HookLifecycleReached { .. }))
            .count(),
        2
    );
}

struct RejectingForeignPort;

impl ForeignQuestionPort for RejectingForeignPort {
    fn cancel(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    fn resume<'a>(
        &'a self,
        _continuation_id: &'a str,
        _canonical_json: &'a str,
        _causality: HookCausality,
    ) -> BoxFuture<'a, Result<ToolExecution, String>> {
        Box::pin(async { Err("runtime-bound port was not invoked".to_owned()) })
    }
}

#[derive(Default)]
struct RuntimeBinding;

impl McpRuntimeBinding for RuntimeBinding {
    fn load_pending_foreign_policy(
        &self,
        _continuation_id: &str,
    ) -> Result<Arc<ego_core::turn::GenerationPolicySnapshot>, String> {
        Err("hook fixture has no pending foreign policy".to_owned())
    }

    fn bind_pending_foreign_continuation(
        &self,
        _continuation_id: &str,
        _snapshot: Arc<ego_core::turn::GenerationPolicySnapshot>,
    ) -> Result<(), String> {
        Err("hook fixture cannot bind a pending continuation".to_owned())
    }

    fn hook_invocation_port<'a>(
        &self,
        _policy: Arc<ego_core::permit::PolicyEngine>,
        _effects: ToolEffectPorts<'a>,
    ) -> Result<Arc<dyn ego_core::hook::HookInvocationPort + 'a>, String> {
        panic!("this runtime-only binding is not exercised by the hook fixture")
    }

    fn foreign_question_port<'session, 'turn>(
        &self,
        _runtime: SessionHookRuntime<'session>,
        _floor: ResumeFloor,
        _effects: ToolEffectPorts<'turn>,
    ) -> Arc<dyn ForeignQuestionPort + 'turn>
    where
        'session: 'turn,
    {
        Arc::new(RejectingForeignPort)
    }
}

#[derive(Debug, Default)]
struct SinkFactory;

impl EventSinkFactory for SinkFactory {
    fn create(
        &self,
        _capabilities: &ego_core::generator::capabilities::ModelCapabilities,
    ) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

#[derive(Debug, Default)]
struct EmptyAdmittedTools;

impl AdmittedToolSource for EmptyAdmittedTools {
    fn capture(&self) -> Result<CapturedToolCatalogue, ego_core::turn::ToolCatalogueError> {
        CapturedToolCatalogue::new("session-hook-runtime", Vec::new())
    }
}

#[test]
fn runtime_binding_is_retained_by_execution_composition() {
    let runtime: Arc<dyn McpRuntimeBinding> = Arc::new(RuntimeBinding);
    let bindings = RunExecutionBindings::new(Arc::new(SinkFactory), None).with_mcp(
        Arc::new(EmptyAdmittedTools),
        vec!["session hook runtime diagnostic".to_owned()],
        Arc::clone(&runtime),
    );
    let forwarded = bindings.mcp_runtime().expect("runtime binding retained");
    assert!(Arc::ptr_eq(&runtime, &forwarded));
    assert_eq!(bindings.mcp_errors(), ["session hook runtime diagnostic"]);
    assert_eq!(
        bindings
            .admitted_tools()
            .expect("admitted tools retained")
            .capture()
            .expect("empty capture")
            .generation(),
        "session-hook-runtime"
    );
}
