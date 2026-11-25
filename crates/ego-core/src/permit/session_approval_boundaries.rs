//! Fresh policy and session isolation remain authoritative after an approval.
use super::*;

#[test]
fn session_approval_survives_capture_but_not_new_session_or_plan() {
    // Catches: a fresh generation discards approval, or another session/Plan inherits it.
    use crate::config::{Mode, Ruleset};
    use crate::turn::GenerationPolicyCapture;
    let root = CanonPath::new(if cfg!(windows) {
        r"C:\workspace"
    } else {
        "/workspace"
    });
    let approvals = SessionApprovals::default();
    let capture = GenerationPolicyCapture::new(
        root.clone(),
        Mode::Default,
        Ruleset::default(),
        Some(Arc::new(Owner::default())),
    )
    .unwrap()
    .with_session_approvals(approvals.clone())
    .unwrap();
    let call = command(&root, "cat input.txt && cat input.txt");
    approve(&capture.policy(), call.clone());
    let captured = GenerationPolicyCapture::restore(capture.snapshot(), None)
        .unwrap()
        .with_session_approvals(approvals.clone())
        .unwrap()
        .with_session_read_root(CanonPath::new(root.as_path().join("outputs")))
        .unwrap()
        .with_read_perimeter(ReadPerimeter::default())
        .unwrap();
    assert!(matches!(
        captured.policy().classify(call.clone()),
        Classification::Settled(Ok(_))
    ));
    let new_session = GenerationPolicyCapture::restore(capture.snapshot(), None).unwrap();
    assert!(matches!(
        new_session.policy().classify(call.clone()),
        Classification::Ask(_)
    ));
    let plan =
        PolicyEngine::new(root, Mode::Plan, Ruleset::default()).with_session_approvals(approvals);
    assert!(matches!(
        plan.classify(call),
        Classification::Settled(Err(PolicyDenied::ModeForbids { .. }))
    ));
}

#[test]
fn session_approval_cannot_override_new_deny_or_changed_cwd_or_environment() {
    // Catches: reuse before fresh policy checks, or reuse by command text alone.
    use super::rule::Rule;
    use crate::config::{Mode, Ruleset, Source, SourcedRule};
    let root = CanonPath::new(if cfg!(windows) {
        r"C:\workspace"
    } else {
        "/workspace"
    });
    let approvals = SessionApprovals::default();
    let engine = PolicyEngine::with_human(
        root.clone(),
        Mode::Default,
        Ruleset::default(),
        Arc::new(Owner::default()),
    )
    .with_session_approvals(approvals.clone());
    let source = "cat input.txt && cat input.txt";
    approve(&engine, command(&root, source));
    let other = CanonPath::new(root.as_path().join("other"));
    assert!(matches!(
        engine.classify(command(&other, source)),
        Classification::Ask(_)
    ));
    let changed_env = PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                vec!["/bin/sh".into(), "-c".into(), source.into()],
                Some(root.clone()),
                None,
                EnvPolicy::exact(vec![
                    ExplicitEnvBinding::from_native(
                        EnvName::new("ENV").unwrap(),
                        "injected.sh".into(),
                    )
                    .unwrap(),
                ])
                .unwrap(),
            )
            .unwrap(),
        },
    );
    assert!(matches!(
        engine.classify(changed_env),
        Classification::Ask(_)
    ));
    let denied = PolicyEngine::new(
        root.clone(),
        Mode::Default,
        Ruleset {
            allow: vec![],
            deny: vec![SourcedRule::new(
                Rule::parse("exec(*)").unwrap(),
                Source::System,
            )],
            ..Ruleset::default()
        },
    )
    .with_session_approvals(approvals);
    assert!(matches!(
        denied.classify(command(&root, source)),
        Classification::Settled(Err(PolicyDenied::Denied {
            from: Source::System,
            ..
        }))
    ));
}

#[test]
fn consequential_command_question_explains_missing_approval_and_process_effects() {
    // Catches: a headless question names only a mode, leaving no actionable reason.
    let root = CanonPath::new(if cfg!(windows) {
        r"C:\workspace"
    } else {
        "/workspace"
    });
    let engine = PolicyEngine::new(
        root.clone(),
        crate::config::Mode::Default,
        crate::config::Ruleset::default(),
    );
    let Classification::Ask(pending) = engine.classify(command(&root, "rm input.txt")) else {
        panic!("approval required");
    };
    let Resolution::Ask(pending) = block_on(engine.settle(pending)) else {
        panic!("unapproved deletion must ask");
    };
    let question = pending.request().to_string();
    assert!(
        question.contains("no exact owner-approved command in this session"),
        "{question}"
    );
    assert!(
        question.contains("file changes or network access"),
        "{question}"
    );
    assert!(question.contains("rm input.txt"), "{question}");
}

#[derive(Debug)]
struct Answer(PermissionDecision);
impl HumanApprover for Answer {
    fn decide<'a>(
        &'a self,
        _: crate::event::id::ItemId,
        _: &'a PermissionRequest,
    ) -> BoxFuture<'a, PermissionDecision> {
        Box::pin(async { self.0.clone() })
    }
}

#[test]
fn only_owner_allow_records_a_session_approval() {
    // Catches: deny or amendment accidentally enters the exact-action cache.
    use crate::config::{Mode, Ruleset};
    let root = CanonPath::new(if cfg!(windows) {
        r"C:\workspace"
    } else {
        "/workspace"
    });
    for decision in [
        PermissionDecision::Deny,
        PermissionDecision::DenyWithInstruction {
            instruction: "read only".into(),
        },
        PermissionDecision::Amend {
            params: serde_json::json!({"command":"pwd"}),
        },
    ] {
        let engine = PolicyEngine::with_human(
            root.clone(),
            Mode::Default,
            Ruleset::default(),
            Arc::new(Answer(decision)),
        );
        let call = command(&root, "cat input.txt && cat input.txt");
        let Classification::Ask(pending) = engine.classify(call.clone()) else {
            panic!("first question");
        };
        let _ = block_on(engine.decide(crate::event::id::ItemId::new(), pending));
        assert!(matches!(engine.classify(call), Classification::Ask(_)));
    }
}
