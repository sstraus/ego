use super::*;
use ego_core::config::{ConfigLayer, Source, Trust, resolve};

// Catches: Adaptive inherits workspace confinement and implicit ~/Gits claims in the cached prefix.
#[test]
fn adaptive_permission_guide_does_not_claim_workspace_or_implicit_root() {
    let guide = captured_stable_guide("adaptive");
    assert!(!guide.contains("The workspace sandbox"), "{guide}");
    assert!(!guide.contains("~/Gits"), "{guide}");
    for fact in [
        "Adaptive sandbox",
        "normal reads and installed executables",
        "credential floors",
        "Writes are confined to the workspace",
        "captured toolchain/cache/git/temp roots",
        "Workspace and derived build artifacts remain executable",
    ] {
        assert!(guide.contains(fact), "missing {fact:?}: {guide}");
    }
}

// Catches: correcting Adaptive erases the strict workspace guide or tells the agent to save a permanent root after a refusal.
#[test]
fn workspace_permission_guide_keeps_its_current_perimeter_and_recovery_text() {
    let guide = captured_stable_guide("workspace");
    for fact in [
        "With sandbox workspace or ro, file access stays within the admitted perimeter.",
        "yolo also admits ordinary native external writes and edits without a root question or saved root change.",
        "Explicit denies, read-only roots and protected-file floors still apply.",
        "Extra read/write roots do not grant exec",
        "Explicit roots can grant extra paths",
        "do not edit saved roots",
        "native interactive approval",
    ] {
        assert!(guide.contains(fact), "missing {fact:?}: {guide}");
    }
    assert!(!guide.contains("Adaptive sandbox"), "{guide}");
    assert!(!guide.contains("~/Gits"), "{guide}");
    assert!(!guide.contains("Use ego roots add"), "{guide}");
}

// Catches: the agent treats a chat answer as an effect grant or saves a permanent
// root by itself when an owner asks for access on one blocked operation.
#[test]
fn stable_guide_requires_native_owner_approval_for_every_scope() {
    for sandbox in ["workspace", "adaptive"] {
        let guide = captured_stable_guide(sandbox);
        for fact in [
            "use its native interactive approval",
            "grant for this session, a permanent grant, or denial",
            "unless the owner explicitly requests lasting access",
            "the owner must approve the actual scope",
            "ask_question only collects an answer and cannot grant effect authority",
            "Never change saved roots, rules or sandbox settings on your own",
            "re-admitted after the turn without a manual restart",
            "If no native approval is offered, report the exact error",
        ] {
            assert!(guide.contains(fact), "missing {fact:?}: {guide}");
        }
    }
}

fn captured_stable_guide(sandbox: &str) -> String {
    let layer = ConfigLayer::parse(Source::User, &format!("sandbox='{sandbox}'"))
        .expect("valid explicit confinement");
    let effective = resolve(&[layer], Trust::Trusted, None).expect("resolved confinement");
    let source: Arc<dyn AdmittedToolSource> = Arc::new(SequencedSource {
        state: Arc::new(Mutex::new(SourceState {
            catalogues: VecDeque::from([
                CapturedToolCatalogue::new("g1", Vec::new()).expect("empty catalogue"),
                CapturedToolCatalogue::new("g2", Vec::new()).expect("empty catalogue"),
            ]),
            ..SourceState::default()
        })),
    });
    let generator = Arc::new(FakeGenerator::new([
        vec![AiEvent::Stop(StopReason::Stop)],
        vec![AiEvent::Stop(StopReason::Stop)],
    ]));
    let (mut turn, _store) = loop_with_policy(
        generator.clone(),
        &[],
        source,
        common::policy_source_with_root("/workspace", Mode::Auto, Ruleset::default()),
        None,
        Some(AgentIdentity {
            version: "9.9.9".to_owned(),
            sandbox: effective.sandbox.expect("explicit confinement"),
            offline: false,
            host: None,
            config_notes: Vec::new(),
        }),
    )
    .expect("valid catalogue");
    futures::executor::block_on(turn.run("explain the perimeter")).expect("first turn");
    futures::executor::block_on(turn.run("continue")).expect("second turn");
    let calls = generator.calls();
    let first = system_texts(&calls[0]);
    let second = system_texts(&calls[1]);
    assert_eq!(
        first.first(),
        second.first(),
        "guide must remain cache stable"
    );
    first.first().expect("stable system identity").to_string()
}
