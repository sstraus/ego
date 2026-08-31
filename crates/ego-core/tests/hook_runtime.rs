//! RED contract for M4 S4: one typed hook catalogue and durable runner.
//!
//! Core receives already projected, source-bound hook values. YAML envelope
//! extraction belongs to the CLI boundary and is covered by its crate-private
//! projection test; this suite never feeds raw skill YAML to core.

use std::sync::{Arc, Mutex};

use ego_core::config::{ConfigLayer, Source, Sourced, Trust};
use ego_core::event::domain::EventKind;
use ego_core::event::hook::{
    HookActivation, HookDecision, HookFailure, HookFailureKind, HookInvocationId, HookLifecycle,
    HookOccurrenceKey, HookOccurrenceOwner, HookOutcome, HookPlan, HookResult, HookSubject,
};
use ego_core::event::id::{EventId, ItemId, Producer};
use ego_core::event::skill::{SkillInvocationId, SkillSourceAuthority};
use ego_core::hook::{
    HookCatalogue, HookCatalogueError, HookConfigEntry, HookEvent, HookFuture, HookHandlerConfig,
    HookInvocationRequest, HookMatcher, HookOccurrence, HookPort, HookReachFuture,
    HookReachedEvidence, HookRecordFuture, HookRunError, HookRunner, HookSource, HookSpec,
    HookStartError, HookStartFuture, HookStartedEvidence,
};
use ego_core::permit::rule::Rule;

fn layer(source: Source, toml: &str) -> Sourced<ConfigLayer> {
    ConfigLayer::parse(source, toml).expect("valid hook layer")
}

fn occurrence(event: HookEvent) -> HookOccurrence {
    let lifecycle = event.lifecycle();
    HookOccurrence::new(
        HookOccurrenceKey::new(
            lifecycle,
            HookOccurrenceOwner::Event {
                event: EventId::new(),
            },
        ),
        event,
    )
    .expect("matching lifecycle occurrence")
}

fn persistent_layers() -> [Sourced<ConfigLayer>; 3] {
    [
        layer(
            Source::System,
            "[[hooks]]\nevent = \"PreToolUse\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"fixture\"\n",
        ),
        layer(
            Source::User,
            "[[hooks]]\nevent = \"PostToolUse\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"fixture\"\n",
        ),
        layer(
            Source::Project,
            "[[hooks]]\nevent = \"PermissionDenied\"\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"fixture\"\n",
        ),
    ]
}

fn typed_skill_hook() -> HookConfigEntry {
    HookConfigEntry::new(
        HookLifecycle::SubagentStop,
        Some(HookMatcher::Permission(
            Rule::load("read(src/**)").expect("canonical permission rule"),
        )),
        HookHandlerConfig::Command {
            command: "/usr/bin/skill-hook".to_owned(),
            args: None,
            shell: None,
            timeout_seconds: None,
        },
    )
}

#[test]
fn typed_sources_and_skill_projection_make_one_strict_source_bound_catalogue() {
    let layers = persistent_layers();
    let skill = HookSource::skill_entries_with_authority(
        "skills/demo/SKILL.md",
        SkillInvocationId::new(),
        SkillSourceAuthority::User,
        [typed_skill_hook()],
    );
    let catalogue = HookCatalogue::admit_typed(&layers, &[skill], Trust::Trusted)
        .expect("complete typed catalogue");
    assert_eq!(catalogue.sources(), ["system", "user", "project", "skill"]);
    assert_eq!(catalogue.specs().len(), 4);
    assert!(
        catalogue
            .specs()
            .iter()
            .all(|spec| spec.location().is_authority_relative())
    );
    assert!(
        catalogue
            .specs()
            .iter()
            .all(|spec| spec.content_digest().is_lower_sha256())
    );
    assert!(
        catalogue
            .specs()
            .windows(2)
            .all(|pair| pair[0].sort_key() < pair[1].sort_key())
    );
    assert!(!catalogue.serialized_config().contains("/usr/bin"));
}

