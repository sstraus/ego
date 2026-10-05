use super::*;
use ego_core::permit::analysis::{DestructiveJudge, DestructiveOpinion, RiskClass};

struct Opinion {
    value: DestructiveOpinion,
    calls: AtomicUsize,
}
impl DestructiveJudge for Opinion {
    fn judge<'a>(
        &'a self,
        root: &'a Path,
        call: &'a PreparedCall,
    ) -> BoxFuture<'a, DestructiveOpinion> {
        assert_eq!(root, Path::new("/ws"));
        assert_eq!(call.tool(), "exec");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::future::ready(self.value.clone()))
    }
}
#[derive(Debug, Default)]
struct Human {
    risks: Mutex<Vec<RiskClass>>,
}
impl HumanApprover for Human {
    fn decide<'a>(
        &'a self,
        _: ItemId,
        request: &'a PermissionRequest,
    ) -> BoxFuture<'a, PermissionDecision> {
        self.risks
            .lock()
            .unwrap()
            .push(request.presentation().unwrap().risk().class);
        Box::pin(futures::future::ready(PermissionDecision::Deny))
    }
}
#[derive(Default)]
struct Process {
    executed: Mutex<Vec<Vec<OsString>>>,
}
impl ProcessExecutor for Process {
    fn execute<'a>(
        &'a self,
        spec: &'a SpawnSpec,
    ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
        self.executed.lock().unwrap().push(spec.argv().to_vec());
        Box::pin(futures::future::ready(Ok(ToolResult::builtin("ran"))))
    }
}
struct CoordinatorPolicy {
    mode: Mode,
    human: Arc<Human>,
    rules: Ruleset,
}
impl GenerationPolicySource for CoordinatorPolicy {
    fn current(&self) -> Result<Arc<PolicyEngine>, String> {
        Ok(self.capture()?.policy())
    }
    fn capture(&self) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::with_coordinator_profile(
            CanonPath::new("/ws"),
            vec![],
            self.mode,
            self.rules.clone(),
            Some(self.human.clone()),
        )
        .map_err(|e| e.to_string())
    }
    fn restore(
        &self,
        snapshot: Arc<GenerationPolicySnapshot>,
    ) -> Result<GenerationPolicyCapture, String> {
        GenerationPolicyCapture::restore(snapshot, Some(self.human.clone()))
            .map_err(|e| e.to_string())
    }
}

fn exercise(
    argv: &[&str],
    value: DestructiveOpinion,
    mode: Mode,
    deny: bool,
    legacy: bool,
) -> (usize, usize, Vec<RiskClass>, usize) {
    let mut fx = Fixture::new(vec![
        vec![
            AiEvent::ToolCall {
                call_id: "exec".into(),
                name: "exec".into(),
                args: json!({"argv":argv}).to_string(),
            },
            AiEvent::Stop(StopReason::ToolUse),
        ],
        vec![
            AiEvent::TextDelta {
                text: r#"{"verdict":"allow","reason":"safe"}"#.into(),
            },
            AiEvent::Stop(StopReason::Stop),
        ],
        vec![AiEvent::Stop(StopReason::Stop)],
    ])
    .with_exec_tool();
    let process = Arc::new(Process::default());
    fx.process = process.clone();
    let human = Arc::new(Human::default());
    let opinion = Arc::new(Opinion {
        value,
        calls: AtomicUsize::new(0),
    });
    let mut runtime = model_for(&fx, &fx.generator, &mut fx.gate.borrow_mut());
    runtime.policy_source = Arc::new(CoordinatorPolicy {
        mode,
        human: human.clone(),
        rules: if deny {
            Ruleset::from_layer(
                PermCfg {
                    judge: None,
                    allow: vec![],
                    deny: vec![Rule::load("exec(*)").unwrap()],
                },
                Source::User,
            )
        } else {
            Ruleset::default()
        },
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
    .with_destructive_judge(opinion.clone());
    if legacy {
        turn = turn.with_judge(JudgeContext {
            confinement: RunConfinement::new(Confinement::Workspace, false, CanonPath::new("/ws"))
                .unwrap(),
            exec_caps: ExecCaps::READ_SCOPED,
        });
    }
    block_on(turn.run("run command")).unwrap();
    drop(turn);
    let executed = process.executed.lock().unwrap();
    if !executed.is_empty() {
        assert_eq!(
            executed[0],
            argv.iter().map(OsString::from).collect::<Vec<_>>()
        );
        assert!(fx.store.events().iter().any(|e| matches!(
            e.kind,
            EventKind::PermissionDecided {
                allowed: Some(true),
                ..
            }
        )));
    }
    let judges = fx
        .store
        .events()
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::GenerationStarted {
                    role: GenerationRole::Judge,
                    ..
                }
            )
        })
        .count();
    let risks = human.risks.lock().unwrap().clone();
    (
        executed.len(),
        opinion.calls.load(Ordering::SeqCst),
        risks,
        judges,
    )
}
fn safe() -> DestructiveOpinion {
    DestructiveOpinion::Safe {
        model: ModelRef("~typesafe/jev-latest".into()),
    }
}

// Catches: a safe exec still prompts, or Jev approves a different canonical call.
#[test]
fn destructive_safe_exec_runs_without_a_human_or_legacy_judge() {
    for mode in [Mode::Auto, Mode::Default] {
        assert_eq!(
            exercise(
                &["custom-inspect", "literal ; rm -rf"],
                safe(),
                mode,
                false,
                true
            ),
            (1, 1, vec![], 0)
        );
    }
}
// Catches: destructive/uncertain scrutiny reaches the legacy allow judge or execution.
#[test]
fn destructive_card_is_high_and_never_falls_through_to_legacy_allow() {
    assert_eq!(
        exercise(
            &["rm", "-rf", "data"],
            DestructiveOpinion::Card,
            Mode::Auto,
            false,
            true
        ),
        (0, 1, vec![RiskClass::High], 0)
    );
}
// Catches: probabilistic approval bypasses deterministic High, deny rules or Plan.
#[test]
fn destructive_high_denied_and_plan_calls_never_reach_jev() {
    for argv in [
        &["git", "push", "--force"][..],
        &["git", "merge", "feature"],
        &["kill", "42"],
    ] {
        assert_eq!(
            exercise(argv, safe(), Mode::Auto, false, true),
            (0, 0, vec![RiskClass::High], 0)
        );
    }
    for (mode, deny) in [(Mode::Auto, true), (Mode::Plan, false)] {
        let (executed, calls, _, judges) = exercise(&["unknown"], safe(), mode, deny, true);
        assert_eq!((executed, calls, judges), (0, 0, 0));
    }
}
// Catches: switching Jev off approves Unknown unjudged or prevents current-judge fallback.
#[test]
fn destructive_disabled_uses_current_judge_or_card() {
    assert_eq!(
        exercise(
            &["unknown"],
            DestructiveOpinion::CurrentJudge,
            Mode::Auto,
            false,
            false
        ),
        (0, 1, vec![RiskClass::Unknown], 0)
    );
    assert_eq!(
        exercise(
            &["unknown"],
            DestructiveOpinion::CurrentJudge,
            Mode::Auto,
            false,
            true
        ),
        (1, 1, vec![], 1)
    );
}
