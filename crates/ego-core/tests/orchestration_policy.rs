use ego_core::config::{Mode, Ruleset, Source, SourcedRule};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Classification, EnvPolicy, Op, PolicyEngine, PreparedCall, Resolution, SpawnSpec,
};
use ego_core::turn::{GenerationPolicyCapture, GenerationPolicySnapshot};
use futures::executor::block_on;

fn call(argv: &[&str]) -> PreparedCall {
    PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                argv.iter().map(|arg| (*arg).into()).collect(),
                Some(CanonPath::new("/ws")),
                None,
                EnvPolicy::Inherited,
            )
            .unwrap(),
        },
    )
}

fn rules(allow: &[&str], deny: &[&str]) -> Ruleset {
    let entries = |rules: &[&str]| {
        rules
            .iter()
            .map(|rule| SourcedRule::new(Rule::load(rule).unwrap(), Source::User))
            .collect()
    };
    Ruleset {
        allow: entries(allow),
        deny: entries(deny),
    }
}

fn asks(engine: &PolicyEngine, argv: &[&str]) -> bool {
    match engine.classify(call(argv)) {
        Classification::Settled(Ok(_)) => false,
        Classification::Settled(Err(error)) => panic!("unexpected denial: {error}"),
        Classification::Ask(pending) => {
            matches!(block_on(engine.settle(pending)), Resolution::Ask(_))
        }
    }
}

// Catches: an allow rule or yolo lets named High operations reach execution.
#[test]
fn high_operations_always_ask_even_with_allow_rules_and_yolo() {
    let cases: &[&[&str]] = &[
        &["git", "merge", "feature"],
        &["git", "-C", "/ws", "push"],
        &["rtk", "git", "branch", "-D", "feature"],
        &["gh", "pr", "create"],
        &[
            "tuic",
            "mcp",
            "repo",
            r#"{"action":"worktree_remove","path":"/ws"}"#,
        ],
        &[
            "tuic",
            "mcp",
            "session",
            r#"{"action":"close","session_id":"child"}"#,
        ],
        &[
            "tuic",
            "mcp",
            "session",
            r#"{"action":"kill","session_id":"child"}"#,
        ],
        &["curl", "https://openrouter.ai/api/alpha/decisions"],
        &["tuic-spawn.sh", "fable", "advisor", "/ws", "brief"],
        &["bash", "/tools/land/tuic-land-cleanup.sh", "/ws", "feature"],
        &["/tools/land/tuic-land.sh", "/ws", "feature"],
    ];
    for mode in [Mode::Auto, Mode::Yolo, Mode::Default] {
        let engine = PolicyEngine::new(CanonPath::new("/ws"), mode, rules(&["exec(*)"], &[]))
            .with_coordinator_profile(true);
        for argv in cases {
            assert!(asks(&engine, argv), "{mode:?} {argv:?}");
        }
    }
}

// Catches: coordinator Low/Bounded operations keep prompting through default mode.
#[test]
fn recognized_coordinator_reads_and_subscription_spawns_do_not_ask() {
    let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default())
        .with_coordinator_profile(true);
    for argv in [
        &["git", "status"][..],
        &["tuic", "ls"],
        &["mdkb", "search", "policy"],
        &["tuic-spawn.sh", "sol", "worker", "/ws", "brief"],
    ] {
        assert!(!asks(&engine, argv), "{argv:?}");
    }
    assert!(asks(&engine, &["unrecognized-program"]));
}

// Catches: coordinator auto-approval bypasses a deny or the plan floor.
#[test]
fn coordinator_policy_preserves_hard_denials() {
    let engine = PolicyEngine::new(
        CanonPath::new("/ws"),
        Mode::Yolo,
        rules(&["exec(*)"], &["exec(git *)"]),
    )
    .with_coordinator_profile(true);
    assert!(matches!(
        engine.classify(call(&["git", "merge", "feature"])),
        Classification::Settled(Err(_))
    ));
    let plan = PolicyEngine::new(CanonPath::new("/ws"), Mode::Plan, Ruleset::default())
        .with_coordinator_profile(true);
    assert!(matches!(
        plan.classify(call(&["tuic-spawn.sh", "sol"])),
        Classification::Settled(Err(_))
    ));
}

// Catches: a recognized Low spawn hides another Unknown spawn in the same call.
#[test]
fn low_orchestration_does_not_approve_unrelated_unknown_effects() {
    let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Auto, Ruleset::default())
        .with_coordinator_profile(true);
    let mixed = call(&["git", "status"]).with_op(call(&["unknown"]).ops()[0].clone());
    let Classification::Ask(pending) = engine.classify(mixed) else {
        panic!("missing analysis");
    };
    assert!(matches!(
        block_on(engine.settle(pending)),
        Resolution::Ask(_)
    ));
}

// Catches: restart drops the coordinator profile and lets an allow rule approve High.
#[test]
fn confidential_capture_restores_the_high_policy_and_changes_its_digest() {
    let ordinary = GenerationPolicyCapture::new(
        CanonPath::new("/ws"),
        Mode::Yolo,
        rules(&["exec(*)"], &[]),
        None,
    )
    .unwrap();
    let coordinator = GenerationPolicyCapture::with_coordinator_profile(
        CanonPath::new("/ws"),
        vec![],
        Mode::Yolo,
        rules(&["exec(*)"], &[]),
        None,
    )
    .unwrap();
    assert_ne!(
        ordinary.snapshot().digest(),
        coordinator.snapshot().digest()
    );
    let restored = GenerationPolicySnapshot::decode_confidential(
        &coordinator.snapshot().encode_confidential(),
    )
    .unwrap();
    let restored = GenerationPolicyCapture::restore(std::sync::Arc::new(restored), None).unwrap();
    assert!(asks(&restored.policy(), &["git", "merge", "feature"]));
    assert!(!asks(&ordinary.policy(), &["git", "merge", "feature"]));
    let default = PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default());
    assert!(asks(&default, &["git", "status"]));
}