#[test]
fn exactly_21_events_are_admitted_and_exact_other_12_names_refuse() {
    let admitted = [
        "SessionStart",
        "InstructionsLoaded",
        "UserPromptSubmit",
        "UserPromptExpansion",
        "PreToolUse",
        "PermissionRequest",
        "PermissionDenied",
        "PostToolUse",
        "PostToolUseFailure",
        "PostToolBatch",
        "PreModelSwitch",
        "PostModelSwitch",
        "SubagentStart",
        "SubagentStop",
        "Stop",
        "StopFailure",
        "PreCompact",
        "PostCompact",
        "Elicitation",
        "ElicitationResult",
        "SessionEnd",
    ];
    let refused = [
        "Setup",
        "MessageDisplay",
        "Notification",
        "TaskCreated",
        "TaskCompleted",
        "TeammateIdle",
        "ConfigChange",
        "CwdChanged",
        "DirectoryAdded",
        "FileChanged",
        "WorktreeCreate",
        "WorktreeRemove",
    ];
    assert_eq!(admitted.len(), 21);
    assert_eq!(refused.len(), 12);
    for event in admitted {
        let spec = HookSpec::from_wire(event, "command", "/usr/bin/hook").expect("admitted event");
        assert_eq!(spec.event_name(), event);
    }
    for event in refused {
        let error = HookSpec::from_wire(event, "command", "/usr/bin/hook").unwrap_err();
        assert_eq!(error.exact_name(), event);
        assert!(matches!(error, HookCatalogueError::UnsupportedEvent { .. }));
    }
}

#[test]
fn closed_handlers_reject_exact_unknown_fields_and_admit_http_without_adapter() {
    for field in [
        "statusMessage",
        "async",
        "asyncRewake",
        "agent",
        "output",
        "regex",
    ] {
        let error = HookSpec::from_fields("PreToolUse", [(field, "value")]).unwrap_err();
        assert_eq!(error.exact_name(), field);
    }
    let http = HookHandlerConfig::Http {
        url: "https://hooks.example.test/notify".to_owned(),
        headers: Vec::new(),
        allowed_env_vars: None,
        timeout_seconds: None,
    };
    let spec = HookSpec::from_typed(
        HookConfigEntry::new(HookLifecycle::PostToolUse, None, http),
        HookSource::User,
        "hooks/user.toml",
        0,
    )
    .expect("catalogue admits closed HTTP handler before S5 adapter");
    assert_eq!(spec.event_name(), "PostToolUse");
}

#[derive(Clone, Default)]
struct DurableHookPort {
    started: Arc<Mutex<Vec<HookStartedEvidence>>>,
    settled: Arc<Mutex<Vec<HookInvocationId>>>,
    emitted: Arc<Mutex<Vec<HookEvent>>>,
    fail: Arc<Mutex<Option<HookFailureKind>>>,
    fail_started: Arc<Mutex<bool>>,
    durable_start_failure: Arc<Mutex<Option<HookStartedEvidence>>>,
    persist_started_before_failure: bool,
    block: bool,
}

impl DurableHookPort {
    fn fail_with(kind: HookFailureKind) -> Self {
        Self {
            fail: Arc::new(Mutex::new(Some(kind))),
            ..Self::default()
        }
    }

    fn blocking() -> Self {
        Self {
            block: true,
            ..Self::default()
        }
    }

    fn fail_started_after_durable_record() -> Self {
        Self {
            fail_started: Arc::new(Mutex::new(true)),
            durable_start_failure: Arc::new(Mutex::new(None)),
            persist_started_before_failure: true,
            ..Self::default()
        }
    }

    fn fail_started_without_durable_record() -> Self {
        Self {
            fail_started: Arc::new(Mutex::new(true)),
            durable_start_failure: Arc::new(Mutex::new(None)),
            ..Self::default()
        }
    }
}

