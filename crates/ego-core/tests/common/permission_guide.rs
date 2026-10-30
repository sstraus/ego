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

// Catches: correcting Adaptive erases the strict workspace guide or its root-list recovery steps.
#[test]
fn workspace_permission_guide_keeps_its_current_perimeter_and_recovery_text() {
    let guide = captured_stable_guide("workspace");
    for fact in [
        "The workspace sandbox confines reads, writes and exec.",
        "Absent explicit roots, file access is limited to the workspace.",
        "Extra read/write roots do not grant exec",
        "Use ego roots add to grant extra paths",
        "roots=[]",
        "ego config unset roots restores absence",
    ] {
        assert!(guide.contains(fact), "missing {fact:?}: {guide}");
    }
    assert!(!guide.contains("Adaptive sandbox"), "{guide}");
    assert!(!guide.contains("~/Gits"), "{guide}");
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
