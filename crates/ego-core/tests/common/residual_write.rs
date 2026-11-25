// The turn loop serving residual write refusals of a running exec call.
use super::*;
use ego_core::permit::residual::{
    ResidualDecision, ResidualListing, ResidualOutcome, ResidualPathKind, ResidualWriteDesk,
    ResidualWriteEvidence,
};

const CANDIDATE: &str = "/data/exports/report.json";

/// An exec whose child "printed" one refusal: it puts the candidate to the
/// session desk and keeps the decision it heard.
struct RefusingProcess {
    desk: Arc<ResidualWriteDesk>,
    admissible: bool,
    /// Report the refusal under another command, as a background exec would.
    foreign: bool,
    heard: Mutex<Vec<ResidualDecision>>,
}

impl ProcessExecutor for RefusingProcess {
    fn residual_desk(&self) -> Option<Arc<ResidualWriteDesk>> {
        Some(Arc::clone(&self.desk))
    }

    fn execute<'a>(
        &'a self,
        spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        Box::pin(async move {
            let evidence = ResidualWriteEvidence {
                requested: CANDIDATE.into(),
                candidate: CanonPath::new(CANDIDATE),
                exists: false,
                kind: ResidualPathKind::Other,
                owned_by_workspace_user: Some(true),
                listing: ResidualListing::default(),
                refusal: format!("touch: {CANDIDATE}: Operation not permitted"),
                writable: vec![CanonPath::new("/ws")],
            };
            let admissible = self.admissible;
            let spec = if self.foreign {
                SpawnSpec::new(vec!["/bin/other".into()], None, None, EnvPolicy::Inherited).unwrap()
            } else {
                spec.clone()
            };
            let decision = self
                .desk
                .decide(
                    spec,
                    evidence,
                    Box::new(move |_| {
                        if admissible {
                            Ok(())
                        } else {
                            Err("the path is under a protected floor".into())
                        }
                    }),
                )
                .await;
            self.heard.lock().unwrap().push(decision);
            Ok(ToolResult::builtin("refused"))
        })
    }
}

/// The main model runs one exec; the judge answers with `verdict`, or never
/// resolves when `verdict` is `None`.
struct ResidualSource {
    main: AtomicUsize,
    verdict: Option<String>,
    trace: GenerationTrace,
}

impl GenerationModelSource for ResidualSource {
    fn resolve<'a>(
        &'a self,
        request: GenerationModelRequest,
    ) -> BoxFuture<'a, Result<GenerationProfile, GenerationModelError>> {
        let judge = request.role() == GenerationRole::Judge;
        if judge && self.verdict.is_none() {
            return Box::pin(futures::future::pending());
        }
        Box::pin(async move {
            let events = if judge {
                vec![
                    AiEvent::TextDelta {
                        text: self.verdict.clone().unwrap(),
                    },
                    AiEvent::Stop(StopReason::Stop),
                ]
            } else if self.main.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![
                    AiEvent::ToolCall {
                        call_id: "process".into(),
                        name: "exec".into(),
                        args: r#"{"argv":["/usr/bin/exporter","--run"]}"#.into(),
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

/// A deadline that never fires, or one that already fired.
struct Deadline(bool);

impl ego_core::turn::RetryPause for Deadline {
    fn pause(&self, _delay: std::time::Duration) -> BoxFuture<'static, ()> {
        if self.0 {
            Box::pin(futures::future::ready(()))
        } else {
            Box::pin(futures::future::pending())
        }
    }
}

fn adaptive_judge() -> JudgeContext {
    JudgeContext {
        confinement: RunConfinement::new(Confinement::Adaptive, false, native_workspace()).unwrap(),
        exec_caps: ExecCaps::READ_SCOPED.union(ExecCaps::WRITE_SCOPED),
    }
}

struct Case<'a> {
    verdict: Option<&'a str>,
    admissible: bool,
    expired: bool,
    foreign: bool,
    mode: Mode,
}

const ANSWERED: Case<'static> = Case {
    verdict: None,
    admissible: true,
    expired: false,
    foreign: false,
    mode: Mode::Auto,
};

fn residual_session(case: Case<'_>) -> (Arc<RefusingProcess>, Vec<DomainEvent>) {
    let Case {
        verdict,
        admissible,
        expired,
        foreign,
        mode,
    } = case;
    let mut fx = Fixture::new(Vec::new()).with_exec_tool();
    // A rule settles the exec itself, so the judge only sees the refusal.
    let permissions = Ruleset::from_layer(
        PermCfg {
            judge: None,
            allow: vec![Rule::load("exec(*)").unwrap()],
            deny: Vec::new(),
        },
        Source::Project,
    );
    fx.policy = PolicyEngine::new(CanonPath::new("/ws"), mode, permissions.clone());
    fx.policy_source = Arc::new(FixedPolicySource {
        mode,
        permissions,
        human: None,
    });
    let process = Arc::new(RefusingProcess {
        desk: ResidualWriteDesk::new(),
        admissible,
        foreign,
        heard: Mutex::default(),
    });
    fx.process = process.clone();
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
        source: Arc::new(ResidualSource {
            main: AtomicUsize::new(0),
            verdict: verdict.map(str::to_owned),
            trace: GenerationTrace::default(),
        }),
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
        retry_pause: Arc::new(Deadline(expired)),
    };
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
    .with_judge(adaptive_judge());
    block_on(turn.run("export the report")).unwrap();
    drop(turn);
    (process, fx.store.events())
}