impl HookPort for DurableHookPort {
    fn reached<'a>(
        &'a self,
        producer: &'a Producer,
        occurrence: &'a HookOccurrence,
        plan: &'a HookPlan,
    ) -> HookReachFuture<'a> {
        Box::pin(futures::future::ready(Ok(
            ego_core::hook::HookReachOutcome::Fresh(HookReachedEvidence::new(
                EventId::new(),
                occurrence.key().clone(),
                occurrence.event().subject(),
                producer.clone(),
                plan.clone(),
            )),
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
        let evidence = HookStartedEvidence::new(
            reached.event(),
            reached.event(),
            spec.id(),
            invocation,
            activation,
        );
        let fail = {
            let mut fail = self.fail_started.lock().expect("started failure");
            let fail_now = *fail;
            *fail = false;
            fail_now
        };
        if fail {
            let failure = HookRunError::new("durable hook start is uncertain");
            if self.persist_started_before_failure {
                self.started.lock().expect("started journal").push(evidence);
                *self
                    .durable_start_failure
                    .lock()
                    .expect("durable start failure evidence") = Some(evidence);
                Box::pin(futures::future::ready(Err(HookStartError::Durable {
                    evidence,
                    failure,
                })))
            } else {
                Box::pin(futures::future::ready(Err(HookStartError::Absent(failure))))
            }
        } else {
            self.started.lock().expect("started journal").push(evidence);
            Box::pin(futures::future::ready(Ok(evidence)))
        }
    }

    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a> {
        self.emitted.lock().expect("hook emissions").push(
            if let Some(selector) = request.selector() {
                HookEvent::tool(request.lifecycle(), request.subject(), selector)
            } else {
                HookEvent::turn(request.lifecycle(), request.subject())
            },
        );
        if let Some(kind) = self.fail.lock().expect("failure").take() {
            return Box::pin(async move { Err(HookFailure::new(kind, None)) });
        }
        let decision = if self.block {
            HookDecision::Block
        } else {
            HookDecision::Continue
        };
        Box::pin(async move { Ok(HookOutcome::new(decision, None)) })
    }

    fn settled<'a>(
        &'a self,
        _spec: &'a HookSpec,
        started: HookStartedEvidence,
        _result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
        self.settled
            .lock()
            .expect("settled journal")
            .push(started.invocation());
        Box::pin(async { Ok(()) })
    }
}

type PendingStart = (
    HookStartedEvidence,
    futures::channel::oneshot::Sender<Result<HookStartedEvidence, HookStartError>>,
);

type PendingStartSlot = Arc<Mutex<Option<PendingStart>>>;

#[derive(Clone, Default)]
struct PendingStartHookPort {
    started: Arc<Mutex<Vec<HookStartedEvidence>>>,
    pending: PendingStartSlot,
    start_calls: Arc<Mutex<usize>>,
    invoke_calls: Arc<Mutex<usize>>,
}

impl PendingStartHookPort {
    fn release(&self) {
        let (evidence, sender) = self
            .pending
            .lock()
            .expect("pending starts")
            .take()
            .expect("one pending start");
        self.started.lock().expect("started journal").push(evidence);
        let _ = sender.send(Ok(evidence));
    }
}

impl HookPort for PendingStartHookPort {
    fn reached<'a>(
        &'a self,
        producer: &'a Producer,
        occurrence: &'a HookOccurrence,
        plan: &'a HookPlan,
    ) -> HookReachFuture<'a> {
        Box::pin(futures::future::ready(Ok(
            ego_core::hook::HookReachOutcome::Fresh(HookReachedEvidence::new(
                EventId::new(),
                occurrence.key().clone(),
                occurrence.event().subject(),
                producer.clone(),
                plan.clone(),
            )),
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
        let evidence = HookStartedEvidence::new(
            reached.event(),
            reached.event(),
            spec.id(),
            invocation,
            activation,
        );
        let (sender, receiver) = futures::channel::oneshot::channel();
        *self.start_calls.lock().expect("pending start calls") += 1;
        *self.pending.lock().expect("pending starts") = Some((evidence, sender));
        Box::pin(async move {
            match receiver.await {
                Ok(result) => result,
                Err(_) => Err(HookStartError::Absent(HookRunError::new(
                    "pending hook start was cancelled",
                ))),
            }
        })
    }

    fn invoke<'a>(&'a self, _request: HookInvocationRequest) -> HookFuture<'a> {
        *self.invoke_calls.lock().expect("hook invokes") += 1;
        Box::pin(async { Ok(HookOutcome::new(HookDecision::Continue, None)) })
    }

    fn settled<'a>(
        &'a self,
        _spec: &'a HookSpec,
        _started: HookStartedEvidence,
        _result: &'a HookResult,
    ) -> HookRecordFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

