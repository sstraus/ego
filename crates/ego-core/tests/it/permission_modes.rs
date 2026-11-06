//! Public permission-mode contracts and durable spelling compatibility.
use ego_core::config::{Mode, Ruleset, Source};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Change, Classification, Op, PolicyEngine, PreparedCall, Resolution,
};
use futures::executor::block_on;

// Catches: renaming edits changes the public cycle or rejects a valid default selection.
#[test]
fn mode_names_roundtrip_with_default_selectable_and_edits_alias() {
    assert_eq!(
        Mode::SELECTABLE.map(Mode::as_str),
        ["plan", "default", "auto", "accept-edits", "yolo"]
    );
    assert_eq!(
        serde_json::from_str::<Mode>("\"edits\"").unwrap(),
        Mode::Edits
    );
    assert_eq!(
        serde_json::to_string(&Mode::Edits).unwrap(),
        "\"accept-edits\""
    );
    for (index, mode) in Mode::SELECTABLE.into_iter().enumerate() {
        assert_eq!(Mode::parse_selection(mode.as_str()), Some(mode));
        assert_eq!(
            serde_json::from_str::<Mode>(&serde_json::to_string(&mode).unwrap()).unwrap(),
            mode
        );
        assert_eq!(mode.next(), Mode::SELECTABLE[(index + 1) % 5]);
    }
}

// Catches: a selection feature changes default's free reads, allow rules or coordinator admission.
#[test]
fn default_policy_keeps_read_admission_write_escalation_and_allow_rules() {
    let workspace = CanonPath::new("/workspace");
    let read = || {
        PreparedCall::new(
            "read",
            Op::Read {
                path: CanonPath::new("/workspace/a"),
            },
        )
    };
    let write = || {
        PreparedCall::new(
            "write",
            Op::Write {
                path: CanonPath::new("/workspace/a"),
                change: Change::Whole(b"x".to_vec()),
            },
        )
    };
    let engine = PolicyEngine::new(workspace.clone(), Mode::Default, Ruleset::default());
    let Classification::Ask(pending) = engine.classify(read()) else {
        panic!("uncovered read enters mode admission");
    };
    assert!(matches!(
        block_on(engine.settle(pending)),
        Resolution::Settled(Ok(_))
    ));
    let Classification::Ask(pending) = engine.classify(write()) else {
        panic!("uncovered write must ask");
    };
    assert!(matches!(
        block_on(engine.settle(pending)),
        Resolution::Ask(_)
    ));
    let mut rules = Ruleset::default();
    Ruleset::merge(
        &mut rules.allow,
        Rule::load("write(/workspace/**)").unwrap(),
        Source::User,
    );
    let covered = PolicyEngine::new(workspace.clone(), Mode::Default, rules);
    assert!(matches!(
        covered.classify(write()),
        Classification::Settled(Ok(_))
    ));
}

// Catches: accept-edits auto-approves writes in an admitted external root.
#[test]
fn accept_edits_only_auto_approves_workspace_file_edits() {
    use ego_core::permit::{ProfileRoot, RootAccess};
    let engine = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Edits,
        Ruleset::default(),
    )
    .with_profile_roots(vec![ProfileRoot::new(
        CanonPath::new("/extra"),
        RootAccess::ReadWrite,
    )]);
    for (path, approved) in [("/workspace/a", true), ("/extra/a", false)] {
        let Classification::Ask(pending) = engine.classify(PreparedCall::new(
            "write",
            Op::Write {
                path: CanonPath::new(path),
                change: Change::Whole(b"new".to_vec()),
            },
        )) else {
            panic!("uncovered write must be classified before mode admission");
        };
        assert_eq!(
            matches!(block_on(engine.settle(pending)), Resolution::Settled(Ok(_))),
            approved
        );
    }
}

// Catches: renaming edits breaks canonical snapshot decoding and resume digests.
#[test]
fn historical_edits_and_default_snapshots_resume_without_rewriting_authority() {
    use ego_core::credentials::SecretMaterial;
    use ego_core::turn::{GenerationPolicyCapture, GenerationPolicySnapshot};
    for (name, mode) in [("edits", Mode::Edits), ("default", Mode::Default)] {
        // Exact v1 wire order and spelling from the pre-change PolicyWire serializer.
        let bytes = format!(
            r#"{{"version":1,"workspace":"/workspace","mode":"{name}","allow":[],"deny":[]}}"#
        )
        .into_bytes();
        let decoded =
            GenerationPolicySnapshot::decode_confidential(&SecretMaterial::new(bytes.clone()))
                .expect("historical canonical snapshot remains admissible");
        assert_eq!(decoded.mode(), mode);
        assert_eq!(
            decoded.encode_confidential().expose_secret_bytes(),
            bytes.as_slice()
        );
        let current = GenerationPolicyCapture::new(
            CanonPath::new("/workspace"),
            mode,
            Ruleset::default(),
            None,
        )
        .unwrap();
        assert_eq!(decoded.digest(), current.snapshot().digest());
    }
}