fn decided(events: &[DomainEvent]) -> Vec<(CanonPath, ResidualOutcome, Option<EventId>)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::SandboxResidualDecided { path, outcome, .. } => {
                Some((path.clone(), *outcome, event.env.parent))
            }
            _ => None,
        })
        .collect()
}

fn write_verdict(scope: &str) -> String {
    format!(
        r#"{{"grant":"write","path":"{CANDIDATE}","scope":"{scope}","reason":"the task exports there"}}"#
    )
}

// Catches: the loop publishes a judge grant the journal never recorded, or
// records it on no call, so an audit cannot say why a path became writable.
#[test]
fn a_judge_write_is_journaled_on_its_call_and_published_for_the_session() {
    let verdict = write_verdict("always");
    let (process, events) = residual_session(Case {
        verdict: Some(&verdict),
        ..ANSWERED
    });
    let started = events
        .iter()
        .find(|event| matches!(event.kind, EventKind::ToolCallStarted { .. }))
        .map(|event| event.env.id);
    assert_eq!(
        decided(&events),
        vec![(CanonPath::new(CANDIDATE), ResidualOutcome::Granted, started)]
    );
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::SandboxResidualDecided {
            recommended: Some(ego_core::permit::residual::ResidualScope::Always),
            attempt: Some(_),
            ..
        }
    )));
    assert_eq!(process.desk.grants(), vec![CanonPath::new(CANDIDATE)]);
    assert!(matches!(
        process.heard.lock().unwrap().as_slice(),
        [ResidualDecision::Granted { .. }]
    ));
}

// Catches: a write verdict is applied without the executor's floor recheck,
// so a protected path opens because the judge said yes.
#[test]
fn a_floor_path_is_never_granted_even_when_the_judge_writes_it() {
    let verdict = write_verdict("session");
    let (process, events) = residual_session(Case {
        verdict: Some(&verdict),
        admissible: false,
        ..ANSWERED
    });
    assert_eq!(
        decided(&events)
            .into_iter()
            .map(|(_, outcome, _)| outcome)
            .collect::<Vec<_>>(),
        vec![ResidualOutcome::Refused]
    );
    assert!(process.desk.grants().is_empty());
    assert!(matches!(
        process.heard.lock().unwrap().as_slice(),
        [ResidualDecision::Refused { .. }]
    ));
}

// Catches: a judge that never answers holds the exec call open, or its
// silence is read as a grant instead of leaving the question to the human.
#[test]
fn a_judge_that_does_not_answer_in_time_grants_nothing() {
    let (process, events) = residual_session(Case {
        expired: true,
        ..ANSWERED
    });
    assert_eq!(
        decided(&events)
            .into_iter()
            .map(|(_, outcome, _)| outcome)
            .collect::<Vec<_>>(),
        vec![ResidualOutcome::Unavailable]
    );
    assert!(process.desk.grants().is_empty());
    assert!(matches!(
        process.heard.lock().unwrap().as_slice(),
        [ResidualDecision::Unavailable { .. }]
    ));
}

// Catches: an invalid verdict (here an action verdict) is read as a grant.
#[test]
fn a_judge_error_grants_nothing() {
    let (process, events) = residual_session(Case {
        verdict: Some(r#"{"verdict":"allow","reason":"looks fine"}"#),
        ..ANSWERED
    });
    assert_eq!(
        decided(&events)
            .into_iter()
            .map(|(_, outcome, _)| outcome)
            .collect::<Vec<_>>(),
        vec![ResidualOutcome::Unavailable]
    );
    assert!(process.desk.grants().is_empty());
}

// Catches: a refusal from another command (a background exec) is judged
// against the running call's argv, so its grant rests on the wrong reason.
#[test]
fn a_refusal_of_another_command_is_not_judged() {
    let verdict = write_verdict("session");
    let (process, events) = residual_session(Case {
        verdict: Some(&verdict),
        foreign: true,
        ..ANSWERED
    });
    assert_eq!(
        decided(&events)
            .into_iter()
            .map(|(_, outcome, _)| outcome)
            .collect::<Vec<_>>(),
        vec![ResidualOutcome::Unavailable]
    );
    assert!(process.desk.grants().is_empty());
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::GenerationStarted {
            role: GenerationRole::Judge,
            ..
        }
    )));
}

// Catches: the sentinel opens paths in `ask` or `default` mode, where every
// widening belongs to the human.
#[test]
fn a_session_outside_auto_never_consults_the_residual_judge() {
    let verdict = write_verdict("session");
    let (process, events) = residual_session(Case {
        verdict: Some(&verdict),
        mode: Mode::Default,
        ..ANSWERED
    });
    assert!(decided(&events).is_empty());
    assert!(process.desk.grants().is_empty());
    assert!(matches!(
        process.heard.lock().unwrap().as_slice(),
        [ResidualDecision::Unavailable { .. }]
    ));
}
