//! regression case: observable policy and exact, memory-only human consent.
use ego_core::config::{Mode, Ruleset, Source};
use ego_core::permit::analysis::{ExecutableFileIdentity, ExecutionIdentity};
use ego_core::permit::rule::Rule;
use ego_core::permit::{
    CanonPath, Change, Classification, EnvName, EnvPolicy, ExplicitEnvBinding, Op,
    PermissionRequest, PolicyEngine, PreparedCall, Resolution, SessionPermissions, SpawnSpec,
};
use futures::executor::block_on;

fn read(path: &str) -> PreparedCall {
    PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new(path),
        },
    )
}

fn request(engine: &PolicyEngine, call: PreparedCall) -> PermissionRequest {
    let Classification::Ask(pending) = engine.classify(call) else {
        panic!("manual must ask even for a read or an allow-covered effect");
    };
    let Resolution::Ask(pending) = block_on(engine.settle(pending)) else {
        panic!("manual must not be approved by mode, coordinator or risk analysis");
    };
    pending.request().clone()
}

// Catches: allow rules, read auto-admission, or coordinator analysis bypass manual consent.
#[test]
fn manual_reads_and_allow_covered_writes_still_ask() {
    let mut rules = Ruleset::default();
    for pattern in ["read(/workspace/**)", "write(/workspace/**)"] {
        Ruleset::merge(&mut rules.allow, Rule::load(pattern).unwrap(), Source::User);
    }
    let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Manual, rules);
    assert!(request(&engine, read("/workspace/a")).is_manual());
    assert!(
        request(
            &engine,
            PreparedCall::new(
                "write",
                Op::Write {
                    path: CanonPath::new("/workspace/a"),
                    change: Change::Whole(b"new".to_vec()),
                }
            )
        )
        .is_manual()
    );
}

// Catches: a human grant is considered before explicit denies or workspace boundaries.
#[test]
fn manual_denies_and_boundaries_never_reach_the_human() {
    let mut rules = Ruleset::default();
    Ruleset::merge(
        &mut rules.deny,
        Rule::load("read(/workspace/secret)").unwrap(),
        Source::System,
    );
    let engine = PolicyEngine::new(CanonPath::new("/workspace"), Mode::Manual, rules);
    for call in [read("/workspace/secret"), read("/outside/a")] {
        assert!(matches!(
            engine.classify(call),
            Classification::Settled(Err(_))
        ));
    }
}

fn exec(arg: &str, cwd: &str, value: &str, digest: u8) -> PreparedCall {
    let env = EnvPolicy::exact(vec![
        ExplicitEnvBinding::from_native(EnvName::new("TEST_SCOPE").unwrap(), value.into()).unwrap(),
    ])
    .unwrap();
    let identity = ExecutionIdentity {
        target: CanonPath::new("/bin/echo"),
        files: vec![ExecutableFileIdentity {
            named: CanonPath::new("/bin/echo"),
            resolved: CanonPath::new("/bin/echo"),
            content: Some([digest; 32]),
            platform: vec![],
        }],
        failures: vec![],
        dynamic_code: false,
    };
    let spec = SpawnSpec::new(
        vec!["/bin/echo".into(), arg.into()],
        Some(CanonPath::new(cwd)),
        None,
        env.clone(),
    )
    .unwrap()
    .with_execution_identity(env, identity);
    PreparedCall::new("exec", Op::Spawn { spec })
}

// Catches: session approval accidentally grants all commands, altered context, or future sessions.
#[test]
fn manual_session_grants_match_argv_cwd_environment_and_executable_identity() {
    let engine = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Manual,
        Ruleset::default(),
    );
    let original = request(&engine, exec("a", "/workspace", "one", 1));
    let mut session = SessionPermissions::default();
    assert!(!session.allows(&original));
    assert!(session.allow(&original));
    assert!(session.allows(&request(&engine, exec("a", "/workspace", "one", 1))));
    for changed in [
        exec("b", "/workspace", "one", 1),
        exec("a", "/workspace/sub", "one", 1),
        exec("a", "/workspace", "two", 1),
        exec("a", "/workspace", "one", 2),
    ] {
        assert!(!session.allows(&request(&engine, changed)));
    }
    assert!(!SessionPermissions::default().allows(&original));
    let other = PolicyEngine::new(CanonPath::new("/other"), Mode::Manual, Ruleset::default());
    assert!(!session.allows(&request(&other, exec("a", "/workspace", "one", 1))));
}

// Catches: restored legacy default silently becomes yolo, or edits loses its exact policy.
#[test]
fn mode_codec_preserves_legacy_authority_and_emits_canonical_names() {
    assert_eq!(
        serde_json::from_str::<Mode>("\"default\"").unwrap(),
        Mode::Default
    );
    assert_eq!(
        serde_json::from_str::<Mode>("\"edits\"").unwrap(),
        Mode::Edits
    );
    assert_eq!(
        serde_json::to_string(&Mode::Edits).unwrap(),
        "\"accept-edits\""
    );
    for mode in Mode::SELECTABLE {
        let encoded = serde_json::to_string(&mode).unwrap();
        assert_eq!(serde_json::from_str::<Mode>(&encoded).unwrap(), mode);
        assert_eq!(Mode::parse_selection(mode.as_str()), Some(mode));
    }
    assert!(!Mode::Default.within(Mode::Manual));
    assert!(Mode::Manual.within(Mode::Default));
    assert_eq!(Mode::Yolo.next(), Mode::Plan);
    assert_eq!(Mode::Default.next(), Mode::Manual);
}

// Catches: manual cache reuses an invocation with unknown inherited process context.
#[test]
fn uncaptured_process_context_is_once_only() {
    let engine = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Manual,
        Ruleset::default(),
    );
    let spec = SpawnSpec::new(
        vec!["echo".into()],
        Some(CanonPath::new("/workspace")),
        None,
        EnvPolicy::Inherited,
    )
    .unwrap();
    let question = request(&engine, PreparedCall::new("exec", Op::Spawn { spec }));
    assert!(!question.session_reusable());
    assert!(!SessionPermissions::default().allow(&question));
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
    let manual = GenerationPolicyCapture::new(
        CanonPath::new("/workspace"),
        Mode::Manual,
        Ruleset::default(),
        None,
    )
    .unwrap();
    let restored =
        GenerationPolicySnapshot::decode_confidential(&manual.snapshot().encode_confidential())
            .unwrap();
    assert_eq!(restored.mode(), Mode::Manual);
}

// Catches: a grant for one file or tool silently grants a sibling target or tool.
#[test]
fn manual_session_grants_do_not_cover_different_targets_or_tools() {
    let engine = PolicyEngine::new(
        CanonPath::new("/workspace"),
        Mode::Manual,
        Ruleset::default(),
    );
    let original = request(&engine, read("/workspace/a"));
    let mut session = SessionPermissions::default();
    assert!(session.allow(&original));
    assert!(!session.allows(&request(&engine, read("/workspace/b"))));
    assert!(!session.allows(&request(
        &engine,
        PreparedCall::new(
            "other-reader",
            Op::Read {
                path: CanonPath::new("/workspace/a"),
            }
        )
    )));
}