fn typed_catalogue() -> HookCatalogue {
    let layers = persistent_layers();
    HookCatalogue::admit_typed(
        &layers,
        &[HookSource::skill_entries_with_authority(
            "skills/demo/SKILL.md",
            SkillInvocationId::new(),
            SkillSourceAuthority::User,
            [typed_skill_hook()],
        )],
        Trust::Trusted,
    )
    .expect("typed catalogue")
}

fn once_catalogue() -> HookCatalogue {
    let layers = persistent_layers();
    HookCatalogue::admit_typed(
        &layers,
        &[HookSource::skill_entries_with_authority(
            "skills/demo/SKILL.md",
            SkillInvocationId::new(),
            SkillSourceAuthority::User,
            [typed_skill_hook().with_once(true)],
        )],
        Trust::Trusted,
    )
    .expect("typed once catalogue")
}

fn multi_once_catalogue() -> HookCatalogue {
    let layers = persistent_layers();
    let matcher = || {
        Some(HookMatcher::Permission(
            Rule::load("read(src/**)").expect("canonical permission rule"),
        ))
    };
    let hooks = [
        HookConfigEntry::new(
            HookLifecycle::SubagentStop,
            matcher(),
            HookHandlerConfig::Command {
                command: "/usr/bin/skill-hook-a".to_owned(),
                args: None,
                shell: None,
                timeout_seconds: None,
            },
        )
        .with_once(true),
        HookConfigEntry::new(
            HookLifecycle::SubagentStop,
            matcher(),
            HookHandlerConfig::Command {
                command: "/usr/bin/skill-hook-b".to_owned(),
                args: None,
                shell: None,
                timeout_seconds: None,
            },
        )
        .with_once(true),
    ];
    HookCatalogue::admit_typed(
        &layers,
        &[HookSource::skill_entries_with_authority(
            "skills/multi-once/SKILL.md",
            SkillInvocationId::new(),
            SkillSourceAuthority::User,
            hooks,
        )],
        Trust::Trusted,
    )
    .expect("typed multi-once catalogue")
}

fn failure_catalogue() -> HookCatalogue {
    HookCatalogue::admit_typed(
        &[],
        &[HookSource::skill_entries_with_authority(
            "skills/failure/SKILL.md",
            SkillInvocationId::new(),
            SkillSourceAuthority::User,
            [
                HookConfigEntry::new(
                    HookLifecycle::PreCompact,
                    None,
                    HookHandlerConfig::Command {
                        command: "/usr/bin/pre-compact".to_owned(),
                        args: None,
                        shell: None,
                        timeout_seconds: None,
                    },
                ),
                HookConfigEntry::new(
                    HookLifecycle::PostCompact,
                    None,
                    HookHandlerConfig::Command {
                        command: "/usr/bin/post-compact".to_owned(),
                        args: None,
                        shell: None,
                        timeout_seconds: None,
                    },
                ),
            ],
        )],
        Trust::Trusted,
    )
    .expect("typed failure catalogue")
}

#[test]
fn deterministic_matching_uses_permission_ast_and_valid_block_wins_with_durable_feedback() {
    let catalogue = typed_catalogue();
    let event = HookEvent::tool(
        HookLifecycle::SubagentStop,
        ego_core::event::hook::HookSubject::Item {
            item: ItemId::new(),
        },
        "read(src/main.rs)",
    );
    let first = catalogue.matching(&event);
    let second = catalogue.matching(&event);
    assert_eq!(first, second);
    assert!(
        first
            .windows(2)
            .all(|pair| pair[0].sort_key() < pair[1].sort_key())
    );
    let port = DurableHookPort::blocking();
    let runner = HookRunner::new(port.clone());
    let result = futures::executor::block_on(runner.run_occurrence(
        &catalogue,
        Producer::Main,
        occurrence(event),
    ))
    .expect("hook run");
    assert_eq!(result.decision(), HookDecision::Block);
    assert!(result.feedback_bytes() <= 4096);
    let started = port.started.lock().expect("starts").clone();
    assert!(!started.is_empty());
    assert_eq!(result.blocking_invocation(), Some(started[0].invocation()));
    assert_eq!(started.len(), port.settled.lock().expect("settles").len());
}

#[test]
fn pre_effect_failure_blocks_observer_failure_stays_durable_and_effect_is_not_reverted() {
    let catalogue = failure_catalogue();
    let pre = futures::executor::block_on(
        HookRunner::new(DurableHookPort::fail_with(HookFailureKind::HandlerFailed)).run_occurrence(
            &catalogue,
            Producer::Main,
            occurrence(HookEvent::session(HookLifecycle::PreCompact)),
        ),
    )
    .expect("pre result");
    assert!(pre.is_blocked());
    let post = futures::executor::block_on(
        HookRunner::new(DurableHookPort::fail_with(HookFailureKind::HandlerFailed)).run_occurrence(
            &catalogue,
            Producer::Main,
            occurrence(HookEvent::session(HookLifecycle::PostCompact)),
        ),
    )
    .expect("post result");
    assert!(post.is_observer_failure());
    assert!(post.is_durable());
    assert!(!post.reverted_effect());
}

#[test]
fn hook_started_event_persists_closed_skill_activation_for_replay() {
    let catalogue = once_catalogue();
    let hook = catalogue
        .specs()
        .iter()
        .find(|spec| spec.event_name() == "SubagentStop")
        .expect("skill hook")
        .id();
    let skill_invocation = SkillInvocationId::new();
    let event = EventKind::HookInvocationStarted {
        occurrence: EventId::new(),
        hook,
        invocation: HookInvocationId::new(),
        lifecycle: HookLifecycle::SubagentStop,
        subject: HookSubject::Session,
        activation: HookActivation::Skill {
            invocation: skill_invocation,
        },
    };
    let encoded = serde_json::to_value(&event).expect("hook start encoding");
    assert_eq!(encoded["activation"]["kind"], "skill");
    assert_eq!(
        encoded["activation"]["invocation"],
        skill_invocation.to_string()
    );
    let decoded: EventKind = serde_json::from_value(encoded).expect("hook start decoding");
    assert!(matches!(
        decoded,
        EventKind::HookInvocationStarted {
            activation: HookActivation::Skill { invocation },
            ..
        } if invocation == skill_invocation
    ));
}

#[test]
fn durable_once_rebuilds_in_a_new_runner_and_hook_producers_are_suppressed() {
    let catalogue = once_catalogue();
    let port = DurableHookPort::default();
    let event = HookEvent::tool(
        HookLifecycle::SubagentStop,
        HookSubject::Item {
            item: ItemId::new(),
        },
        "read(src/main.rs)",
    );
    let durable_started = port.started.lock().expect("started evidence").clone();
    let first = futures::executor::block_on(
        HookRunner::new_with_durable_started(port.clone(), durable_started).run_occurrence(
            &catalogue,
            Producer::Main,
            occurrence(event.clone()),
        ),
    )
    .expect("first once invocation");
    assert!(first.started_invocation());
    let started = port.started.lock().expect("started evidence").clone();
    let skill_invocation = started
        .iter()
        .find_map(|evidence| match evidence.activation() {
            HookActivation::Skill { invocation } => Some(invocation),
            HookActivation::Persistent => None,
        })
        .expect("skill activation evidence");
    let hook = started.iter().find(|evidence| {
        matches!(evidence.activation(), HookActivation::Skill { invocation } if invocation == skill_invocation)
    }).expect("matching skill hook evidence").hook();
    assert!(started.iter().any(|evidence| {
        evidence.hook() == hook
            && matches!(evidence.activation(), HookActivation::Skill { invocation } if invocation == skill_invocation)
    }));
    let rebuilt = HookRunner::new_with_durable_started(port.clone(), started.clone());
    let second = futures::executor::block_on(rebuilt.run_occurrence(
        &catalogue,
        Producer::Main,
        occurrence(event.clone()),
    ))
    .expect("replayed once");
    assert!(second.was_skipped_once());
    let persistent_replay = vec![HookStartedEvidence::new(
        EventId::new(),
        EventId::new(),
        hook,
        started[0].invocation(),
        HookActivation::Persistent,
    )];
    let fresh = HookRunner::new_with_durable_started(port.clone(), persistent_replay);
    let not_skipped = futures::executor::block_on(fresh.run_occurrence(
        &catalogue,
        Producer::Main,
        occurrence(HookEvent::tool(
            HookLifecycle::SubagentStop,
            HookSubject::Item {
                item: ItemId::new(),
            },
            "read(src/main.rs)",
        )),
    ))
    .expect("persistent activation does not consume skill once");
    assert!(!not_skipped.was_skipped_once());
    let suppressed = futures::executor::block_on(rebuilt.run_occurrence(
        &catalogue,
        Producer::Hook(hook),
        occurrence(HookEvent::session(HookLifecycle::SessionStart)),
    ))
    .expect("suppressed recursive event");
    assert!(!suppressed.started_invocation());
}

#[test]
fn once_start_uncertainty_is_durable_and_rebuilt_runner_cannot_repeat() {
    let catalogue = once_catalogue();
    let port = DurableHookPort::fail_started_after_durable_record();
    let event = HookEvent::tool(
        HookLifecycle::SubagentStop,
        HookSubject::Item {
            item: ItemId::new(),
        },
        "read(src/main.rs)",
    );
    let runner = HookRunner::new(port.clone());
    let first = futures::executor::block_on(runner.run_occurrence(
        &catalogue,
        Producer::Main,
        occurrence(event.clone()),
    ))
    .expect("uncertain pre-effect hook result");
    assert!(
        first.is_blocked(),
        "uncertain pre-effect delivery fails closed"
    );
    assert!(
        first.is_durable(),
        "uncertainty is durable before it is reported"
    );
    assert!(!first.was_skipped_once());
    let started = port.started.lock().expect("durable starts").clone();
    assert_eq!(started.len(), 1);
    assert_eq!(first.blocking_invocation(), Some(started[0].invocation()));
    assert!(matches!(
        started[0].activation(),
        HookActivation::Skill { .. }
    ));
    assert_eq!(
        *port
            .durable_start_failure
            .lock()
            .expect("durable start failure evidence"),
        Some(started[0])
    );

    let live_retry = futures::executor::block_on(runner.run_occurrence(
        &catalogue,
        Producer::Main,
        occurrence(event.clone()),
    ))
    .expect("live runner consumes durable once evidence");
    assert!(live_retry.was_skipped_once());
    assert!(!live_retry.started_invocation());
    assert_eq!(port.started.lock().expect("durable starts").len(), 1);

    let rebuilt = HookRunner::new_with_durable_started(port.clone(), started);
    let replay = futures::executor::block_on(rebuilt.run_occurrence(
        &catalogue,
        Producer::Main,
        occurrence(event),
    ))
    .expect("reconstructed runner consumes durable uncertainty");
    assert!(replay.was_skipped_once());
    assert!(!replay.started_invocation());
    assert_eq!(port.started.lock().expect("durable starts").len(), 1);
}

#[test]
fn concurrent_once_holds_admission_until_one_pending_start_completes() {
    use futures::task::LocalSpawnExt;

    let catalogue = once_catalogue();
    let port = PendingStartHookPort::default();
    let runner = HookRunner::new(port.clone());
    let event = HookEvent::tool(
        HookLifecycle::SubagentStop,
        HookSubject::Item {
            item: ItemId::new(),
        },
        "read(src/main.rs)",
    );
    let mut pool = futures::executor::LocalPool::new();
    let spawner = pool.spawner();
    let first_runner = runner.clone();
    let first_catalogue = catalogue.clone();
    let first_event = event.clone();
    let first_future = spawner
        .spawn_local_with_handle(async move {
            first_runner
                .run_occurrence(&first_catalogue, Producer::Main, occurrence(first_event))
                .await
        })
        .expect("spawn first concurrent once run");
    let second_runner = runner.clone();
    let second_catalogue = catalogue.clone();
    let second_future = spawner
        .spawn_local_with_handle(async move {
            second_runner
                .run_occurrence(&second_catalogue, Producer::Main, occurrence(event))
                .await
        })
        .expect("spawn second concurrent once run");
    pool.run_until_stalled();
    assert_eq!(
        port.start_calls
            .lock()
            .expect("pending start calls")
            .to_owned(),
        1,
        "first run reached the externally controlled pending start"
    );
    assert_eq!(
        port.start_calls
            .lock()
            .expect("pending start calls")
            .to_owned(),
        1,
        "second run waits on the per-key admission"
    );
    port.release();
    let first = pool.run_until(first_future);
    let second = pool.run_until(second_future);
    let first = first.expect("first concurrent once run");
    let second = second.expect("second concurrent once run");
    assert_eq!(
        port.start_calls
            .lock()
            .expect("pending start calls")
            .to_owned(),
        1,
        "the waiting run is skipped without a second port start"
    );
    assert_eq!(port.started.lock().expect("started journal").len(), 1);
    assert_eq!(
        port.invoke_calls.lock().expect("hook invokes").to_owned(),
        1,
        "only the durable winner invokes"
    );
    assert_eq!(
        usize::from(first.was_skipped_once()) + usize::from(second.was_skipped_once()),
        1
    );
    assert_ne!(first.started_invocation(), second.started_invocation());
}

#[test]
fn multiple_matching_once_hooks_each_consume_their_own_reservation() {
    let catalogue = multi_once_catalogue();
    let port = DurableHookPort::default();
    let runner = HookRunner::new(port.clone());
    let event = HookEvent::tool(
        HookLifecycle::SubagentStop,
        HookSubject::Item {
            item: ItemId::new(),
        },
        "read(src/main.rs)",
    );

    let first = futures::executor::block_on(runner.run_occurrence(
        &catalogue,
        Producer::Main,
        occurrence(event.clone()),
    ))
    .expect("both matching once hooks run");
    assert!(first.started_invocation());
    assert!(!first.was_skipped_once());
    assert_eq!(port.started.lock().expect("started evidence").len(), 2);
    assert_eq!(port.settled.lock().expect("settled evidence").len(), 2);

    let second = futures::executor::block_on(runner.run_occurrence(
        &catalogue,
        Producer::Main,
        occurrence(event),
    ))
    .expect("both once reservations are consumed");
    assert!(second.was_skipped_once());
    assert!(!second.started_invocation());
    assert_eq!(port.started.lock().expect("started evidence").len(), 2);
    assert_eq!(port.settled.lock().expect("settled evidence").len(), 2);
}

#[test]
fn once_start_without_durable_fact_does_not_preconsume_in_memory() {
    let catalogue = once_catalogue();
    let port = DurableHookPort::fail_started_without_durable_record();
    let runner = HookRunner::new(port.clone());
    let event = HookEvent::tool(
        HookLifecycle::SubagentStop,
        HookSubject::Item {
            item: ItemId::new(),
        },
        "read(src/main.rs)",
    );
    let first = futures::executor::block_on(runner.run_occurrence(
        &catalogue,
        Producer::Main,
        occurrence(event.clone()),
    ))
    .expect("closed result for uncertain start");
    assert!(first.is_blocked());
    assert!(!first.started_invocation());
    assert!(
        first.is_durable(),
        "the reached occurrence is durable even without a durable start"
    );
    assert_eq!(first.blocking_invocation(), None);
    assert!(
        port.durable_start_failure
            .lock()
            .expect("durable start failure evidence")
            .is_none()
    );
    assert!(port.started.lock().expect("durable starts").is_empty());

    // This is an explicit probe: absence of a durable start must not leave a
    // stale in-memory once bit that silently suppresses a later admitted run.
    let retry = futures::executor::block_on(runner.run_occurrence(
        &catalogue,
        Producer::Main,
        occurrence(event),
    ))
    .expect("explicit retry after absent durable fact");
    assert!(retry.started_invocation());
    assert_eq!(port.started.lock().expect("durable starts").len(), 1);
}

#[test]
fn catalogue_reload_builds_a_real_candidate_then_atomically_keeps_last_good() {
    let mut catalogue = typed_catalogue();
    let before = catalogue.generation().to_owned();
    let invalid = [layer(
        Source::User,
        "[[hooks]]\nevent = \"PostToolUse\"\n[hooks.handler]\ntype = \"command\"\ncommand = \"\"\n",
    )];
    assert!(
        catalogue
            .reload_typed(&invalid, &[], Trust::Trusted)
            .is_err()
    );
    assert_eq!(catalogue.generation(), before);
    let layers = persistent_layers();
    let first_invocation = SkillInvocationId::new();
    let second_invocation = SkillInvocationId::new();
    let skills = [
        HookSource::skill_entries_with_authority(
            "skills/reloaded/first/SKILL.md",
            first_invocation,
            SkillSourceAuthority::User,
            [
                typed_skill_hook().with_once(true),
                HookConfigEntry::new(
                    HookLifecycle::PreCompact,
                    None,
                    HookHandlerConfig::Command {
                        command: "/usr/bin/reloaded-first".to_owned(),
                        args: None,
                        shell: None,
                        timeout_seconds: None,
                    },
                ),
            ],
        ),
        HookSource::skill_entries_with_authority(
            "skills/reloaded/second/SKILL.md",
            second_invocation,
            SkillSourceAuthority::User,
            [
                HookConfigEntry::new(
                    HookLifecycle::SubagentStart,
                    None,
                    HookHandlerConfig::Command {
                        command: "/usr/bin/reloaded-second-start".to_owned(),
                        args: None,
                        shell: None,
                        timeout_seconds: None,
                    },
                ),
                HookConfigEntry::new(
                    HookLifecycle::PostCompact,
                    None,
                    HookHandlerConfig::Command {
                        command: "/usr/bin/reloaded-second-compact".to_owned(),
                        args: None,
                        shell: None,
                        timeout_seconds: None,
                    },
                ),
            ],
        ),
    ];
    let expected = HookCatalogue::admit_typed(&layers, &skills, Trust::Trusted)
        .expect("identical source-bound candidate");
    catalogue
        .reload_typed(&layers, &skills, Trust::Trusted)
        .expect("candidate validates before publication");
    assert_ne!(catalogue.generation(), before);
    assert_eq!(catalogue.sources(), expected.sources());
    assert_eq!(catalogue.specs(), expected.specs());

    let reloaded_skill_specs = catalogue
        .specs()
        .iter()
        .filter(|spec| spec.skill_invocation().is_some())
        .collect::<Vec<_>>();
    assert_eq!(reloaded_skill_specs.len(), 4);
    assert_eq!(
        reloaded_skill_specs
            .iter()
            .map(|spec| spec.skill_invocation())
            .collect::<Vec<_>>(),
        vec![
            Some(first_invocation),
            Some(first_invocation),
            Some(second_invocation),
            Some(second_invocation),
        ]
    );
    assert_eq!(
        reloaded_skill_specs
            .iter()
            .map(|spec| spec.ordinal().expect("skill source ordinal"))
            .collect::<Vec<_>>(),
        vec![0, 1, 0, 1]
    );
    assert_eq!(
        reloaded_skill_specs
            .iter()
            .map(|spec| spec.location().as_str())
            .collect::<Vec<_>>(),
        vec![
            "skills/reloaded/first/SKILL.md",
            "skills/reloaded/first/SKILL.md",
            "skills/reloaded/second/SKILL.md",
            "skills/reloaded/second/SKILL.md",
        ]
    );
}

#[test]
fn persistent_once_is_refused_and_candidate_duplicates_are_not_published() {
    assert!(matches!(
        HookCatalogue::admit_typed(
            &[layer(
                Source::User,
                "[[hooks]]\nevent = \"PostToolUse\"\nonce = true\n[hooks.handler]\ntype = \"prompt\"\nprompt = \"fixture\"\n",
            )],
            &[],
            Trust::Trusted,
        ),
        Err(HookCatalogueError::Invalid { exact_name, .. }) if exact_name == "once"
    ));
    let duplicate = HookCatalogue::admit_typed(
        &[],
        &[HookSource::skill_entries_with_authority(
            "skills/demo/SKILL.md",
            SkillInvocationId::new(),
            SkillSourceAuthority::User,
            [typed_skill_hook(), typed_skill_hook()],
        )],
        Trust::Trusted,
    );
    assert!(duplicate.is_err());
}
